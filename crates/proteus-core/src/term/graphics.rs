//! Graphics protocol front ends: kitty graphics (`APC G`), Sixel (`DCS q`)
//! and iTerm2 inline images (`OSC 1337 File=`), plus the APC pre-filter that
//! `vte` lacks.

use std::collections::HashSet;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use super::{BASE64, Terminal, join_params};
use crate::event::Terminator;
use crate::graphics::kitty::{self, Command, Error, err};
use crate::graphics::media::{self, Medium};
use crate::graphics::sixel::SixelDecoder;
use crate::graphics::{
    AnimationState, DEFAULT_GAP, DecodeError, DecodedImage, FALLBACK_CELL_PX, FRAME_OVERHEAD, Fit, Frame, Image,
    ImageStore, ImageView, MAX_FRAMES, MAX_RELATIVE_DEPTH, Placement, PxRect, Source, Upload, decode_image,
    image_dimensions, run_in_background,
};
use crate::modes::TermMode;
use base64::Engine as _;

/// State of the string pre-filter: vte discards APC strings and buffers OSC
/// strings without a limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Apc {
    #[default]
    Ground,
    /// Saw ESC outside a string.
    Esc,
    Body,
    /// Saw ESC inside an APC string.
    BodyEsc,
    /// Inside an OSC string that vte is buffering.
    Osc,
    /// Saw ESC inside an OSC string.
    OscEsc,
    /// Dropping the rest of an oversized OSC string.
    OscDiscard,
    /// Saw ESC while dropping an oversized OSC string.
    OscDiscardEsc,
}

/// OSC strings larger than this make the parser be replaced afterwards, so
/// its buffer does not keep the peak allocation.
const OSC_SHRINK: usize = 64 << 10;

/// Largest GIF frame delay treated as "as fast as possible" (browser convention).
const MIN_GIF_DELAY_MS: u32 = 10;

/// Frame gap for a GIF delay.
fn gif_gap(d: u32) -> i32 {
    if d <= MIN_GIF_DELAY_MS { 100 } else { d.min(i32::MAX as u32) as i32 }
}

/// A kitty reply held back until the image is decoded.
pub(super) struct HeldReply {
    cmd: Command,
    id: u32,
    number: u32,
    pid: u32,
    /// Outcome of the placement made before decoding.
    put: Result<(), Error>,
}

/// Result of a background decode.
pub(super) struct Decoded {
    key: u64,
    _permit: Option<crate::graphics::DecodePermit>,
    result: Result<DecodedImage, DecodeError>,
    reply: Option<HeldReply>,
}

/// Images being decoded on background threads (see [`crate::GraphicsConfig::waker`]).
pub(super) struct Decodes {
    tx: Sender<Decoded>,
    rx: Receiver<Decoded>,
    pending: usize,
    /// Frame and animation commands waiting for their image to decode.
    deferred: Vec<(u64, Command)>,
}

/// Most commands held back until an image decodes.
const MAX_DEFERRED: usize = 64;

impl Default for Decodes {
    fn default() -> Self {
        let (tx, rx) = channel();
        Self { tx, rx, pending: 0, deferred: Vec::new() }
    }
}

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
    // vte 0.15 can consume an extra ASCII byte when completing a partial
    // UTF-8 scalar from a previous call. Feeding the first three bytes singly
    // completes any pending scalar without giving that path surplus input.
    // The rest retains the parser's fast bulk path; no buffering or copies.
    fn advance_parser(&mut self, parser: &mut vte::Parser, bytes: &[u8]) {
        let mut prefix = bytes.len().min(3);
        // Do not introduce a new split inside a scalar at our bulk boundary.
        while prefix < bytes.len().min(6) && bytes[prefix] & 0xc0 == 0x80 {
            prefix += 1;
        }
        for byte in &bytes[..prefix] {
            parser.advance(self, std::slice::from_ref(byte));
        }
        if prefix < bytes.len() {
            parser.advance(self, &bytes[prefix..]);
        }
    }

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
                        self.advance_parser(parser, &bytes[start..=i]);
                        start = i + 1;
                        self.apc = Apc::Body;
                        self.apc_buf.clear();
                        self.apc_overflow = false;
                    } else if b == b']' {
                        self.apc = Apc::Osc;
                        self.osc_len = 0;
                        self.osc_head.clear();
                    } else if b != 0x1b {
                        self.apc = Apc::Ground;
                    }
                }
                Apc::Osc => {
                    let rest = &bytes[i..];
                    let end = rest.iter().position(|&b| matches!(b, 0x07 | 0x18 | 0x1a | 0x1b));
                    let len = end.unwrap_or(rest.len());
                    let head = self.osc_head.len();
                    if head < 5 {
                        self.osc_head.extend_from_slice(&rest[..len.min(5 - head)]);
                    }
                    if self.osc_len + len > self.max_osc() {
                        tracing::debug!(
                            kind = "osc",
                            size = self.osc_len + len,
                            max = self.max_osc(),
                            "dropping oversized string"
                        );
                        self.advance_parser(parser, &bytes[start..i]);
                        self.osc_drop = true;
                        self.advance_parser(parser, b"\x07");
                        *parser = vte::Parser::new();
                        self.apc = Apc::OscDiscard;
                        start = bytes.len();
                        continue;
                    }
                    self.osc_len += len;
                    let Some(off) = end else { break };
                    i += off;
                    if bytes[i] == 0x1b {
                        self.advance_parser(parser, &bytes[start..i]);
                        start = i + 1;
                        self.apc = Apc::OscEsc;
                    } else {
                        self.advance_parser(parser, &bytes[start..=i]);
                        start = i + 1;
                        self.osc_finished(parser);
                    }
                }
                Apc::OscEsc => {
                    if bytes[i] == b'\\' {
                        self.advance_parser(parser, b"\x1b\\");
                        start = i + 1;
                        self.osc_finished(parser);
                    } else {
                        self.advance_parser(parser, b"\x1b");
                        start = i;
                        self.apc = Apc::Esc;
                        continue;
                    }
                }
                Apc::OscDiscard => {
                    match bytes[i..].iter().position(|&b| matches!(b, 0x07 | 0x18 | 0x1a | 0x1b)) {
                        Some(off) => i += off,
                        None => break,
                    }
                    self.apc = if bytes[i] == 0x1b { Apc::OscDiscardEsc } else { Apc::Ground };
                    start = i + 1;
                }
                Apc::OscDiscardEsc => {
                    if bytes[i] == b'\\' {
                        start = i + 1;
                        self.apc = Apc::Ground;
                    } else {
                        self.advance_parser(parser, b"\x1b");
                        start = i;
                        self.apc = Apc::Esc;
                        continue;
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
                    let mut data = std::mem::take(&mut self.apc_buf);
                    if !std::mem::take(&mut self.apc_overflow) {
                        self.apc_dispatch(&data);
                    }
                    data.clear();
                    if data.capacity() > OSC_SHRINK {
                        data = Vec::new();
                    }
                    self.apc_buf = data;
                    if bytes[i] == b'\\' {
                        self.advance_parser(parser, b"\x1b\\");
                        start = i + 1;
                        self.apc = Apc::Ground;
                    } else {
                        // ESC aborted the string; it starts a new sequence.
                        self.advance_parser(parser, b"\x1b");
                        start = i;
                        self.apc = Apc::Esc;
                        continue;
                    }
                }
            }
            i += 1;
        }
        if start < bytes.len() && matches!(self.apc, Apc::Ground | Apc::Esc | Apc::Osc) {
            self.advance_parser(parser, &bytes[start..]);
        }
    }

    /// Only iTerm2 images (OSC 1337) may be as large as an image; everything
    /// else fits the largest clipboard write (OSC 52).
    fn max_osc(&self) -> usize {
        let image =
            self.osc_head.starts_with(b"1337;") || (self.osc_head.len() < 5 && b"1337;".starts_with(&self.osc_head));
        let max =
            if image { super::MAX_OSC52.max(self.config.graphics.max_image_bytes / 3 * 4) } else { super::MAX_OSC52 };
        max + 8192
    }

    fn osc_finished(&mut self, parser: &mut vte::Parser) {
        if self.osc_len > OSC_SHRINK {
            *parser = vte::Parser::new();
        }
        self.apc = Apc::Ground;
    }

    fn apc_push(&mut self, chunk: &[u8]) {
        if self.apc_overflow {
            return;
        }
        let max = self.config.graphics.max_image_bytes / 3 * 4 + 8192;
        if self.apc_buf.len() + chunk.len() > max {
            tracing::debug!(kind = "apc", size = self.apc_buf.len() + chunk.len(), max, "dropping oversized string");
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

    /// File-based kitty media are only allowed for panes the host reports as
    /// local (see [`Terminal::set_local`]); an OSC 7 cwd on another host also
    /// disables them, but the program cannot enable them through OSC 7.
    fn files_allowed(&self) -> bool {
        let g = &self.config.graphics;
        g.allow_local_files
            && self.local
            && self.cwd.as_ref().is_none_or(|c| match &g.hostname {
                Some(h) => c.is_local(h),
                None => matches!(c.host.as_deref(), None | Some("") | Some("localhost")),
            })
    }

    /// Whether the program in the foreground runs on this machine. Only then
    /// may it send images as local file, temp-file or shared-memory names.
    pub fn set_local(&mut self, local: bool) {
        self.local = local;
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
        // Frame and animation commands need the pixels of the image; while it
        // is still decoding they run once the decode finishes.
        if matches!(cmd.action(), b'f' | b'a' | b'c') {
            self.poll_images();
            if let Ok(key) = self.kitty_lookup(&cmd)
                && (self.images.image(key).is_some_and(|i| i.is_loading())
                    || self.decodes.deferred.iter().any(|(k, _)| *k == key))
            {
                let held: usize = self.decodes.deferred.iter().map(|(_, c)| c.payload.len()).sum();
                let max = self.config.graphics.max_image_bytes / 3 * 4 + 8192;
                if self.decodes.deferred.len() >= MAX_DEFERRED || held + cmd.payload.len() > max {
                    self.kitty_reply(&cmd, id, number, 0, Err(err("EBUSY", "Too many commands wait for a decode")));
                } else {
                    self.decodes.deferred.push((key, cmd));
                }
                return;
            }
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
        if let Err(e) = &r {
            tracing::debug!(
                action = %char::from(cmd.action()),
                medium = %char::from(cmd.ch(b't', b'd')),
                id,
                number,
                error = %e,
                "kitty graphics command failed"
            );
        }
        if id == 0 && number == 0 {
            return;
        }
        let q = cmd.quiet();
        // Errors about file media all look alike so a client cannot probe files.
        let r = match r {
            Err(_) if matches!(cmd.ch(b't', b'd'), b'f' | b't' | b's') => {
                Err(err("EBADF", "Failed to read image file"))
            }
            r => r,
        };
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
                    let g = &self.config.graphics;
                    let reason = if !g.allow_local_files {
                        "local file media disabled"
                    } else if !self.local {
                        "pane is not local"
                    } else {
                        "cwd reported on another host"
                    };
                    tracing::debug!(medium = ?m, reason, "kitty graphics file medium refused");
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
        let png = cmd.has(b'f') && cmd.u(b'f') == 100;
        if png && cmd.action() != b'q' && self.config.graphics.waker.is_some() {
            return self.kitty_transmit_later(cmd);
        }
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

    /// `a=t`/`a=T` of a compressed image: store and place it with its
    /// declared size right away, decode in the background, reply when done.
    fn kitty_transmit_later(&mut self, cmd: Command) {
        let (id, number) = (cmd.id(), cmd.number());
        let data = match self.kitty_data(&cmd) {
            Ok(d) => d,
            Err(e) => return self.kitty_reply(&cmd, id, number, 0, Err(e)),
        };
        let max = self.config.graphics.max_image_bytes;
        let (w, h) = match image_dimensions(&data) {
            Some((w, h)) if w > 0 && h > 0 => (w, h),
            _ => {
                crate::graphics::log_decode_failure(&data, "unreadable or empty image header");
                return self.kitty_reply(&cmd, id, number, 0, Err(err("EBADPNG", "Failed to decode image")));
            }
        };
        let bytes = w as usize * h as usize * 4;
        if bytes > max {
            return self.kitty_reply(&cmd, id, number, 0, Err(err("EFBIG", "Image too large")));
        }
        let id = if number != 0 { self.images.alloc_id() } else { id };
        if let Some(old) = self.images.key_for_id(id) {
            self.images.remove_image(old);
            self.damage.images = true;
        }
        let other = self.images_inactive.bytes();
        if let Err(e) = self.images.reserve(bytes, other, &self.config.graphics, None) {
            return self.kitty_reply(&cmd, id, number, 0, Err(e));
        }
        let key = self.images.insert(Image::loading(id, number, Source::Kitty, w, h));
        let mut pid = 0;
        let mut put = Ok(());
        if cmd.action() == b'T' {
            pid = cmd.placement();
            put = self.kitty_put(&cmd, Some(key));
            if id == 0
                && let Err(e) = put
            {
                self.images.remove_image(key);
                return self.kitty_reply(&cmd, id, number, pid, Err(e));
            }
        }
        let mut held = cmd;
        held.payload = Vec::new();
        self.decode_later(key, data, Some(HeldReply { cmd: held, id, number, pid, put }));
    }

    // --- background decoding -------------------------------------------------

    fn decode_later(&mut self, key: u64, data: Vec<u8>, reply: Option<HeldReply>) {
        let tx = self.decodes.tx.clone();
        let waker = self.config.graphics.waker.clone();
        let configured_max = self.config.graphics.max_image_bytes;
        let frame = image_dimensions(&data)
            .and_then(|(w, h)| (w as usize).checked_mul(h as usize)?.checked_mul(4))
            .unwrap_or(1 << 20)
            .min(configured_max);
        // Each GIF frame takes at least 12 bytes (image descriptor and data);
        // its decoder composites onto a canvas besides the frame.
        let (max, scratch) = if data.starts_with(b"GIF8") {
            (frame.saturating_mul(data.len() / 12 + 1).min(configured_max), frame.saturating_mul(2))
        } else {
            (frame, frame)
        };
        self.decodes.pending += 1;
        let need = data.len().saturating_add(max).saturating_add(scratch);
        let Some(permit) = crate::graphics::reserve_decode(need) else {
            self.finish_decode(Decoded { key, _permit: None, result: Err(DecodeError::TooLarge), reply });
            return;
        };
        run_in_background(Box::new(move || {
            let decode = std::panic::AssertUnwindSafe(|| decode_image(&data, max));
            let result = std::panic::catch_unwind(decode).unwrap_or_else(|payload| {
                let message = payload
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("non-string panic payload");
                tracing::error!(panic = message, len = data.len(), "image decoder panicked");
                Err(DecodeError::Invalid("panic".into()))
            });
            if tx.send(Decoded { key, _permit: Some(permit), result, reply }).is_ok()
                && let Some(w) = waker
            {
                (w.0)();
            }
        }));
    }

    /// Apply finished background decodes.
    pub(super) fn poll_images(&mut self) {
        while self.decodes.pending > 0 {
            match self.decodes.rx.try_recv() {
                Ok(d) => self.finish_decode(d),
                Err(_) => break,
            }
        }
        self.prune_deferred();
    }

    /// Drop commands waiting for images that were removed meanwhile.
    fn prune_deferred(&mut self) {
        let (a, b) = (&self.images, &self.images_inactive);
        let waiting = |k: u64| a.image(k).or_else(|| b.image(k)).is_some_and(|i| i.is_loading());
        if self.decodes.deferred.iter().all(|(k, _)| waiting(*k)) {
            return;
        }
        let (keep, gone): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.decodes.deferred).into_iter().partition(|(k, _)| waiting(*k));
        self.decodes.deferred = keep;
        for (_, cmd) in gone {
            self.kitty_reply(&cmd, cmd.id(), cmd.number(), 0, Err(err("ENOENT", "Image not found")));
        }
    }

    fn finish_decode(&mut self, d: Decoded) {
        self.decodes.pending = self.decodes.pending.saturating_sub(1);
        self.damage.images = true;
        let key = d.key;
        let primary = self.images.image(key).is_some();
        let (store, other) = if primary {
            (&mut self.images, self.images_inactive.bytes())
        } else {
            (&mut self.images_inactive, self.images.bytes())
        };
        let outcome = match d.result {
            Err(DecodeError::TooLarge) => Err(err("EFBIG", "Image too large")),
            Err(DecodeError::Invalid(_)) => Err(err("EBADPNG", "Failed to decode image")),
            Ok(dec) => match store.image_mut(key) {
                // Deleted meanwhile, or evicted: nothing to fill in.
                None => Ok(()),
                Some(img) if img.evicted => {
                    img.loading = false;
                    Ok(())
                }
                Some(img) if (dec.width, dec.height) != img.size() => {
                    tracing::warn!(
                        format = ?dec.format,
                        header = ?img.size(),
                        decoded = ?(dec.width, dec.height),
                        "image size differs from its header"
                    );
                    Err(err("EBADPNG", "Image size differs from its header"))
                }
                Some(img) => {
                    let mut frames = dec.frames.into_iter();
                    let (first, delay) = frames.next().expect("decoders yield a frame");
                    img.frames[0].pixels = first;
                    img.loading = false;
                    img.generation += 1;
                    // Animated (iTerm2 GIF): the other frames need room too.
                    let rest: Vec<(Vec<u8>, u32)> =
                        if d.reply.is_none() { frames.take(MAX_FRAMES - 1).collect() } else { Vec::new() };
                    let need = rest.len() * img.extra_frame_bytes();
                    if !rest.is_empty() {
                        if store.reserve(need, other, &self.config.graphics, Some(key)).is_ok() {
                            let img = store.image_mut(key).expect("protected from eviction");
                            img.frames[0].gap = gif_gap(delay);
                            img.frames.extend(rest.into_iter().map(|(pixels, d)| Frame { pixels, gap: gif_gap(d) }));
                            img.state = AnimationState::Running;
                        } else {
                            tracing::debug!(
                                frames = rest.len() + 1,
                                bytes = need,
                                "no room for animation frames; showing the first only"
                            );
                        }
                    }
                    Ok(())
                }
            },
        };
        if outcome.is_err() {
            store.remove_image(key);
            store.free_anonymous();
        }
        if let Some(r) = d.reply {
            self.kitty_reply(&r.cmd, r.id, r.number, r.pid, outcome.and(r.put));
        }
        if self.decodes.deferred.iter().any(|(k, _)| *k == key) {
            let (ready, rest) = std::mem::take(&mut self.decodes.deferred).into_iter().partition(|(k, _)| *k == key);
            self.decodes.deferred = rest;
            for (_, cmd) in ready {
                self.kitty_run(cmd);
            }
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
        let index = s.placement_index();
        // Placement sequence numbers to delete, plus images to drop outright.
        let mut doomed: HashSet<u64> = HashSet::new();
        let mut drop_images: HashSet<u64> = HashSet::new();
        let mut select = |f: &dyn Fn(&Placement) -> bool| {
            doomed.extend(s.placements().iter().filter(|p| f(p)).map(|p| p.seq));
        };
        match d.to_ascii_lowercase() {
            b'a' => select(&|p| s.intersects_lines(&index, p, top, bottom)),
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
                    drop_images.insert(key);
                }
            }
            b'c' => select(&|p| s.intersects(&index, p, Some(cur.0), Some(cur.1))),
            b'p' => select(&|p| s.intersects(&index, p, Some(y), Some(x))),
            b'q' => select(&|p| p.z == z && s.intersects(&index, p, Some(y), Some(x))),
            b'x' => select(&|p| s.intersects(&index, p, None, Some(x))),
            b'y' => select(&|p| s.intersects(&index, p, Some(y), None)),
            b'z' => select(&|p| !p.is_virtual && p.z == z),
            b'r' => {
                let (lo, hi) = (cmd.u(b'x'), cmd.u(b'y'));
                let keys: HashSet<u64> =
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
        self.images.remove_images(&drop_images);
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
        let freed = (img.frames.len() - 1) * img.extra_frame_bytes();
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
        if cmd.u(b'r') == 0 && img.frames.len() >= MAX_FRAMES {
            return Err(err("ENOSPC", "Too many frames"));
        }
        let (iw, ih) = img.size();
        let px = self.kitty_pixels(cmd, (iw, ih))?;
        let (x, y) = (cmd.u(b'x'), cmd.u(b'y'));
        if x >= iw || y >= ih {
            return Err(err("EINVAL", "Frame offset out of bounds"));
        }
        let rect = PxRect { x: 0, y: 0, w: px.width.min(iw - x), h: px.height.min(ih - y) };
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
            let img = self.images.image(key).expect("checked");
            let extra = img.extra_frame_bytes();
            let base = cmd.u(b'c') as usize;
            if base > img.frames.len() {
                return Err(err("ENOENT", "Base frame not found"));
            }
            let other = self.images_inactive.bytes();
            self.images.reserve(extra, other, &self.config.graphics, Some(key))?;
            let img = self.images.image_mut(key).expect("protected from eviction");
            let mut canvas = if base > 0 {
                img.frames[base - 1].pixels.clone()
            } else {
                cmd.u(b'Y').to_be_bytes().repeat(iw as usize * ih as usize)
            };
            kitty::compose(&mut canvas, iw, &px.rgba, px.width, rect, x, y, replace);
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
        let replace = cmd.u(b'C') == 1;
        if src == dst {
            let f = &mut img.frames[src - 1].pixels;
            let (iw, sx, sy, w) = (iw as usize, sx as usize, sy as usize, w as usize);
            let mut rows = Vec::with_capacity(w * h as usize * 4);
            for y in sy..sy + h as usize {
                let s = (y * iw + sx) * 4;
                let Some(row) = f.get(s..s + w * 4) else { break };
                rows.extend_from_slice(row);
            }
            let rect = PxRect { x: 0, y: 0, w: w as u32, h };
            kitty::compose(f, iw as u32, &rows, w as u32, rect, dx, dy, replace);
        } else {
            let (a, b) = img.frames.split_at_mut(src.max(dst) - 1);
            let (lo, hi) = (&mut a[src.min(dst) - 1].pixels, &mut b[0].pixels);
            let (source, dest) = if src < dst { (lo, hi) } else { (hi, lo) };
            let rect = PxRect { x: sx, y: sy, w, h };
            kitty::compose(dest, iw, source, iw, rect, dx, dy, replace);
        }
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
            if img.frames.iter().all(|f| f.gap <= 0) {
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
        let (cw, ch) = self.cell_pixel_size();
        let screen = (self.grid.cols() * cw as usize, self.grid.lines() * ch as usize);
        g.enabled.then(|| {
            let mut dec = SixelDecoder::new(p2, g.max_image_bytes, g.window_budget.clone());
            dec.limit_padding(screen.0, screen.1);
            Box::new(dec)
        })
    }

    pub(super) fn sixel_finish(&mut self, dec: SixelDecoder) {
        if let Some(s) = dec.finish() {
            self.place_anonymous(Source::Sixel, s.width, s.height, Some(vec![(s.rgba, 0)]), None);
        }
    }

    /// Store and place a Sixel or iTerm2 image at the cursor (z = -1, below text).
    fn place_anonymous(
        &mut self,
        source: Source,
        w: u32,
        h: u32,
        frames: Option<Vec<(Vec<u8>, u32)>>,
        size: Option<(u32, u32)>,
    ) -> Option<u64> {
        let (cw, ch) = self.cell_pixel_size();
        let (dw, dh) = size.unwrap_or((w, h));
        let cols = ceil_div(dw as u64, cw as u64).max(1);
        let rows = ceil_div(dh as u64, ch as u64).max(1);
        let fixed = source == Source::Sixel && self.mode.contains(TermMode::SIXEL_DISPLAY);
        let (line, col) =
            if fixed { (self.grid.abs_line(0) as i64, 0) } else { (self.abs_cursor_line(), self.cursor.x) };
        // Replace earlier images drawn at the same spot (before inserting the new one).
        self.images.remove_covered(line, col, cols, rows);

        // Still being decoded: room for the first frame.
        let frame = w as usize * h as usize * 4;
        let extra = frames.as_ref().map_or(0, |f| f.len().saturating_sub(1));
        let need = frame + extra * frame.max(FRAME_OVERHEAD);
        let other = self.images_inactive.bytes();
        if let Err(e) = self.images.reserve(need, other, &self.config.graphics, None) {
            tracing::debug!(?source, width = w, height = h, bytes = need, error = %e, "image not stored");
            return None;
        }
        let img = match frames {
            None => Image::loading(0, 0, source, w, h),
            Some(frames) => {
                let animated = frames.len() > 1;
                let mut frames = frames.into_iter();
                let (first, first_delay) = frames.next().expect("at least one frame");
                let mut img = Image::new(0, 0, source, w, h, first);
                if animated {
                    img.frames[0].gap = gif_gap(first_delay);
                    img.frames.extend(frames.map(|(pixels, d)| Frame { pixels, gap: gif_gap(d) }));
                    img.state = AnimationState::Running;
                }
                img
            }
        };
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
        Some(key)
    }

    /// OSC 1337 (iTerm2): `File=`, multipart files and `ReportCellSize`.
    pub(super) fn osc_iterm(&mut self, params: &[&[u8]], term: Terminator) {
        let body = join_params(params, 1);
        let max = self.config.graphics.max_image_bytes / 3 * 4 + 8192;
        if !body.starts_with("FilePart=") && body != "FileEnd" {
            self.iterm_multipart = None;
        }
        if let Some(rest) = body.strip_prefix("File=") {
            let (args, data) = rest.split_once(':').unwrap_or((rest, ""));
            self.iterm_file(args, data.as_bytes());
        } else if let Some(args) = body.strip_prefix("MultipartFile=") {
            self.iterm_multipart = Some((args.to_string(), Vec::new()));
        } else if let Some(part) = body.strip_prefix("FilePart=") {
            if let Some((_, buf)) = &mut self.iterm_multipart {
                if buf.len() + part.len() > max {
                    tracing::debug!(
                        kind = "iterm2 multipart file",
                        size = buf.len() + part.len(),
                        max,
                        "dropping oversized string"
                    );
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
                "size" if v.parse::<usize>().is_ok_and(|n| n > max) => {
                    tracing::debug!(declared_size = v, max, "iterm2 image larger than the limit; ignored");
                    return;
                }
                _ => {}
            }
        }
        // Downloads (inline=0) are not supported.
        if !inline {
            tracing::debug!("iterm2 file download (inline=0) not supported; ignored");
            return;
        }
        if data.len() > max / 3 * 4 + 8192 {
            tracing::debug!(len = data.len(), max, "iterm2 image larger than the limit; ignored");
            return;
        }
        let Ok(bytes) = BASE64.decode(data) else {
            tracing::debug!(len = data.len(), "iterm2 image is not valid base64; ignored");
            return;
        };
        // Decode now, or only read the size and decode in the background.
        let (img, (iw, ih)) = if self.config.graphics.waker.is_some() {
            match image_dimensions(&bytes) {
                Some((w, h)) if w > 0 && h > 0 && w as usize * h as usize * 4 <= max => (None, (w, h)),
                Some((w, h)) if w > 0 && h > 0 => {
                    tracing::debug!(width = w, height = h, max, "iterm2 image larger than the limit; ignored");
                    return;
                }
                _ => {
                    crate::graphics::log_decode_failure(&bytes, "unreadable or empty image header");
                    return;
                }
            }
        } else {
            match decode_image(&bytes, max) {
                Ok(img) => {
                    let size = (img.width, img.height);
                    (Some(img), size)
                }
                Err(_) => return,
            }
        };
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
        let (iw, ih) = (iw as u64, ih as u64);
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
        let (w, h) = (iw as u32, ih as u32);
        match img {
            Some(img) => {
                self.place_anonymous(Source::Iterm, w, h, Some(img.frames), Some(size));
            }
            None => {
                if let Some(key) = self.place_anonymous(Source::Iterm, w, h, None, Some(size)) {
                    self.decode_later(key, bytes, None);
                }
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;

    #[test]
    fn deferred_commands_are_capped_and_dropped_with_their_image() {
        let mut config = Config::default();
        config.graphics.max_image_bytes = 4096;
        let mut t = Terminal::new(20, 5, config);
        let cfg = t.config.graphics.clone();
        t.images.reserve(16, 0, &cfg, None).unwrap();
        t.images.insert(Image::loading(9, 0, Source::Kitty, 2, 2));
        let frame = format!("\x1b_Ga=f,i=9,s=2,v=2;{}\x1b\\", "AAAA".repeat(3000));
        t.advance(frame.as_bytes());
        assert_eq!(t.decodes.deferred.len(), 1);
        t.advance(frame.as_bytes());
        assert_eq!(t.decodes.deferred.len(), 1);
        assert!(String::from_utf8(t.take_pty_output()).unwrap().starts_with("\x1b_Gi=9;EBUSY"));
        t.advance(b"\x1b_Ga=d,d=I,i=9\x1b\\");
        t.advance(b"");
        assert!(t.decodes.deferred.is_empty());
        assert!(String::from_utf8(t.take_pty_output()).unwrap().starts_with("\x1b_Gi=9;ENOENT"));
    }
}
