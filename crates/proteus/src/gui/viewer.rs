//! Image viewer: Ctrl+click an image (or run `image.view`) to see it over
//! the whole window. Zoom with the wheel or `+`/`-`, pan by dragging or with
//! the arrow keys; `c` copies it (PNG), `s` saves it, Esc closes.
//!
//! The viewer keeps its own copy of the pixels (current frame), so it stays
//! valid whatever the program does to the image meanwhile.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use proteus_core::input::{Key, Mods};

use super::App;
use super::images;
use super::rows::Rows;
use crate::layout::PaneId;

/// Texture keys of viewer copies (the core's keys count up from 1).
static NEXT_KEY: AtomicU64 = AtomicU64::new(u64::MAX / 2);

const ZOOM_STEP: f32 = 1.25;
const MIN_ZOOM: f32 = 0.02;
const MAX_ZOOM: f32 = 64.0;
/// Pan step of the arrow keys, in screen pixels.
const PAN: f32 = 64.0;

pub struct Viewer {
    pub key: u64,
    pub width: u32,
    pub height: u32,
    pub pixels: Arc<Vec<u8>>,
    /// Screen pixels per image pixel; `None`: fit the window (at most 1:1).
    zoom: Option<f32>,
    /// Image point shown at the center of the view area.
    center: (f32, f32),
    /// Result of the last copy or save, shown in the bar.
    pub status: Option<String>,
}

/// What the renderer draws for the viewer.
pub struct ViewerFrame<'a> {
    pub key: u64,
    pub size: (u32, u32),
    pub pixels: &'a [u8],
    /// Destination (x, y, w, h) in physical pixels.
    pub rect: [f32; 4],
    /// Sample without smoothing (zoomed in).
    pub sharp: bool,
}

impl Viewer {
    pub fn new(width: u32, height: u32, pixels: Vec<u8>) -> Viewer {
        Viewer {
            key: NEXT_KEY.fetch_add(1, Ordering::Relaxed),
            width,
            height,
            pixels: Arc::new(pixels),
            zoom: None,
            center: (width as f32 / 2.0, height as f32 / 2.0),
            status: None,
        }
    }

    fn fit(&self, area: (f32, f32)) -> f32 {
        (area.0 / self.width as f32).min(area.1 / self.height as f32).min(1.0)
    }

    pub fn scale(&self, area: (f32, f32)) -> f32 {
        self.zoom.unwrap_or_else(|| self.fit(area))
    }

    /// Keep the image on screen: centered on an axis where it fits, else
    /// no empty space at its edges.
    fn clamp(&mut self, area: (f32, f32)) {
        let s = self.scale(area);
        let axis = |c: f32, len: f32, avail: f32| {
            let half = avail / (2.0 * s);
            if len * s <= avail { len / 2.0 } else { c.clamp(half, len - half) }
        };
        self.center = (axis(self.center.0, self.width as f32, area.0), axis(self.center.1, self.height as f32, area.1));
    }

    /// Destination rectangle in an area of `area` pixels at (0, 0).
    pub fn rect(&self, area: (f32, f32)) -> [f32; 4] {
        let s = self.scale(area);
        let (w, h) = (self.width as f32 * s, self.height as f32 * s);
        let x = area.0 / 2.0 - self.center.0 * s;
        let y = area.1 / 2.0 - self.center.1 * s;
        [x.round(), y.round(), w.round().max(1.0), h.round().max(1.0)]
    }

    /// Zoom by `factor`, keeping the image point under `at` (area pixels) in place.
    pub fn zoom_by(&mut self, factor: f32, at: (f32, f32), area: (f32, f32)) {
        let old = self.scale(area);
        let new = (old * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let (dx, dy) = (at.0 - area.0 / 2.0, at.1 - area.1 / 2.0);
        let p = (self.center.0 + dx / old, self.center.1 + dy / old);
        self.zoom = Some(new);
        self.center = (p.0 - dx / new, p.1 - dy / new);
        self.clamp(area);
    }

    /// Move the view by `d` screen pixels (the image moves the other way).
    pub fn pan(&mut self, d: (f32, f32), area: (f32, f32)) {
        let s = self.scale(area);
        self.center = (self.center.0 + d.0 / s, self.center.1 + d.1 / s);
        self.clamp(area);
    }

    pub fn reset(&mut self, zoom: Option<f32>, area: (f32, f32)) {
        self.zoom = zoom;
        self.clamp(area);
    }

    pub fn frame(&self, area: (f32, f32)) -> ViewerFrame<'_> {
        ViewerFrame {
            key: self.key,
            size: (self.width, self.height),
            pixels: &self.pixels,
            rect: self.rect(area),
            sharp: self.scale(area) >= 2.0,
        }
    }

    /// Bar text: size and zoom, plus the last status.
    pub fn title(&self, area: (f32, f32), catalog: &crate::i18n::Catalog) -> String {
        let pct = (self.scale(area) * 100.0).round();
        let mut s = catalog.t(
            "ui-image-width-height-percent",
            &[("width", &self.width.to_string()), ("height", &self.height.to_string()), ("percent", &pct.to_string())],
        );
        if let Some(st) = &self.status {
            s.push_str("  ");
            s.push_str(st);
        }
        s
    }

    fn png(&self) -> Result<Vec<u8>, String> {
        let mut out = std::io::Cursor::new(Vec::new());
        image::write_buffer_with_format(
            &mut out,
            &self.pixels,
            self.width,
            self.height,
            image::ColorType::Rgba8,
            image::ImageFormat::Png,
        )
        .map_err(|e| e.to_string())?;
        Ok(out.into_inner())
    }
}

/// `$XDG_PICTURES_DIR` (via `xdg-user-dir`), else `~/Pictures`, else `~`.
fn pictures_dir() -> Option<PathBuf> {
    let from_xdg = std::process::Command::new("xdg-user-dir")
        .arg("PICTURES")
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
        .filter(|p| p.is_absolute());
    let home = std::env::var_os("HOME").map(PathBuf::from);
    from_xdg.or_else(|| home.as_ref().map(|h| h.join("Pictures")).filter(|p| p.is_dir())).or(home)
}

/// A file name in `dir` that does not exist yet.
fn free_name(dir: &Path, stamp: u64) -> PathBuf {
    let mut p = dir.join(format!("proteus-{stamp}.png"));
    let mut n = 1;
    while p.exists() {
        n += 1;
        p = dir.join(format!("proteus-{stamp}-{n}.png"));
    }
    p
}

impl App {
    /// The view area of the viewer: the window minus the bottom bar.
    pub(crate) fn viewer_area(&self) -> (f32, f32) {
        let (pw, ph) = self.physical();
        let bar = self.cell().1 as f32 + 1.5 * self.pad_y() as f32;
        (pw as f32, (ph as f32 - bar).max(1.0))
    }

    /// Image of pane `id` drawn at a window position (logical), topmost
    /// first: its current frame's pixels.
    fn image_at(&self, id: PaneId, pos: (f64, f64)) -> Option<(u32, u32, Vec<u8>)> {
        let r = self.view().rect(id)?;
        let t = &self.panes.get(&id)?.term;
        let skip = self.row_skip(id);
        let (cw, ch) = self.cell();
        let rows = Rows::of(t);
        let views = images::place(
            t.visible_images(),
            &rows,
            t.grid().viewport_line(0),
            t.cols(),
            (cw, ch),
            t.cell_pixel_size(),
            skip,
        );
        let (x, y) = self.phys(pos);
        let (lx, ly) = ((x - r.x) as i64, (y - r.y) as i64 + (skip as u32 * ch) as i64);
        views
            .iter()
            .rev()
            .filter(|v| v.pixels.is_some())
            .find(|v| {
                let d = v.dest;
                lx >= d.x && lx < d.x + d.w && ly >= d.y && ly < d.y + d.h
            })
            .map(|v| (v.width, v.height, v.pixels.unwrap_or_default().to_vec()))
    }

    /// Ctrl+click: open the image under the pointer. Returns whether one was there.
    pub(crate) fn view_image_at(&mut self, id: PaneId, pos: (f64, f64)) -> bool {
        let Some((w, h, px)) = self.image_at(id, pos) else { return false };
        self.open_viewer(Viewer::new(w, h, px));
        true
    }

    /// `image.view`: the lowest (most recent) image on the focused pane's screen.
    pub(crate) fn view_last_image(&mut self) {
        let id = self.focused_id();
        let Some(t) = self.panes.get(&id).map(|p| &p.term) else { return };
        let found = t
            .visible_images()
            .into_iter()
            .filter(|v| v.pixels.is_some())
            .max_by_key(|v| (v.dest.y + v.dest.h, v.dest.x))
            .map(|v| (v.width, v.height, v.pixels.unwrap_or_default().to_vec()));
        match found {
            Some((w, h, px)) => self.open_viewer(Viewer::new(w, h, px)),
            None => self.notice(self.cfg.i18n.t("ui-no-image-on-screen", &[])),
        }
    }

    fn open_viewer(&mut self, v: Viewer) {
        self.w.mouse.drag = None;
        self.w.mouse.selecting = None;
        self.w.viewer = Some(v);
        self.w.dirty = true;
        self.update_pointer_icon();
    }

    pub(crate) fn close_viewer(&mut self) {
        self.w.viewer = None;
        self.w.mouse.drag = None;
        self.w.dirty = true;
        self.update_pointer_icon();
    }

    /// Keys while the viewer is open.
    pub(crate) fn viewer_key(&mut self, key: Key, mods: Mods) {
        let area = self.viewer_area();
        let center = (area.0 / 2.0, area.1 / 2.0);
        let Some(v) = self.w.viewer.as_mut() else { return };
        let step = if mods.contains(Mods::SHIFT) { PAN * 4.0 } else { PAN };
        match key {
            Key::Escape | Key::Char('q') => return self.close_viewer(),
            Key::Char('+' | '=') => v.zoom_by(ZOOM_STEP, center, area),
            Key::Char('-') => v.zoom_by(1.0 / ZOOM_STEP, center, area),
            Key::Char('0') => v.reset(None, area),
            Key::Char('1') => v.reset(Some(1.0), area),
            Key::Left | Key::Char('h') => v.pan((-step, 0.0), area),
            Key::Right | Key::Char('l') => v.pan((step, 0.0), area),
            Key::Up | Key::Char('k') => v.pan((0.0, -step), area),
            Key::Down | Key::Char('j') => v.pan((0.0, step), area),
            Key::Char('c') => self.viewer_copy(),
            Key::Char('s') => self.viewer_save(),
            _ => return,
        }
        self.w.dirty = true;
    }

    /// Wheel over the viewer: zoom around the pointer.
    pub(crate) fn viewer_wheel(&mut self, notches: f64, pos: (f64, f64)) {
        let area = self.viewer_area();
        let at = self.phys(pos);
        if let Some(v) = self.w.viewer.as_mut() {
            v.zoom_by(ZOOM_STEP.powf(-notches as f32), (at.0 as f32, at.1 as f32), area);
            self.w.dirty = true;
        }
    }

    /// Dragged from `from` to `to` (logical): the image follows the pointer.
    pub(crate) fn viewer_drag(&mut self, from: (f64, f64), to: (f64, f64)) {
        let area = self.viewer_area();
        let (a, b) = (self.phys(from), self.phys(to));
        if let Some(v) = self.w.viewer.as_mut() {
            v.pan(((a.0 - b.0) as f32, (a.1 - b.1) as f32), area);
            self.w.dirty = true;
        }
    }

    fn viewer_copy(&mut self) {
        let Some(v) = self.w.viewer.as_ref() else { return };
        let status = match v.png() {
            Ok(png) => {
                self.set_clipboard_image(png);
                self.cfg.i18n.t("ui-copied", &[])
            }
            Err(e) => self.cfg.i18n.t("ui-copy-failed-error", &[("error", &e.to_string())]),
        };
        if let Some(v) = self.w.viewer.as_mut() {
            v.status = Some(status);
        }
    }

    fn viewer_save(&mut self) {
        let Some(v) = self.w.viewer.as_ref() else { return };
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let status = match pictures_dir() {
            None => self.cfg.i18n.t("ui-save-failed-no-home-directory", &[]),
            Some(dir) => {
                let path = free_name(&dir, stamp);
                let r = std::fs::create_dir_all(&dir)
                    .map_err(|e| e.to_string())
                    .and_then(|()| v.png())
                    .and_then(|png| std::fs::write(&path, png).map_err(|e| e.to_string()));
                match r {
                    Ok(()) => self.cfg.i18n.t("ui-saved-path", &[("path", &path.display().to_string())]),
                    Err(e) => self.cfg.i18n.t("ui-save-failed-error", &[("error", &e.to_string())]),
                }
            }
        };
        if let Some(v) = self.w.viewer.as_mut() {
            v.status = Some(status);
        }
    }

    /// Offer a PNG on the clipboard (replaces any text there).
    fn set_clipboard_image(&mut self, png: Vec<u8>) {
        let (Some(mgr), Some(dev)) = (&self.data_manager, &self.data_device) else { return };
        let src = mgr.create_copy_paste_source(&self.qh, ["image/png"]);
        src.set_selection(dev, self.serial);
        self.clipboard = None;
        self.clipboard_image = Some((src, Arc::new(png)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: (f32, f32) = (800.0, 600.0);

    #[test]
    fn fits_without_enlarging() {
        let v = Viewer::new(1600, 600, vec![0; 1600 * 600 * 4]);
        assert_eq!(v.scale(AREA), 0.5);
        assert_eq!(v.rect(AREA), [0.0, 150.0, 800.0, 300.0]);
        let small = Viewer::new(100, 50, vec![0; 100 * 50 * 4]);
        assert_eq!(small.scale(AREA), 1.0);
        assert_eq!(small.rect(AREA), [350.0, 275.0, 100.0, 50.0]);
    }

    #[test]
    fn zoom_keeps_the_point_under_the_pointer() {
        let mut v = Viewer::new(800, 600, vec![0; 800 * 600 * 4]);
        // Image pixel (200, 150) is at screen (200, 150).
        v.zoom_by(2.0, (200.0, 150.0), AREA);
        let r = v.rect(AREA);
        assert_eq!((r[0] + 200.0 * 2.0, r[1] + 150.0 * 2.0), (200.0, 150.0));
        // Zooming out past the fit re-centers.
        v.zoom_by(0.25, (0.0, 0.0), AREA);
        assert_eq!(v.rect(AREA), [200.0, 150.0, 400.0, 300.0]);
    }

    #[test]
    fn pan_stops_at_the_edges() {
        let mut v = Viewer::new(800, 600, vec![0; 800 * 600 * 4]);
        v.reset(Some(2.0), AREA);
        v.pan((-10_000.0, 0.0), AREA);
        assert_eq!(v.rect(AREA)[0], 0.0);
        v.pan((10_000.0, 10_000.0), AREA);
        let r = v.rect(AREA);
        assert_eq!((r[0] + r[2], r[1] + r[3]), AREA);
    }

    #[test]
    fn save_names() {
        let dir = std::env::temp_dir().join(format!("proteus-viewer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = free_name(&dir, 7);
        assert_eq!(a.file_name().unwrap(), "proteus-7.png");
        std::fs::write(&a, b"x").unwrap();
        assert_eq!(free_name(&dir, 7).file_name().unwrap(), "proteus-7-2.png");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
