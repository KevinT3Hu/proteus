//! File previews: what kind a file is, syntax-highlighted text, image
//! sizes and the freedesktop thumbnail cache. No host calls (testable).

use std::cell::OnceCell;
use std::io::Read;
use std::path::{Path, PathBuf};

use proteus_plugin::types::{Attrs, Color, Line, Span, Style};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, ThemeSet};
use syntect::parsing::SyntaxSet;

/// Text previews read at most this much.
pub const TEXT_BYTES: usize = 64 << 10;
/// Images bigger than this are not previewed.
pub const IMAGE_BYTES: u64 = 48 << 20;
/// Lines highlighted at most (highlighting is from the top).
const TEXT_LINES: usize = 400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum What {
    /// Decoded by the host (`load-image`).
    Image,
    Svg,
    Video,
    Pdf,
    Archive,
    Text,
    Binary,
}

/// The kind of a file from its mime type, name and first bytes.
pub fn classify(mime: &str, name: &str, head: &[u8]) -> What {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    let lower = name.to_ascii_lowercase();
    match mime {
        "image/png" | "image/jpeg" | "image/gif" | "image/webp" => return What::Image,
        "image/svg+xml" => return What::Svg,
        "application/pdf" => return What::Pdf,
        _ => {}
    }
    if mime.starts_with("video/") {
        return What::Video;
    }
    const ARCHIVES: [&str; 12] = ["zip", "tar", "tgz", "gz", "xz", "bz2", "zst", "7z", "rar", "jar", "deb", "iso"];
    let archive_mime = mime.contains("zip")
        || mime.contains("tar")
        || mime.contains("compressed")
        || mime.contains("archive")
        || mime == "application/x-xz"
        || mime == "application/zstd";
    if archive_mime || ARCHIVES.contains(&ext.as_str()) || lower.ends_with(".tar.gz") {
        return What::Archive;
    }
    if is_text(head) { What::Text } else { What::Binary }
}

/// No NUL bytes and mostly valid UTF-8 (a cut-off character at the end is fine).
pub fn is_text(head: &[u8]) -> bool {
    if head.contains(&0) {
        return false;
    }
    match std::str::from_utf8(head) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none() && e.valid_up_to() + 4 >= head.len(),
    }
}

/// The first `n` bytes of a file.
pub fn head(path: &Path, n: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(n.min(1 << 16));
    std::fs::File::open(path)?.take(n as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

thread_local! {
    static SYNTAXES: OnceCell<SyntaxSet> = const { OnceCell::new() };
    static THEMES: OnceCell<ThemeSet> = const { OnceCell::new() };
}

/// `text` highlighted for its file name, with line numbers, at most
/// `rows` lines of `cols` cells (longer lines are cut).
pub fn highlight(name: &str, text: &str, rows: usize, cols: usize, dark: bool, dim: Color) -> Vec<Line> {
    SYNTAXES.with(|ss| {
        THEMES.with(|ts| {
            let ss = ss.get_or_init(SyntaxSet::load_defaults_newlines);
            let ts = ts.get_or_init(ThemeSet::load_defaults);
            let ext = name.rsplit_once('.').map_or(name, |(_, e)| e);
            let first = text.lines().next().unwrap_or("");
            let syntax = ss
                .find_syntax_by_extension(ext)
                .or_else(|| ss.find_syntax_by_extension(name))
                .or_else(|| ss.find_syntax_by_first_line(first))
                .unwrap_or_else(|| ss.find_syntax_plain_text());
            let theme = &ts.themes[if dark { "base16-ocean.dark" } else { "InspiredGitHub" }];
            let mut h = HighlightLines::new(syntax, theme);
            let total = text.lines().count();
            let numw = total.clamp(1, TEXT_LINES).to_string().len();
            let mut out = Vec::new();
            for (i, line) in syntect::util::LinesWithEndings::from(text).enumerate().take(rows.min(TEXT_LINES)) {
                let mut spans = vec![Span {
                    text: format!("{:>numw$} ", i + 1),
                    style: Style { fg: Some(dim), ..Style::PLAIN },
                    link: None,
                }];
                let mut room = cols.saturating_sub(numw + 1);
                let ranges = h.highlight_line(line, ss).unwrap_or_default();
                for (st, piece) in ranges {
                    if room == 0 {
                        break;
                    }
                    let piece: String = piece.replace('\t', "    ").chars().filter(|c| !c.is_control()).collect();
                    let piece = proteus_plugin::fit(&piece, room);
                    room -= proteus_plugin::width(&piece).min(room);
                    let mut attrs = Attrs::empty();
                    if st.font_style.contains(FontStyle::BOLD) {
                        attrs |= Attrs::BOLD;
                    }
                    if st.font_style.contains(FontStyle::ITALIC) {
                        attrs |= Attrs::ITALIC;
                    }
                    let fg = st.foreground;
                    let fg = Color::Rgb((fg.r as u32) << 16 | (fg.g as u32) << 8 | fg.b as u32);
                    spans.push(Span { text: piece, style: Style { fg: Some(fg), bg: None, attrs }, link: None });
                }
                out.push(spans);
            }
            out
        })
    })
}

/// Pixel size of an image file's header.
pub fn image_size(head: &[u8]) -> Option<(u32, u32)> {
    let s = imagesize::blob_size(head).ok()?;
    Some((s.width as u32, s.height as u32)).filter(|&(w, h)| w > 0 && h > 0)
}

/// Cells for a `w`×`h` px image in at most `cols`×`rows` cells, keeping
/// its shape (cells are about twice as tall as wide) and not enlarging
/// small images beyond `cell_px` px per column.
pub fn fit_cells((w, h): (u32, u32), cols: usize, rows: usize, cell_px: u32) -> (u32, u32) {
    let (w, h) = (w.max(1) as f64, h.max(1) as f64);
    let natural = (w / cell_px.max(1) as f64).ceil();
    let c = (cols as f64).min(natural).max(1.0);
    let r = c * h / w / 2.0;
    let (c, r) = if r > rows as f64 { (rows as f64 * w / h * 2.0, rows as f64) } else { (c, r) };
    ((c.round() as u32).max(1), (r.round() as u32).max(1))
}

/// An SVG drawn to PNG bytes at most `max` px on its long side.
pub fn svg_png(data: &[u8], max: u32) -> Result<(Vec<u8>, (u32, u32)), String> {
    use resvg::{tiny_skia, usvg};
    let tree = usvg::Tree::from_data(data, &usvg::Options::default()).map_err(|e| e.to_string())?;
    let size = tree.size();
    let scale = (max as f32 / size.width().max(size.height())).min(4.0);
    let (w, h) = (((size.width() * scale).ceil() as u32).max(1), ((size.height() * scale).ceil() as u32).max(1));
    let mut pm = tiny_skia::Pixmap::new(w, h).ok_or("svg: bad size")?;
    resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pm.as_mut());
    Ok((pm.encode_png().map_err(|e| e.to_string())?, (w, h)))
}

/// The freedesktop thumbnail (`large`, 256 px) of a file.
pub fn thumbnail_path(path: &Path, home: &Path) -> PathBuf {
    use md5::{Digest, Md5};
    let uri = file_uri(path);
    let hash: String = Md5::digest(uri.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    home.join(".cache/thumbnails/large").join(format!("{hash}.png"))
}

/// `file://` URI with the path percent-encoded.
pub fn file_uri(path: &Path) -> String {
    let mut s = String::from("file://");
    for b in path.to_string_lossy().bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

/// Archive listing lines (`bsdtar -tf` / `unzip -Z1`): names, directories
/// folded to their entries.
pub fn archive_lines(out: &[u8], rows: usize) -> (Vec<String>, usize) {
    let text = String::from_utf8_lossy(out);
    let all: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    (all.iter().take(rows).map(|l| l.chars().filter(|c| !c.is_control()).collect()).collect(), all.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies() {
        assert_eq!(classify("image/png", "a.png", b""), What::Image);
        assert_eq!(classify("image/svg+xml", "a.svg", b"<svg"), What::Svg);
        assert_eq!(classify("video/mp4", "a.mp4", b""), What::Video);
        assert_eq!(classify("application/pdf", "a.pdf", b"%PDF"), What::Pdf);
        assert_eq!(classify("application/gzip", "a.tar.gz", b"\x1f\x8b"), What::Archive);
        assert_eq!(classify("application/octet-stream", "x.zip", b"PK\x03\x04\0"), What::Archive);
        assert_eq!(classify("text/plain", "a.rs", b"fn main() {}"), What::Text);
        assert_eq!(classify("application/octet-stream", "a.bin", b"\x7fELF\0\0"), What::Binary);
        // A character cut by the read limit is still text.
        assert!(is_text("中文".as_bytes().split_at(4).0));
        assert!(!is_text(b"\xff\xfe\xfd\xfc\xfb\xfa"));
    }

    #[test]
    fn highlights() {
        let l = highlight("a.rs", "fn main() {\n\tlet x = 1;\n}\n", 2, 40, true, Color::Indexed(8));
        assert_eq!(l.len(), 2);
        let text: String = l[1].iter().map(|s| s.text.as_str()).collect();
        assert_eq!(text, "2     let x = 1;");
        // Keywords get a color of their own.
        assert!(l[0].iter().filter(|s| s.style.fg.is_some()).count() > 2);
        let cut = highlight("a.txt", &"x".repeat(100), 5, 10, false, Color::Indexed(8));
        assert!(proteus_plugin::width(&cut[0].iter().map(|s| s.text.as_str()).collect::<String>()) <= 10);
    }

    #[test]
    fn images_and_thumbnails() {
        assert_eq!(fit_cells((800, 400), 40, 30, 8), (40, 10));
        assert_eq!(fit_cells((400, 800), 40, 20, 8), (20, 20));
        assert_eq!(fit_cells((16, 16), 40, 20, 8), (2, 1));
        let png = svg_png(br#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10"/>"#, 100).unwrap();
        assert_eq!(png.1, (80, 40));
        assert_eq!(image_size(&png.0), Some((80, 40)));
        assert_eq!(file_uri(Path::new("/a b/ü")), "file:///a%20b/%C3%BC");
        // The spec's example: md5 of "file:///home/jens/photos/me.png".
        let t = thumbnail_path(Path::new("/home/jens/photos/me.png"), Path::new("/h"));
        assert_eq!(t, Path::new("/h/.cache/thumbnails/large/c6ee772d9e49320e97ec29a7eb5b1697.png"));
        assert_eq!(archive_lines(b"a/\na/b\nc\n", 2), (vec!["a/".to_string(), "a/b".to_string()], 3));
    }
}
