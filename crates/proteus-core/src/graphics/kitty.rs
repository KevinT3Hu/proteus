//! Kitty graphics protocol: command parsing and pixel-level helpers.
//! The terminal-side handling (placement, cursor, replies) lives in
//! `term/graphics.rs`.

use super::PxRect as Rect;
use super::decode::{DecodeError, decode_image};

/// A parsed `ESC _ G <control> ; <payload> ESC \` command.
#[derive(Debug, Clone)]
pub(crate) struct Command {
    vals: [i64; 128],
    set: u128,
    /// Raw (still base64-encoded) payload.
    pub payload: Vec<u8>,
}

/// Keys whose value is a single character rather than a number.
const CHAR_KEYS: &[u8] = b"atod";

impl Command {
    pub fn parse(data: &[u8]) -> Option<Command> {
        let (control, payload) = match data.iter().position(|&b| b == b';') {
            Some(i) => (&data[..i], &data[i + 1..]),
            None => (data, &[][..]),
        };
        let mut cmd = Command { vals: [0; 128], set: 0, payload: payload.to_vec() };
        for kv in control.split(|&b| b == b',') {
            if kv.is_empty() {
                continue;
            }
            let [key, b'=', value @ ..] = kv else { return None };
            if !key.is_ascii_alphabetic() || value.is_empty() {
                return None;
            }
            let v = if CHAR_KEYS.contains(key) {
                match value {
                    [c] => *c as i64,
                    _ => return None,
                }
            } else {
                std::str::from_utf8(value).ok()?.parse::<i64>().ok()?
            };
            cmd.vals[*key as usize] = v;
            cmd.set |= 1 << *key;
        }
        Some(cmd)
    }

    pub fn has(&self, key: u8) -> bool {
        self.set & (1 << key) != 0
    }
    pub fn int(&self, key: u8) -> i64 {
        self.vals[key as usize]
    }
    /// Value as an unsigned 32-bit integer (negative and huge values clamp).
    pub fn u(&self, key: u8) -> u32 {
        self.int(key).clamp(0, u32::MAX as i64) as u32
    }
    pub fn i(&self, key: u8) -> i32 {
        self.int(key).clamp(i32::MIN as i64, i32::MAX as i64) as i32
    }
    pub fn ch(&self, key: u8, default: u8) -> u8 {
        if self.has(key) { self.int(key) as u8 } else { default }
    }
    pub fn set(&mut self, key: u8, v: i64) {
        self.vals[key as usize] = v;
        self.set |= 1 << key;
    }

    pub fn action(&self) -> u8 {
        self.ch(b'a', b't')
    }
    pub fn quiet(&self) -> u32 {
        self.u(b'q')
    }
    pub fn id(&self) -> u32 {
        self.u(b'i')
    }
    pub fn number(&self) -> u32 {
        self.u(b'I')
    }
    pub fn placement(&self) -> u32 {
        self.u(b'p')
    }
    pub fn more(&self) -> bool {
        self.int(b'm') == 1
    }
}

/// A protocol error: `CODE:message`.
pub(crate) type Error = String;

pub(crate) fn err(code: &str, msg: &str) -> Error {
    format!("{code}:{msg}")
}

/// Raw pixel data decoded from a transmission.
#[derive(Debug)]
pub(crate) struct Pixels {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Convert transmitted data in format `f` (24, 32 or 100) to RGBA.
/// `(w, h)` are the declared dimensions for raw formats.
pub(crate) fn to_rgba(format: u32, data: Vec<u8>, w: u32, h: u32, max: usize) -> Result<Pixels, Error> {
    match format {
        24 | 32 => {
            if w == 0 || h == 0 {
                return Err(err("EINVAL", "Image dimensions not specified"));
            }
            let px = w as usize * h as usize;
            if px.saturating_mul(4) > max {
                return Err(err("EFBIG", "Image too large"));
            }
            let bpp = if format == 24 { 3 } else { 4 };
            if data.len() < px * bpp {
                return Err(err("ENODATA", "Insufficient image data"));
            }
            let rgba = if bpp == 4 {
                let mut d = data;
                d.truncate(px * 4);
                d
            } else {
                let mut out = Vec::with_capacity(px * 4);
                for p in data[..px * 3].as_chunks::<3>().0 {
                    out.extend_from_slice(&[p[0], p[1], p[2], 255]);
                }
                out
            };
            Ok(Pixels { width: w, height: h, rgba })
        }
        100 => match decode_image(&data, max) {
            Ok(mut img) => {
                let (rgba, _) = img.frames.swap_remove(0);
                Ok(Pixels { width: img.width, height: img.height, rgba })
            }
            Err(DecodeError::TooLarge) => Err(err("EFBIG", "Image too large")),
            Err(DecodeError::Invalid(_)) => Err(err("EBADPNG", "Failed to decode image")),
        },
        _ => Err(err("EINVAL", "Unknown image format")),
    }
}

/// Inflate zlib (`o=z`) data, refusing output larger than `max`.
pub(crate) fn inflate(data: &[u8], max: usize) -> Result<Vec<u8>, Error> {
    miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(data, max)
        .map_err(|_| err("EINVAL", "Failed to decompress image data"))
}

/// Source-over blend of one RGBA pixel onto another (non-premultiplied).
fn blend(dst: &mut [u8], src: &[u8]) {
    let sa = src[3] as u32;
    if sa == 255 {
        dst.copy_from_slice(src);
        return;
    }
    if sa == 0 {
        return;
    }
    let da = dst[3] as u32;
    let oa = sa * 255 + da * (255 - sa);
    if oa == 0 {
        dst.copy_from_slice(&[0; 4]);
        return;
    }
    for i in 0..3 {
        let v = (src[i] as u32 * sa * 255 + dst[i] as u32 * da * (255 - sa)) / oa;
        dst[i] = v as u8;
    }
    dst[3] = (oa / 255) as u8;
}

/// Copy `rect` of `src` (stride `sw`) to `dst` (stride `dw`) at `(dx, dy)`,
/// either replacing or alpha blending. Stops at the first row outside either buffer.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compose(dst: &mut [u8], dw: u32, src: &[u8], sw: u32, rect: Rect, dx: u32, dy: u32, replace: bool) {
    let n = rect.w as usize * 4;
    if n == 0 {
        return;
    }
    for row in 0..rect.h as usize {
        let s = ((rect.y as usize + row) * sw as usize + rect.x as usize) * 4;
        let d = ((dy as usize + row) * dw as usize + dx as usize) * 4;
        let (Some(dst), Some(src)) = (dst.get_mut(d..d + n), src.get(s..s + n)) else { return };
        if replace {
            dst.copy_from_slice(src);
        } else {
            for (d, s) in dst.as_chunks_mut::<4>().0.iter_mut().zip(src.as_chunks::<4>().0) {
                blend(d, s);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_control_data() {
        let c = Command::parse(b"a=T,f=24,s=2,v=1,i=7,z=-3;AAAA").unwrap();
        assert_eq!(c.action(), b'T');
        assert_eq!((c.u(b'f'), c.u(b's'), c.u(b'v'), c.id()), (24, 2, 1, 7));
        assert_eq!(c.i(b'z'), -3);
        assert_eq!(c.payload, b"AAAA");
        assert!(!c.has(b'm'));
        assert!(Command::parse(b"a=Tx").is_none());
        assert!(Command::parse(b"i=abc").is_none());
        assert_eq!(Command::parse(b"i=4294967295").unwrap().id(), u32::MAX);
    }

    #[test]
    fn rgb_conversion_and_limits() {
        let p = to_rgba(24, vec![1, 2, 3, 4, 5, 6], 2, 1, 1024).ok().unwrap();
        assert_eq!(p.rgba, [1, 2, 3, 255, 4, 5, 6, 255]);
        assert!(to_rgba(32, vec![0; 4], 2, 1, 1024).unwrap_err().starts_with("ENODATA"));
        assert!(to_rgba(32, vec![], 10_000, 10_000, 1024).unwrap_err().starts_with("EFBIG"));
    }

    #[test]
    fn blending() {
        let mut d = [0, 0, 255, 255];
        blend(&mut d, &[255, 0, 0, 128]);
        assert_eq!(d[3], 255);
        assert!(d[0] > 120 && d[0] < 135 && d[2] > 120);
    }
}
