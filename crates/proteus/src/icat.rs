//! `proteus +icat`: show images in the terminal with the kitty graphics
//! protocol (like `kitty +kitten icat`).
//!
//! Locally, PNG files are sent by path (`t=f`), so the terminal reads them
//! itself; over ssh, or for other formats, the image is converted to PNG and
//! sent inline (`t=d`) in chunks.

use std::io::{self, IsTerminal, Write};
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;

/// Bytes of base64 per escape sequence (the protocol's limit).
const CHUNK: usize = 4096;

/// Arguments of `proteus +icat` (a subcommand of the main command line).
#[derive(clap::Args, Debug)]
pub struct Icat {
    /// Image files (PNG, JPEG, GIF, WebP), or http(s) URLs (fetched with curl).
    #[arg(value_name = "FILE|URL", required_unless_present = "clear")]
    pub files: Vec<String>,
    /// Draw in a box of COLSxROWS cells at column X, row Y (0-based):
    /// `40x20@0x0`. The cursor does not move.
    #[arg(long, value_name = "WxH@XxY", value_parser = parse_place)]
    pub place: Option<Place>,
    /// Scale small images up to the terminal width (or the --place box).
    #[arg(long)]
    pub scale: bool,
    /// Delete all images shown in the terminal (before showing FILEs).
    #[arg(long)]
    pub clear: bool,
    /// Always send image data inline, even for local files.
    #[arg(long)]
    pub inline: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Place {
    pub cols: u32,
    pub rows: u32,
    pub x: u32,
    pub y: u32,
}

fn parse_place(s: &str) -> Result<Place, String> {
    let bad = || format!("`{s}`: expected COLSxROWS@XxY, like 40x20@0x0");
    let (size, at) = s.split_once('@').ok_or_else(bad)?;
    let pair = |p: &str| -> Result<(u32, u32), String> {
        let (a, b) = p.split_once('x').ok_or_else(bad)?;
        Ok((a.parse().map_err(|_| bad())?, b.parse().map_err(|_| bad())?))
    };
    let ((cols, rows), (x, y)) = (pair(size)?, pair(at)?);
    if cols == 0 || rows == 0 {
        return Err(bad());
    }
    Ok(Place { cols, rows, x, y })
}

/// Terminal size: (cols, rows, cell width px, cell height px).
fn term_size() -> (u32, u32, u32, u32) {
    let ws = [rustix::stdio::stdout(), rustix::stdio::stdin()]
        .into_iter()
        .find_map(|fd| rustix::termios::tcgetwinsize(fd).ok());
    match ws {
        Some(w) if w.ws_col > 0 && w.ws_row > 0 => {
            let (c, r) = (w.ws_col as u32, w.ws_row as u32);
            let cw = if w.ws_xpixel > 0 { w.ws_xpixel as u32 / c } else { 8 };
            let ch = if w.ws_ypixel > 0 { w.ws_ypixel as u32 / r } else { 16 };
            (c, r, cw.max(1), ch.max(1))
        }
        _ => (80, 24, 8, 16),
    }
}

fn is_remote() -> bool {
    ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"].iter().any(|v| std::env::var_os(v).is_some())
}

/// Encoded image bytes of a file or URL.
fn load(src: &str) -> Result<Vec<u8>, String> {
    if src.starts_with("http://") || src.starts_with("https://") {
        let out = std::process::Command::new("curl")
            .args(["-fsSL", "--max-filesize", "268435456", "--", src])
            .output()
            .map_err(|e| format!("cannot run curl: {e}"))?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }
        return Ok(out.stdout);
    }
    std::fs::read(src).map_err(|e| e.to_string())
}

/// The sequences that show one image: transmit-and-display with the size in
/// cells, chunked.
pub fn encode(control: &str, payload: &[u8]) -> String {
    let b = B64.encode(payload);
    let chunks: Vec<&str> = if b.is_empty() {
        vec![""]
    } else {
        b.as_bytes().chunks(CHUNK).map(|c| std::str::from_utf8(c).unwrap()).collect()
    };
    let mut out = String::with_capacity(b.len() + chunks.len() * 32);
    for (i, c) in chunks.iter().enumerate() {
        let m = u8::from(i + 1 < chunks.len());
        if i == 0 {
            out.push_str(&format!("\x1b_G{control},m={m};{c}\x1b\\"));
        } else {
            out.push_str(&format!("\x1b_Gm={m};{c}\x1b\\"));
        }
    }
    out
}

/// Cells for an image of `w` x `h` px: natural size, shrunk to fit `box`,
/// or grown to it with `scale`.
pub fn fit(w: u32, h: u32, cell: (u32, u32), bx: (u32, u32), scale: bool) -> (u32, u32) {
    let (cw, ch) = (cell.0.max(1) as f64, cell.1.max(1) as f64);
    let (nat_c, nat_r) = (w as f64 / cw, h as f64 / ch);
    let s = (bx.0 as f64 / nat_c).min(bx.1 as f64 / nat_r);
    let s = if scale { s } else { s.min(1.0) };
    (((nat_c * s).round() as u32).clamp(1, bx.0.max(1)), ((nat_r * s).round() as u32).clamp(1, bx.1.max(1)))
}

fn show(src: &str, args: &Icat, size: (u32, u32, u32, u32)) -> Result<String, String> {
    let (cols, rows, cw, ch) = size;
    let data = load(src)?;
    let format = image::guess_format(&data).map_err(|_| "not a supported image".to_string())?;
    let img = image::load_from_memory(&data).map_err(|e| e.to_string())?;
    let (w, h) = (img.width(), img.height());
    // Box: the --place box, else the terminal width and (at most) its height.
    let bx = match args.place {
        Some(p) => (p.cols, p.rows),
        None => (cols, rows.saturating_sub(1).max(1)),
    };
    let (c, r) = fit(w, h, (cw, ch), bx, args.scale);
    let ctrl = format!("a=T,f=100,c={c},r={r},q=2");
    let local = Path::new(src).is_file() && !is_remote() && !args.inline;
    let seq = if local && format == image::ImageFormat::Png {
        let path = std::fs::canonicalize(src).map_err(|e| e.to_string())?;
        encode(&format!("{ctrl},t=f"), path.as_os_str().as_encoded_bytes())
    } else if format == image::ImageFormat::Png {
        encode(&ctrl, &data)
    } else {
        let mut png = io::Cursor::new(Vec::new());
        img.write_to(&mut png, image::ImageFormat::Png).map_err(|e| e.to_string())?;
        encode(&ctrl, png.get_ref())
    };
    Ok(match args.place {
        // Save the cursor, draw at the box without moving it, restore.
        Some(p) => format!("\x1b7\x1b[{};{}H{}", p.y + 1, p.x + 1, seq.replacen("q=2", "q=2,C=1", 1)) + "\x1b8",
        None => seq + "\n",
    })
}

/// Run `proteus +icat ARGS`. Returns the exit code.
pub fn run(args: Icat) -> u8 {
    let mut out = io::stdout().lock();
    if !io::stdout().is_terminal() {
        eprintln!("proteus +icat: standard output is not a terminal");
    }
    let mut code = 0;
    if args.clear {
        let _ = out.write_all(b"\x1b_Ga=d,d=A,q=2\x1b\\");
    }
    let size = term_size();
    for f in &args.files {
        match show(f, &args, size) {
            Ok(s) => {
                let _ = out.write_all(s.as_bytes());
            }
            Err(e) => {
                eprintln!("proteus +icat: {f}: {e}");
                code = 1;
            }
        }
    }
    let _ = out.flush();
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn place() {
        assert_eq!(parse_place("40x20@1x2"), Ok(Place { cols: 40, rows: 20, x: 1, y: 2 }));
        assert!(parse_place("0x2@1x1").is_err());
        assert!(parse_place("4x2").is_err());
        assert!(parse_place("4x2@a").is_err());
    }

    #[test]
    fn sizing() {
        // Natural: 80x32 px at 8x16 = 10x2 cells.
        assert_eq!(fit(80, 32, (8, 16), (80, 23), false), (10, 2));
        // Too wide: shrunk to the box, aspect kept.
        assert_eq!(fit(1600, 320, (8, 16), (80, 23), false), (80, 8));
        // --scale grows it.
        assert_eq!(fit(80, 32, (8, 16), (40, 23), true), (40, 8));
    }

    #[test]
    fn chunks() {
        let s = encode("a=T", &[0u8; 6000]);
        let n = s.matches("\x1b_G").count();
        assert_eq!(n, 2);
        assert!(s.starts_with("\x1b_Ga=T,m=1;"));
        assert!(s.contains("\x1b_Gm=0;"));
    }
}
