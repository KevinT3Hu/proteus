//! Streaming Sixel decoder (DCS P1;P2;P3 q ... ST) producing RGBA.

const REGISTERS: usize = 256;
const MAX_SIDE: usize = 1 << 14;
const MAX_PARAMS: usize = 8;

/// VT340 default palette, in percent.
const VT340: [(u32, u32, u32); 16] = [
    (0, 0, 0),
    (20, 20, 80),
    (80, 13, 13),
    (20, 80, 20),
    (80, 20, 80),
    (20, 80, 80),
    (80, 80, 20),
    (53, 53, 53),
    (26, 26, 26),
    (33, 33, 60),
    (60, 26, 26),
    (33, 60, 33),
    (60, 33, 60),
    (33, 60, 60),
    (60, 60, 33),
    (80, 80, 80),
];

fn pct(v: u32) -> u8 {
    ((v.min(100) * 255 + 50) / 100) as u8
}

fn rgba(r: u8, g: u8, b: u8) -> u32 {
    u32::from_le_bytes([r, g, b, 255])
}

/// DEC HLS (hue 0 = blue, 120 = red, 240 = green; L and S in percent) to RGB.
fn hls(h: u32, l: u32, s: u32) -> u32 {
    let h = ((h % 360) + 240) % 360;
    let (l, s) = (l.min(100) as f32 / 100.0, s.min(100) as f32 / 100.0);
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h as f32 / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let to = |v: f32| ((v + m).clamp(0.0, 1.0) * 255.0).round() as u8;
    rgba(to(r), to(g), to(b))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Data,
    Repeat,
    Color,
    Raster,
}

#[derive(Debug)]
pub(crate) struct SixelDecoder {
    palette: [u32; REGISTERS],
    color: usize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    /// Pixel storage, `stride` x `rows`, packed little-endian RGBA (0 = unset).
    buf: Vec<u32>,
    stride: usize,
    rows: usize,
    max_pixels: usize,
    state: State,
    params: Vec<u32>,
    cur: Option<u32>,
    transparent: bool,
}

/// A finished sixel image.
#[derive(Debug)]
pub(crate) struct Sixel {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl SixelDecoder {
    /// `p2 == 1` makes pixels that are never drawn transparent.
    pub fn new(p2: u16, max_bytes: usize) -> Self {
        let mut palette = [rgba(0, 0, 0); REGISTERS];
        for (i, &(r, g, b)) in VT340.iter().enumerate() {
            palette[i] = rgba(pct(r), pct(g), pct(b));
        }
        Self {
            palette,
            color: 0,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            buf: Vec::new(),
            stride: 0,
            rows: 0,
            max_pixels: max_bytes / 4,
            state: State::Data,
            params: Vec::new(),
            cur: None,
            transparent: p2 == 1,
        }
    }

    pub fn put(&mut self, b: u8) {
        if self.state != State::Data {
            match b {
                b'0'..=b'9' => {
                    let d = (b - b'0') as u32;
                    self.cur = Some(self.cur.unwrap_or(0).saturating_mul(10).saturating_add(d));
                    return;
                }
                b';' => {
                    if self.params.len() < MAX_PARAMS {
                        self.params.push(self.cur.take().unwrap_or(0));
                    }
                    self.cur = None;
                    return;
                }
                _ => {
                    if let Some(c) = self.cur.take()
                        && self.params.len() < MAX_PARAMS
                    {
                        self.params.push(c);
                    }
                    let state = std::mem::replace(&mut self.state, State::Data);
                    let params = std::mem::take(&mut self.params);
                    match state {
                        State::Repeat => {
                            if let 0x3f..=0x7e = b {
                                let n = params.first().copied().unwrap_or(1).max(1) as usize;
                                self.sixel(b - 0x3f, n);
                                return;
                            }
                        }
                        State::Color => self.color_cmd(&params),
                        State::Raster => self.raster(&params),
                        State::Data => {}
                    }
                }
            }
        }
        match b {
            b'"' => self.state = State::Raster,
            b'#' => self.state = State::Color,
            b'!' => self.state = State::Repeat,
            b'$' => self.x = 0,
            b'-' => {
                self.x = 0;
                self.y += 6;
            }
            0x3f..=0x7e => self.sixel(b - 0x3f, 1),
            _ => {}
        }
    }

    fn color_cmd(&mut self, p: &[u32]) {
        let Some(&pc) = p.first() else { return };
        let idx = pc as usize % REGISTERS;
        if p.len() >= 5 {
            self.palette[idx] = match p[1] {
                1 => hls(p[2], p[3], p[4]),
                2 => rgba(pct(p[2]), pct(p[3]), pct(p[4])),
                _ => self.palette[idx],
            };
        }
        self.color = idx;
    }

    fn raster(&mut self, p: &[u32]) {
        // Pan;Pad;Ph;Pv — pixel aspect is ignored (square pixels).
        if p.len() >= 4 {
            let (w, h) = (p[2] as usize, p[3] as usize);
            if self.reserve(w, h) {
                self.width = self.width.max(w);
                self.height = self.height.max(h);
            }
        }
    }

    /// Make sure `w` x `h` pixels are addressable. False if over the limits.
    fn reserve(&mut self, w: usize, h: usize) -> bool {
        if w <= self.stride && h <= self.rows {
            return true;
        }
        if w > MAX_SIDE || h > MAX_SIDE {
            return false;
        }
        let nw = if w > self.stride { w.max(self.stride * 2).clamp(64, MAX_SIDE) } else { self.stride };
        let nh = if h > self.rows { h.max(self.rows * 2).clamp(6, MAX_SIDE) } else { self.rows };
        let (nw, nh) = (nw.max(w), nh.max(h));
        if nw * nh > self.max_pixels {
            // Retry with an exact fit before giving up.
            if w.max(self.stride) * h.max(self.rows) > self.max_pixels {
                return false;
            }
            return self.grow(w.max(self.stride), h.max(self.rows));
        }
        self.grow(nw, nh)
    }

    fn grow(&mut self, nw: usize, nh: usize) -> bool {
        let mut nb = vec![0u32; nw * nh];
        for y in 0..self.rows {
            nb[y * nw..y * nw + self.stride].copy_from_slice(&self.buf[y * self.stride..(y + 1) * self.stride]);
        }
        self.buf = nb;
        self.stride = nw;
        self.rows = nh;
        true
    }

    fn sixel(&mut self, bits: u8, n: usize) {
        let n = n.min(MAX_SIDE);
        if bits != 0 {
            if !self.reserve(self.x + n, self.y + 6) {
                self.x += n;
                return;
            }
            let c = self.palette[self.color];
            for i in 0..6 {
                if bits & (1 << i) != 0 {
                    let row = (self.y + i) * self.stride;
                    self.buf[row + self.x..row + self.x + n].fill(c);
                }
            }
            let used = 8 - bits.leading_zeros() as usize;
            self.height = self.height.max(self.y + used);
            self.width = self.width.max(self.x + n);
        }
        self.x = (self.x + n).min(MAX_SIDE);
    }

    pub fn finish(self) -> Option<Sixel> {
        let (w, h) = (self.width.min(self.stride), self.height.min(self.rows));
        if w == 0 || h == 0 {
            return None;
        }
        let bg = if self.transparent { 0 } else { self.palette[0] };
        let mut rgba = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for &px in &self.buf[y * self.stride..y * self.stride + w] {
                let px = if px == 0 { bg } else { px };
                rgba.extend_from_slice(&px.to_le_bytes());
            }
        }
        Some(Sixel { width: w as u32, height: h as u32, rgba })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(p2: u16, data: &str) -> Sixel {
        let mut d = SixelDecoder::new(p2, 1 << 20);
        for b in data.bytes() {
            d.put(b);
        }
        d.finish().unwrap()
    }

    fn px(s: &Sixel, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * s.width + x) * 4) as usize;
        s.rgba[i..i + 4].try_into().unwrap()
    }

    #[test]
    fn colors_repeat_and_bands() {
        // Red 3 pixels wide full band, then next band one green pixel on row 6.
        let s = decode(1, "#1;2;100;0;0#1!3~-#2;2;0;100;0@");
        assert_eq!((s.width, s.height), (3, 7));
        assert_eq!(px(&s, 0, 0), [255, 0, 0, 255]);
        assert_eq!(px(&s, 2, 5), [255, 0, 0, 255]);
        assert_eq!(px(&s, 0, 6), [0, 255, 0, 255]);
        assert_eq!(px(&s, 1, 6), [0, 0, 0, 0]);
    }

    #[test]
    fn raster_and_opaque_background() {
        let s = decode(0, "\"1;1;4;2#3;2;0;0;100#3A");
        assert_eq!((s.width, s.height), (4, 2));
        assert_eq!(px(&s, 0, 1), [0, 0, 255, 255]);
        assert_eq!(px(&s, 3, 0), [0, 0, 0, 255]);
    }

    #[test]
    fn hls_primary_colors() {
        assert_eq!(hls(0, 50, 100), rgba(0, 0, 255));
        assert_eq!(hls(120, 50, 100), rgba(255, 0, 0));
        assert_eq!(hls(240, 50, 100), rgba(0, 255, 0));
    }

    #[test]
    fn huge_repeat_is_bounded() {
        let mut d = SixelDecoder::new(1, 4096);
        for b in "!99999~".bytes() {
            d.put(b);
        }
        assert!(d.finish().is_none());
    }
}
