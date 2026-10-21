//! Graphics protocol front ends: kitty graphics (`APC G`), Sixel (`DCS q`)
//! and iTerm2 inline images (`OSC 1337 File=`), plus the APC pre-filter that
//! `vte` lacks.

use std::time::{Duration, Instant};

use super::{BASE64, Terminal, join_params};
use crate::event::Terminator;
use crate::graphics::kitty::{self, Command, Error, err};
use crate::graphics::media::{self, Medium};
use crate::graphics::sixel::SixelDecoder;
use crate::graphics::{
    AnimationState, DEFAULT_GAP, FALLBACK_CELL_PX, Fit, Frame, Image, ImageStore, ImageView, MAX_RELATIVE_DEPTH,
    Placement, PxRect, Source, Upload, decode_image,
};
use crate::modes::TermMode;
use base64::Engine as _;

/// State of the APC pre-filter (vte discards APC strings).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Apc {
    #[default]
    Ground,
    /// Saw ESC outside an APC string.
    Esc,
    Body,
    /// Saw ESC inside an APC string.
    BodyEsc,
}

/// Largest GIF frame delay treated as "as fast as possible" (browser convention).
const MIN_GIF_DELAY_MS: u32 = 10;

fn ceil_div(a: u64, b: u64) -> u32 {
    a.div_ceil(b.max(1)).min(u32::MAX as u64) as u32
}

/// Width/height argument of iTerm2's `File=`.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Dim {
    Auto,
    Cells(u32),
    Px(u32),
    Percent(u32),
}

fn parse_dim(v: &str) -> Dim {
    if v == "auto" {
        Dim::Auto
    } else if let Some(n) = v.strip_suffix("px") {
        n.parse().map_or(Dim::Auto, Dim::Px)
    } else if let Some(n) = v.strip_suffix('%') {
        n.parse().map_or(Dim::Auto, |p: u32| Dim::Percent(p.min(100)))
    } else {
        v.parse().map_or(Dim::Auto, Dim::Cells)
    }
}

impl Terminal {
    // --- byte stream -------------------------------------------------------

    /// Feed bytes to `vte`, cutting out APC strings (kitty graphics) on the
    /// way. `ESC _` and the terminating `ESC \` still reach `vte` so its state
    /// machine stays in sync.
    pub(super) fn feed(&mut self, parser: &mut vte::Parser, bytes: &[u8]) {
        let mut start = 0;
        let mut i = 0;
        while i < bytes.len() {
            match self.apc {
                Apc::Ground => match bytes[i..].iter().position(|&b| b == 0x1b) {
                    Some(off) => {
                        i += off;
                        self.apc = Apc::Esc;
                    }
                    None => break,
                },
                Apc::Esc => {
                    let b = bytes[i];
                    if b == b'_' {
                        parser.advance(self, &bytes[start..=i]);
                        start = i + 1;
                        self.apc = Apc::Body;
                        self.apc_buf.clear();
                        self.apc_overflow = false;
                    } else if b != 0x1b {
                        self.apc = Apc::Ground;
                    }
                }
                Apc::Body => {
                    let end = bytes[i..].iter().position(|&b| b == 0x1b).map_or(bytes.len(), |o| i + o);
                    self.apc_push(&bytes[i..end]);
                    if end == bytes.len() {
                        start = end;
                        break;
                    }
                    i = end;
                    start = end + 1;
                    self.apc = Apc::BodyEsc;
                }
                Apc::BodyEsc => {
                    let data = std::mem::take(&mut self.apc_buf);
                    if !std::mem::take(&mut self.apc_overflow) {
                        self.apc_dispatch(&data);
                    }
                    self.apc_buf = data;
                    self.apc_buf.clear();
                    if bytes[i] == b'\\' {
                        parser.advance(self, b"\x1b\\");
                        start = i + 1;
                        self.apc = Apc::Ground;
                    } else {
                        // ESC aborted the string; it starts a new sequence.
                        parser.advance(self, b"\x1b");
                        start = i;
                        self.apc = Apc::Esc;
                        continue;
                    }
                }
            }
            i += 1;
        }
        if start < bytes.len() && matches!(self.apc, Apc::Ground | Apc::Esc) {
            parser.advance(self, &bytes[start..]);
        }
    }

    fn apc_push(&mut self, chunk: &[u8]) {
        if self.apc_overflow {
            return;
        }
        let max = self.config.graphics.max_image_bytes / 3 * 4 + 8192;
        if self.apc_buf.len() + chunk.len() > max {
            self.apc_overflow = true;
            self.apc_buf = Vec::new();
        } else {
            self.apc_buf.extend_from_slice(chunk);
        }
    }

    fn apc_dispatch(&mut self, data: &[u8]) {
        if let Some(rest) = data.strip_prefix(b"G")
            && self.config.graphics.enabled
            && let Some(cmd) = Command::parse(rest)
        {
            self.kitty(cmd);
        }
    }

    // --- shared helpers ------------------------------------------------------

    /// Cell size used for image geometry.
    pub fn cell_pixel_size(&self) -> (u32, u32) {
        if self.cell_px.0 == 0 || self.cell_px.1 == 0 { FALLBACK_CELL_PX } else { self.cell_px }
    }

    /// Images and placements of the active screen.
    pub fn images(&self) -> &ImageStore {
        &self.images
    }

    /// Everything the renderer must draw over/under the viewport, bottom-most
    /// first. Destinations are in viewport pixels for [`Self::cell_pixel_size`].
    pub fn visible_images(&self) -> Vec<ImageView<'_>> {
        self.images.views(&self.grid, &self.styles, self.cell_pixel_size())
    }

    /// File-based kitty media are only allowed for panes on this machine.
    fn files_allowed(&self) -> bool {
        let g = &self.config.graphics;
        g.allow_local_files
            && self.cwd.as_ref().is_none_or(|c| match &g.hostname {
                Some(h) => c.is_local(h),
                None => matches!(c.host.as_deref(), None | Some("") | Some("localhost")),
            })
    }

    fn abs_cursor_line(&self) -> i64 {
        self.grid.abs_line(self.cursor.y) as i64
    }

    /// Kitty: cursor goes to the last row of the image, just after it.
    fn cursor_after_image(&mut self, cols: u32, rows: u32) {
        for _ in 1..rows.min(self.lines() as u32 * 4) {
            self.linefeed();
        }
        self.cursor.x = (self.cursor.x + cols as usize).min(self.cols() - 1);
        self.cursor.pending_wrap = false;
    }

    // --- kitty graphics --------------------------------------------------------

    fn kitty(&mut self, cmd: Command) {
        if let Some(up) = &mut self.images.upload {
            let continues = !cmd.has(b'a') || (cmd.action() == b'f' && up.cmd.action() == b'f');
            if continues {
                let max = self.config.graphics.max_image_bytes / 3 * 4 + 8192;
                if up.data.len() + cmd.payload.len() > max {
                    let up = self.images.upload.take().expect("checked");
                    let (id, n) = (up.cmd.id(), up.cmd.number());
                    self.kitty_reply(&up.cmd, id, n, 0, Err(err("EFBIG", "Image too large")));
                    return;
                }
                up.data.extend_from_slice(&cmd.payload);
                if cmd.has(b'q') {
                    up.cmd.set(b'q', cmd.int(b'q'));
                }
                if cmd.more() {
                    return;
                }
                let up = self.images.upload.take().expect("checked");
                let mut first = up.cmd;
                first.payload = up.data;
                first.set(b'm', 0);
                self.kitty_run(first);
                return;
            }
            // Anything else aborts the partial upload.
            self.images.upload = None;
        }
        if cmd.more() && cmd.ch(b't', b'd') == b'd' && matches!(cmd.action(), b't' | b'T' | b'q' | b'f') {
            let data = cmd.payload.clone();
            self.images.upload = Some(Upload { cmd, data });
            return;
        }
        self.kitty_run(cmd);
    }

    fn kitty_run(&mut self, cmd: Command) {
        let (id, number) = (cmd.id(), cmd.number());
        if id != 0 && number != 0 {
            self.kitty_reply(&cmd, id, number, 0, Err(err("EINVAL", "Must not specify both i and I")));
            return;
        }
        match cmd.action() {
            b't' | b'T' | b'q' => self.kitty_transmit(cmd),
            b'f' => {
                let r = self.kitty_frame(&cmd);
                self.kitty_reply(&cmd, id, number, 0, r);
            }
            b'p' => {
                let r = self.kitty_put(&cmd, None);
                let id = if id == 0 {
                    self.kitty_lookup(&cmd).ok().and_then(|k| self.images.image(k)).map_or(0, |i| i.id)
                } else {
                    id
                };
                self.kitty_reply(&cmd, id, number, cmd.placement(), r);
            }
            b'd' => self.kitty_delete(&cmd),
            b'a' => {
                if let Err(e) = self.kitty_animate(&cmd) {
                    self.kitty_reply(&cmd, id, number, 0, Err(e));
                }
            }
            b'c' => {
                let r = self.kitty_compose(&cmd);
                if r.is_err() {
                    self.kitty_reply(&cmd, id, number, 0, r);
                }
            }
            _ => self.kitty_reply(&cmd, id, number, 0, Err(err("EINVAL", "Unknown action"))),
        }
    }

    fn kitty_reply(&mut self, cmd: &Command, id: u32, number: u32, pid: u32, r: Result<(), Error>) {
        if id == 0 && number == 0 {
            return;
        }
        let q = cmd.quiet();
        let msg = match &r {
            Ok(()) if q >= 1 => return,
            Ok(()) => "OK",
            Err(_) if q >= 2 => return,
            Err(e) => e.as_str(),
        };
        let mut s = format!("\x1b_Gi={id}");
        if number != 0 {
            s.push_str(&format!(",I={number}"));
        }
        if pid != 0 {
            s.push_str(&format!(",p={pid}"));
        }
        s.push(';');
        s.push_str(msg);
        s.push_str("\x1b\\");
        self.reply(&s);
    }

    fn kitty_lookup(&self, cmd: &Command) -> Result<u64, Error> {
        let key = if cmd.id() != 0 {
            self.images.key_for_id(cmd.id())
        } else if cmd.number() != 0 {
            self.images.newest_with_number(cmd.number())
        } else {
            return Err(err("EINVAL", "No image id or number specified"));
        };
        key.ok_or_else(|| err("ENOENT", "Image not found"))
    }

    /// Payload bytes after medium and compression handling.
    fn kitty_data(&self, cmd: &Command) -> Result<Vec<u8>, Error> {
        let max = self.config.graphics.max_image_bytes;
        let raw = BASE64.decode(&cmd.payload).map_err(|_| err("EINVAL", "Invalid base64 data"))?;
        let medium = match cmd.ch(b't', b'd') {
            b'd' => None,
            b'f' => Some(Medium::File),
            b't' => Some(Medium::TempFile),
            b's' => Some(Medium::SharedMemory),
            _ => return Err(err("EINVAL", "Unknown transmission medium")),
        };
        let data = match medium {
            None => raw,
            Some(m) => {
                let bad = || err("EBADF", "Failed to read image file");
                if !self.files_allowed() {
                    return Err(bad());
                }
                media::read(m, &raw, cmd.u(b'S') as u64, cmd.u(b'O') as u64, max).map_err(|_| bad())?
            }
        };
        match cmd.ch(b'o', 0) {
            0 => Ok(data),
            b'z' => kitty::inflate(&data, max),
            _ => Err(err("EINVAL", "Unknown compression")),
        }
    }

    fn kitty_pixels(&self, cmd: &Command, default: (u32, u32)) -> Result<kitty::Pixels, Error> {
        let data = self.kitty_data(cmd)?;
        let format = if cmd.has(b'f') { cmd.u(b'f') } else { 32 };
        let (w, h) = (cmd.u(b's'), cmd.u(b'v'));
        let (w, h) = if w == 0 && h == 0 { default } else { (w, h) };
        kitty::to_rgba(format, data, w, h, self.config.graphics.max_image_bytes)
    }

    fn kitty_transmit(&mut self, cmd: Command) {
        let (id, number) = (cmd.id(), cmd.number());
        let px = match self.kitty_pixels(&cmd, (0, 0)) {
            Ok(p) => p,
            Err(e) => {
                self.kitty_reply(&cmd, id, number, 0, Err(e));
                return;
            }
        };
        if cmd.action() == b'q' {
            self.kitty_reply(&cmd, id, number, 0, Ok(()));
            return;
        }
        let id = if number != 0 { self.images.alloc_id() } else { id };
        // Re-transmitting an id replaces the image and drops its placements.
        if let Some(old) = self.images.key_for_id(id) {
            self.images.remove_image(old);
            self.damage.images = true;
        }
        let other = self.images_inactive.bytes();
        if let Err(e) = self.images.reserve(px.rgba.len(), other, &self.config.graphics, None) {
            self.kitty_reply(&cmd, id, number, 0, Err(e));
            return;
        }
        let key = self.images.insert(Image::new(id, number, Source::Kitty, px.width, px.height, px.rgba));
        if cmd.action() == b'T' {
            let r = self.kitty_put(&cmd, Some(key));
            if id == 0 && r.is_err() {
                self.images.remove_image(key);
            }
            self.kitty_reply(&cmd, id, number, cmd.placement(), r);
        } else {
            self.kitty_reply(&cmd, id, number, 0, Ok(()));
        }
    }

    fn kitty_put(&mut self, cmd: &Command, key: Option<u64>) -> Result<(), Error> {
        let key = match key {
            Some(k) => k,
            None => self.kitty_lookup(cmd)?,
        };
        let img = self.images.image(key).ok_or_else(|| err("ENOENT", "Image not found"))?;
        let (iw, ih) = img.size();
        let image_id = img.id;
        let (x, y) = (cmd.u(b'x').min(iw), cmd.u(b'y').min(ih));
        let w = match cmd.u(b'w') {
            0 => iw - x,
            w => w.min(iw - x),
        };
        let h = match cmd.u(b'h') {
            0 => ih - y,
            h => h.min(ih - y),
        };
        if w == 0 || h == 0 {
            return Err(err("EINVAL", "Source rectangle is empty"));
        }
        let (cw, ch) = self.cell_pixel_size();
        let (ox, oy) = (cmd.u(b'X').min(cw - 1), cmd.u(b'Y').min(ch - 1));
        let (c, r) = (cmd.u(b'c').min(1 << 14), cmd.u(b'r').min(1 << 14));
        let (w64, h64, cw64, ch64) = (w as u64, h as u64, cw as u64, ch as u64);
        let (cols, rows, fit) = match (c, r) {
            (0, 0) => (ceil_div(ox as u64 + w64, cw64), ceil_div(oy as u64 + h64, ch64), Fit::Natural),
            (c, 0) => (c, ceil_div(c as u64 * cw64 * h64, w64 * ch64), Fit::Contain),
            (0, r) => (ceil_div(r as u64 * ch64 * w64, h64 * cw64), r, Fit::Contain),
            (c, r) => (c, r, Fit::Contain),
        };
        let is_virtual = cmd.u(b'U') == 1;
        let mut p = Placement::new(key, self.abs_cursor_line(), self.cursor.x, PxRect { x, y, w, h }, cols, rows, fit);
        // Placement ids only exist for images with an id.
        p.id = if image_id == 0 { 0 } else { cmd.placement() };
        p.offset = (ox, oy);
        p.z = cmd.i(b'z');
        p.is_virtual = is_virtual;
        if cmd.u(b'P') != 0 {
            if is_virtual {
                return Err(err("EINVAL", "Virtual placements cannot be relative"));
            }
            let pk = self.images.key_for_id(cmd.u(b'P')).ok_or_else(|| err("ENOPARENT", "Parent image not found"))?;
            let parent = self
                .images
                .find_placement(pk, cmd.u(b'Q'))
                .ok_or_else(|| err("ENOPARENT", "Parent placement not found"))?;
            let pid = parent.id;
            match self.images.chain_depth(pk, pid, (key, p.id)) {
                None => return Err(err("ECYCLE", "Relative placement would create a cycle")),
                Some(d) if d >= MAX_RELATIVE_DEPTH => return Err(err("ETOODEEP", "Relative placement chain too deep")),
                Some(_) => {}
            }
            if (pk, pid) == (key, p.id) && p.id != 0 {
                return Err(err("ECYCLE", "Placement cannot be relative to itself"));
            }
            p.parent = Some((pk, pid));
            p.rel = (cmd.i(b'H'), cmd.i(b'V'));
        }
        let moves = !is_virtual && p.parent.is_none() && cmd.u(b'C') != 1;
        self.images.add_placement(p);
        self.damage.images = true;
        if moves {
            self.cursor_after_image(cols, rows);
        }
        Ok(())
    }

    fn kitty_delete(&mut self, cmd: &Command) {
        self.images.upload = None;
        let d = cmd.ch(b'd', b'a');
        let free = d.is_ascii_uppercase();
        let top = self.grid.abs_line(0) as i64;
        let bottom = top + self.lines() as i64;
        let cur = (self.abs_cursor_line(), self.cursor.x as i64);
        let x = cmd.u(b'x') as i64 - 1;
        let y = top + cmd.u(b'y') as i64 - 1;
        let z = cmd.i(b'z');
        let s = &self.images;
        // Placement sequence numbers to delete, plus images to drop outright.
        let mut doomed: Vec<u64> = Vec::new();
        let mut drop_images: Vec<u64> = Vec::new();
        let mut select = |f: &dyn Fn(&Placement) -> bool| {
            doomed.extend(s.placements().iter().filter(|p| f(p)).map(|p| p.seq));
        };
        match d.to_ascii_lowercase() {
            b'a' => select(&|p| s.intersects_lines(p, top, bottom)),
            b'i' | b'n' => {
                let key = if d.eq_ignore_ascii_case(&b'i') {
                    s.key_for_id(cmd.id())
                } else {
                    s.newest_with_number(cmd.number())
                };
                let Some(key) = key else { return };
                let pid = cmd.placement();
                select(&|p| p.image == key && (pid == 0 || p.id == pid));
                if free && pid == 0 {
                    drop_images.push(key);
                }
            }
            b'c' => select(&|p| s.intersects(p, Some(cur.0), Some(cur.1))),
            b'p' => select(&|p| s.intersects(p, Some(y), Some(x))),
            b'q' => select(&|p| p.z == z && s.intersects(p, Some(y), Some(x))),
            b'x' => select(&|p| s.intersects(p, None, Some(x))),
            b'y' => select(&|p| s.intersects(p, Some(y), None)),
            b'z' => select(&|p| !p.is_virtual && p.z == z),
            b'r' => {
                let (lo, hi) = (cmd.u(b'x'), cmd.u(b'y'));
                let keys: Vec<u64> =
                    s.images().filter(|i| i.id != 0 && i.id >= lo && i.id <= hi).map(|i| i.key).collect();
                select(&|p| keys.contains(&p.image));
                if free {
                    drop_images.extend(keys);
                }
            }
            b'f' => {
                if let Ok(key) = self.kitty_lookup(cmd) {
                    self.delete_frames(key);
                }
                return;
            }
            _ => return,
        }
        let affected = self.images.remove_placements(|p| doomed.contains(&p.seq));
        for k in drop_images {
            self.images.remove_image(k);
        }
        if free {
            self.images.free_unplaced(&affected);
        }
        self.images.free_anonymous();
        self.damage.images = true;
    }

    fn delete_frames(&mut self, key: u64) {
        let Some(img) = self.images.image_mut(key) else { return };
        if img.frames.len() < 2 {
            return;
        }
        let freed = (img.frames.len() - 1) * img.frame_bytes();
        let freed = if img.evicted { 0 } else { freed };
        img.frames.truncate(1);
        img.current = 0;
        img.state = AnimationState::Stopped;
        img.generation += 1;
        self.images.release(freed);
        self.damage.images = true;
    }

    fn kitty_frame(&mut self, cmd: &Command) -> Result<(), Error> {
        let key = self.kitty_lookup(cmd)?;
        let img = self.images.image(key).ok_or_else(|| err("ENOENT", "Image not found"))?;
        if img.evicted {
            return Err(err("ENOSPC", "Image data was evicted"));
        }
        let (iw, ih) = img.size();
        let px = self.kitty_pixels(cmd, (iw, ih))?;
        let (x, y) = (cmd.u(b'x'), cmd.u(b'y'));
        let rect = PxRect { x: 0, y: 0, w: px.width.min(iw.saturating_sub(x)), h: px.height.min(ih.saturating_sub(y)) };
        let replace = cmd.u(b'X') == 1;
        let gap = cmd.i(b'z');
        let edit = cmd.u(b'r') as usize;
        if edit > 0 {
            let img = self.images.image_mut(key).expect("checked");
            let current = img.current;
            let f = img.frames.get_mut(edit - 1).ok_or_else(|| err("ENOENT", "Frame not found"))?;
            kitty::compose(&mut f.pixels, iw, &px.rgba, px.width, rect, x, y, replace);
            if gap != 0 {
                f.gap = gap;
            }
            if edit - 1 == current {
                img.generation += 1;
            }
        } else {
            let base = cmd.u(b'c') as usize;
            let mut canvas = if base > 0 {
                img.frames.get(base - 1).ok_or_else(|| err("ENOENT", "Base frame not found"))?.pixels.clone()
            } else {
                cmd.u(b'Y').to_be_bytes().repeat(iw as usize * ih as usize)
            };
            kitty::compose(&mut canvas, iw, &px.rgba, px.width, rect, x, y, replace);
            let other = self.images_inactive.bytes();
            self.images.reserve(canvas.len(), other, &self.config.graphics, Some(key))?;
            let img = self.images.image_mut(key).expect("protected from eviction");
            img.frames.push(Frame { pixels: canvas, gap: if gap != 0 { gap } else { DEFAULT_GAP } });
        }
        self.damage.images = true;
        Ok(())
    }

    fn kitty_animate(&mut self, cmd: &Command) -> Result<(), Error> {
        let key = self.kitty_lookup(cmd)?;
        let img = self.images.image_mut(key).ok_or_else(|| err("ENOENT", "Image not found"))?;
        let now = Instant::now();
        if cmd.has(b's') {
            match cmd.u(b's') {
                1 => {
                    img.state = AnimationState::Stopped;
                    img.loops_done = 0;
                }
                s @ (2 | 3) => {
                    if img.state == AnimationState::Stopped {
                        img.shown_at = Some(now);
                    }
                    img.state = if s == 2 { AnimationState::Loading } else { AnimationState::Running };
                }
                _ => {}
            }
        }
        let r = cmd.u(b'r') as usize;
        if r > 0 && cmd.i(b'z') != 0 {
            let f = img.frames.get_mut(r - 1).ok_or_else(|| err("ENOENT", "Frame not found"))?;
            f.gap = cmd.i(b'z');
        }
        let c = cmd.u(b'c') as usize;
        if c > 0 {
            if c > img.frames.len() {
                return Err(err("ENOENT", "Frame not found"));
            }
            img.current = c - 1;
            img.shown_at = Some(now);
            img.generation += 1;
        }
        match cmd.u(b'v') {
            0 => {}
            1 => img.loops = None,
            n => img.loops = Some(n - 1),
        }
        self.damage.images = true;
        Ok(())
    }

    fn kitty_compose(&mut self, cmd: &Command) -> Result<(), Error> {
        let key = self.kitty_lookup(cmd)?;
        let img = self.images.image_mut(key).ok_or_else(|| err("ENOENT", "Image not found"))?;
        let (iw, ih) = img.size();
        let (src, dst) = (cmd.u(b'r') as usize, cmd.u(b'c') as usize);
        let n = img.frames.len();
        if src == 0 || dst == 0 || src > n || dst > n || img.evicted {
            return Err(err("ENOENT", "Frame not found"));
        }
        let w = if cmd.u(b'w') == 0 { iw } else { cmd.u(b'w') };
        let h = if cmd.u(b'h') == 0 { ih } else { cmd.u(b'h') };
        let (sx, sy, dx, dy) = (cmd.u(b'X'), cmd.u(b'Y'), cmd.u(b'x'), cmd.u(b'y'));
        let fits = |x: u32, y: u32| x as u64 + w as u64 <= iw as u64 && y as u64 + h as u64 <= ih as u64;
        if !fits(sx, sy) || !fits(dx, dy) {
            return Err(err("EINVAL", "Rectangle out of bounds"));
        }
        let overlap = sx < dx + w && dx < sx + w && sy < dy + h && dy < sy + h;
        if src == dst && overlap {
            return Err(err("EINVAL", "Source and destination overlap"));
        }
        let source = img.frames[src - 1].pixels.clone();
        let rect = PxRect { x: sx, y: sy, w, h };
        kitty::compose(&mut img.frames[dst - 1].pixels, iw, &source, iw, rect, dx, dy, cmd.u(b'C') == 1);
        if dst - 1 == img.current {
            img.generation += 1;
            self.damage.images = true;
        }
        Ok(())
    }

    /// Advance running animations of the active screen. Call when the pane is
    /// visible (e.g. on frame callbacks); returns when the next frame is due.
    pub fn tick_images(&mut self, now: Instant) -> Option<Instant> {
        let mut next: Option<Instant> = None;
        let mut changed = false;
        for img in self.images.images_mut() {
            if img.state == AnimationState::Stopped || img.frames.len() < 2 || img.evicted {
                continue;
            }
            let mut shown = *img.shown_at.get_or_insert(now);
            for _ in 0..img.frames.len() * 2 {
                let gap = img.frames[img.current].gap.max(0) as u64;
                let due = shown + Duration::from_millis(gap);
                if due > now {
                    next = Some(next.map_or(due, |n| n.min(due)));
                    break;
                }
                if img.current + 1 < img.frames.len() {
                    img.current += 1;
                } else if img.state == AnimationState::Loading {
                    break;
                } else if img.loops.is_some_and(|m| img.loops_done >= m) {
                    img.state = AnimationState::Stopped;
                    img.loops_done = 0;
                    break;
                } else {
                    img.loops_done = img.loops_done.saturating_add(1);
                    img.current = 0;
                }
                // Don't try to catch up after a long pause (pane hidden).
                shown = if now.duration_since(due) > Duration::from_secs(1) { now } else { due };
                img.generation += 1;
                changed = true;
            }
            img.shown_at = Some(shown);
        }
        if changed {
            self.damage.images = true;
        }
        next
    }

    // --- Sixel and iTerm2 ------------------------------------------------------

    pub(super) fn new_sixel(&self, p2: u16) -> Option<Box<SixelDecoder>> {
        let g = &self.config.graphics;
        g.enabled.then(|| Box::new(SixelDecoder::new(p2, g.max_image_bytes)))
    }

    pub(super) fn sixel_finish(&mut self, dec: SixelDecoder) {
        if let Some(s) = dec.finish() {
            self.place_anonymous(Source::Sixel, s.width, s.height, vec![(s.rgba, 0)], None);
        }
    }

    /// Store and place a Sixel or iTerm2 image at the cursor (z = -1, below text).
    fn place_anonymous(
        &mut self,
        source: Source,
        w: u32,
        h: u32,
        frames: Vec<(Vec<u8>, u32)>,
        size: Option<(u32, u32)>,
    ) {
        let (cw, ch) = self.cell_pixel_size();
        let (dw, dh) = size.unwrap_or((w, h));
        let cols = ceil_div(dw as u64, cw as u64).max(1);
        let rows = ceil_div(dh as u64, ch as u64).max(1);
        let fixed = source == Source::Sixel && self.mode.contains(TermMode::SIXEL_DISPLAY);
        let (line, col) =
            if fixed { (self.grid.abs_line(0) as i64, 0) } else { (self.abs_cursor_line(), self.cursor.x) };
        // Replace earlier images drawn at the same spot (before inserting the new one).
        self.images.remove_covered(line, col, cols, rows);

        let need = frames.iter().map(|f| f.0.len()).sum();
        let other = self.images_inactive.bytes();
        if self.images.reserve(need, other, &self.config.graphics, None).is_err() {
            return;
        }
        let animated = frames.len() > 1;
        let mut frames = frames.into_iter();
        let (first, first_delay) = frames.next().expect("at least one frame");
        let mut img = Image::new(0, 0, source, w, h, first);
        let gif_gap = |d: u32| if d <= MIN_GIF_DELAY_MS { 100 } else { d.min(i32::MAX as u32) as i32 };
        if animated {
            img.frames[0].gap = gif_gap(first_delay);
            img.frames.extend(frames.map(|(pixels, d)| Frame { pixels, gap: gif_gap(d) }));
            img.state = AnimationState::Running;
        }
        let key = self.images.insert(img);
        let fit = if size.is_some() { Fit::Pixels(dw, dh) } else { Fit::Natural };
        let mut p = Placement::new(key, line, col, PxRect { x: 0, y: 0, w, h }, cols, rows, fit);
        p.z = -1;
        self.images.add_placement(p);
        self.damage.images = true;

        match source {
            _ if fixed => {}
            Source::Sixel if self.mode.contains(TermMode::SIXEL_CURSOR_RIGHT) => self.cursor_after_image(cols, rows),
            // Sixel scrolling: text continues on the line below the image.
            Source::Sixel => {
                for _ in 0..rows.min(self.lines() as u32 * 4) {
                    self.linefeed();
                }
            }
            _ => self.cursor_after_image(cols, rows),
        }
    }

    /// OSC 1337 (iTerm2): `File=`, multipart files and `ReportCellSize`.
    pub(super) fn osc_iterm(&mut self, params: &[&[u8]], term: Terminator) {
        let body = join_params(params, 1);
        let max = self.config.graphics.max_image_bytes / 3 * 4 + 8192;
        if let Some(rest) = body.strip_prefix("File=") {
            let (args, data) = rest.split_once(':').unwrap_or((rest, ""));
            self.iterm_file(args, data.as_bytes());
        } else if let Some(args) = body.strip_prefix("MultipartFile=") {
            self.iterm_multipart = Some((args.to_string(), Vec::new()));
        } else if let Some(part) = body.strip_prefix("FilePart=") {
            if let Some((_, buf)) = &mut self.iterm_multipart {
                if buf.len() + part.len() > max {
                    self.iterm_multipart = None;
                } else {
                    buf.extend_from_slice(part.as_bytes());
                }
            }
        } else if body == "FileEnd" {
            if let Some((args, data)) = self.iterm_multipart.take() {
                self.iterm_file(&args, &data);
            }
        } else if body == "ReportCellSize" {
            let (cw, ch) = self.cell_pixel_size();
            self.reply(&format!("\x1b]1337;ReportCellSize={ch}.0;{cw}.0;1.0{}", term.as_str()));
        }
    }

    fn iterm_file(&mut self, args: &str, data: &[u8]) {
        if !self.config.graphics.enabled {
            return;
        }
        let max = self.config.graphics.max_image_bytes;
        let (mut inline, mut preserve) = (false, true);
        let (mut dw, mut dh) = (Dim::Auto, Dim::Auto);
        for kv in args.split(';') {
            let Some((k, v)) = kv.split_once('=') else { continue };
            match k {
                "inline" => inline = v == "1",
                "width" => dw = parse_dim(v),
                "height" => dh = parse_dim(v),
                "preserveAspectRatio" => preserve = v != "0",
                "size" if v.parse::<usize>().is_ok_and(|n| n > max) => return,
                _ => {}
            }
        }
        // Downloads (inline=0) are not supported.
        if !inline || data.len() > max / 3 * 4 + 8192 {
            return;
        }
        let Ok(bytes) = BASE64.decode(data) else { return };
        let Ok(img) = decode_image(&bytes, max) else { return };
        let (cw, ch) = self.cell_pixel_size();
        let (tw, th) = (self.cols() as u64 * cw as u64, self.lines() as u64 * ch as u64);
        let px = |d: Dim, cell: u32, total: u64| -> Option<u64> {
            match d {
                Dim::Auto => None,
                Dim::Cells(n) => Some(n as u64 * cell as u64),
                Dim::Px(n) => Some(n as u64),
                Dim::Percent(p) => Some(total * p as u64 / 100),
            }
        };
        let (iw, ih) = (img.width as u64, img.height as u64);
        let (w, h) = match (px(dw, cw, tw), px(dh, ch, th)) {
            (None, None) if iw > tw => (tw, ih * tw / iw),
            (None, None) => (iw, ih),
            (Some(w), None) => (w, if preserve { ih * w / iw } else { ih }),
            (None, Some(h)) => (if preserve { iw * h / ih } else { iw }, h),
            (Some(w), Some(h)) if preserve => {
                if iw * h <= ih * w {
                    (iw * h / ih, h)
                } else {
                    (w, ih * w / iw)
                }
            }
            (Some(w), Some(h)) => (w, h),
        };
        let clamp = |v: u64| v.clamp(1, 1 << 15) as u32;
        let size = (clamp(w), clamp(h));
        self.place_anonymous(Source::Iterm, img.width, img.height, img.frames, Some(size));
    }

    // --- interaction with the rest of the emulator ------------------------------

    /// XTSMGRAPHICS (`CSI ? Pi ; Pa ; Pv S`): report Sixel limits.
    pub(super) fn xtsmgraphics(&mut self, item: usize, action: usize) {
        let (cw, ch) = self.cell_pixel_size();
        let (w, h) = (self.cols() as u32 * cw, self.lines() as u32 * ch);
        let body = match (item, action) {
            (1, 1..=4) => "0;256".to_string(),
            (2, 1..=4) => format!("0;{w};{h}"),
            (1 | 2, _) => "2".to_string(),
            _ => "1".to_string(),
        };
        self.reply(&format!("\x1b[?{item};{body}S"));
    }
}
