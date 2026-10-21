//! Decoding of compressed image formats (PNG, JPEG, GIF, WebP) to RGBA.
//!
//! Declared dimensions are checked against the size cap before any pixel
//! buffer is allocated.

use std::io::Cursor;

use image::codecs::gif::GifDecoder;
use image::{AnimationDecoder, ImageDecoder, ImageFormat, ImageReader, Limits};

/// A decoded image: one or more RGBA8 frames of `width` x `height`.
#[derive(Debug, Clone)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    /// Format detected from the data.
    pub format: Option<ImageFormat>,
    /// Frame pixels (RGBA8, row-major) and the delay before the next frame in ms.
    pub frames: Vec<(Vec<u8>, u32)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// Decoded size would exceed the cap.
    TooLarge,
    Invalid(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::TooLarge => f.write_str("image too large"),
            DecodeError::Invalid(e) => write!(f, "invalid image: {e}"),
        }
    }
}

impl std::error::Error for DecodeError {}

fn invalid(e: impl std::fmt::Display) -> DecodeError {
    DecodeError::Invalid(e.to_string())
}

fn limits(max_bytes: usize) -> Limits {
    let mut l = Limits::default();
    l.max_image_width = Some(1 << 16);
    l.max_image_height = Some(1 << 16);
    // Decoders need some scratch space on top of the output buffer.
    l.max_alloc = Some((max_bytes as u64).saturating_mul(2));
    l
}

/// Decode `data`, refusing images whose RGBA size exceeds `max_bytes`.
/// Animated GIFs yield all frames that fit within `max_bytes` in total.
pub fn decode_image(data: &[u8], max_bytes: usize) -> Result<DecodedImage, DecodeError> {
    let reader = ImageReader::new(Cursor::new(data)).with_guessed_format().map_err(invalid)?;
    let format = reader.format();
    if format.is_none() {
        return Err(invalid("unknown format"));
    }
    let (w, h) = reader.into_dimensions().map_err(invalid)?;
    let frame_bytes = w as usize * h as usize * 4;
    if w == 0 || h == 0 {
        return Err(invalid("empty image"));
    }
    if frame_bytes > max_bytes {
        return Err(DecodeError::TooLarge);
    }
    if format == Some(ImageFormat::Gif) {
        let mut dec = GifDecoder::new(Cursor::new(data)).map_err(invalid)?;
        dec.set_limits(limits(max_bytes)).map_err(invalid)?;
        let mut frames = Vec::new();
        let mut total = 0usize;
        for f in dec.into_frames() {
            let f = f.map_err(invalid)?;
            let (num, den) = f.delay().numer_denom_ms();
            let delay = num.checked_div(den).unwrap_or(0);
            let buf = f.into_buffer();
            if buf.width() != w || buf.height() != h {
                continue;
            }
            total += frame_bytes;
            if total > max_bytes {
                break;
            }
            frames.push((buf.into_raw(), delay));
        }
        if frames.is_empty() {
            return Err(invalid("no frames"));
        }
        return Ok(DecodedImage { width: w, height: h, format, frames });
    }
    let mut reader = ImageReader::new(Cursor::new(data)).with_guessed_format().map_err(invalid)?;
    reader.limits(limits(max_bytes));
    let img = reader.decode().map_err(invalid)?.into_rgba8();
    let (w, h) = img.dimensions();
    Ok(DecodedImage { width: w, height: h, format, frames: vec![(img.into_raw(), 0)] })
}

/// Read only the dimensions of an encoded image.
pub fn image_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    ImageReader::new(Cursor::new(data)).with_guessed_format().ok()?.into_dimensions().ok()
}
