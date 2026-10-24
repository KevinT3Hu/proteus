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

// Reserve both encoded input and the decoder's output/scratch allowance.
// The permit follows the result until the GUI consumes it, not just until
// decoding ends. This bounds queued jobs and queued decoded frames together.
static DECODE_USAGE: std::sync::Mutex<(usize, usize)> = std::sync::Mutex::new((0, 0));
#[derive(Debug)]
pub(crate) struct DecodePermit(usize);
impl Drop for DecodePermit {
    fn drop(&mut self) {
        let mut used = DECODE_USAGE.lock().unwrap();
        used.0 -= self.0;
        used.1 -= 1;
    }
}
pub(crate) fn reserve_decode(bytes: usize) -> Option<DecodePermit> {
    let mut used = DECODE_USAGE.lock().unwrap();
    if used.1 >= 32 || bytes > (512usize << 20).saturating_sub(used.0) {
        return None;
    }
    used.0 += bytes;
    used.1 += 1;
    Some(DecodePermit(bytes))
}

type Job = Box<dyn FnOnce() + Send>;

/// Run `job` on a small pool of decoder threads shared by all terminals.
pub(crate) fn run_in_background(job: Job) {
    use std::sync::mpsc::{Sender, channel};
    use std::sync::{Arc, Mutex, OnceLock};
    static POOL: OnceLock<Option<Sender<Job>>> = OnceLock::new();
    let pool = POOL.get_or_init(|| {
        let (tx, rx) = channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        let n = std::thread::available_parallelism().map_or(2, |n| n.get()).clamp(1, 4);
        let mut started = 0;
        for i in 0..n {
            let rx = rx.clone();
            let spawned = std::thread::Builder::new().name(format!("image-decode-{i}")).spawn(move || {
                loop {
                    let job = match rx.lock() {
                        Ok(rx) => rx.recv(),
                        Err(_) => return,
                    };
                    match job {
                        Ok(job) => job(),
                        Err(_) => return,
                    }
                }
            });
            started += spawned.is_ok() as usize;
        }
        (started > 0).then_some(tx)
    });
    match pool {
        Some(tx) => {
            if let Err(e) = tx.send(job) {
                (e.0)();
            }
        }
        // No threads: decode right here.
        None => job(),
    }
}

#[cfg(test)]
mod resource_tests {
    use super::*;

    #[test]
    fn queued_and_completed_decodes_share_byte_and_job_limits() {
        assert!(reserve_decode((512 << 20) + 1).is_none());
        let large = reserve_decode(512 << 20).unwrap();
        assert!(reserve_decode(1).is_none());
        drop(large);
        let permits: Vec<_> = (0..32).map(|_| reserve_decode(1).unwrap()).collect();
        assert!(reserve_decode(1).is_none());
        drop(permits);
        assert!(reserve_decode(1).is_some());
    }
}
