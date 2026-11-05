//! Inline graphics: kitty graphics protocol, Unicode placeholders, Sixel, iTerm2.

use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use proteus_core::graphics::placeholder::{PLACEHOLDER, diacritic};
use proteus_core::graphics::{AnimationState, GraphicsConfig, MIB};
use proteus_core::{Config, Terminal};

const CELL: (u32, u32) = (10, 20);

fn term(cols: usize, lines: usize) -> Terminal {
    term_with(cols, lines, Config::default())
}

fn term_with(cols: usize, lines: usize, config: Config) -> Terminal {
    let mut t = Terminal::new(cols, lines, config);
    t.set_cell_pixel_size(CELL.0, CELL.1);
    t
}

fn feed(t: &mut Terminal, s: &str) {
    t.advance(s.as_bytes());
}

fn out(t: &mut Terminal) -> String {
    String::from_utf8(t.take_pty_output()).unwrap()
}

fn apc(ctrl: &str, payload: &[u8]) -> String {
    format!("\x1b_G{ctrl};{}\x1b\\", B64.encode(payload))
}

/// `w` x `h` RGBA pixels of a single color.
fn rgba(w: u32, h: u32, c: [u8; 4]) -> Vec<u8> {
    c.repeat((w * h) as usize)
}

fn png(w: u32, h: u32, c: [u8; 4]) -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(w, h, image::Rgba(c));
    let mut buf = std::io::Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
    buf.into_inner()
}

fn cursor(t: &Terminal) -> (usize, usize) {
    (t.cursor().x, t.cursor().y)
}

#[test]
fn transmit_and_display() {
    let mut t = term(20, 10);
    feed(&mut t, "ab");
    feed(&mut t, &apc("a=T,f=24,s=25,v=30,i=7", &[9; 25 * 30 * 3]));
    assert_eq!(out(&mut t), "\x1b_Gi=7;OK\x1b\\");
    let img = t.images().image_by_id(7).unwrap();
    assert_eq!(img.size(), (25, 30));
    assert_eq!(&img.pixels().unwrap()[..4], &[9, 9, 9, 255]);
    let p = &t.images().placements()[0];
    // 25x30 px over 10x20 cells: 3 columns, 2 rows.
    assert_eq!(p.cells(), (3, 2));
    assert_eq!(p.anchor(), (0, 2));
    // Cursor ends on the image's last row, just after it.
    assert_eq!(cursor(&t), (5, 1));
    let v = t.visible_images();
    assert_eq!(v.len(), 1);
    assert_eq!((v[0].dest.x, v[0].dest.y, v[0].dest.w, v[0].dest.h), (20, 0, 25, 30));
    assert!(t.take_damage().images_changed());
}

#[test]
fn chunked_upload_across_reads_and_query() {
    let mut t = term(20, 10);
    let data = B64.encode(rgba(4, 4, [1, 2, 3, 4]));
    let (a, b) = data.split_at(32);
    let stream = format!("\x1b_Ga=t,s=4,v=4,i=3,m=1;{a}\x1b\\\x1b_Gm=0;{b}\x1b\\");
    // Feed byte by byte: the APC filter must survive arbitrary splits.
    for byte in stream.bytes() {
        t.advance(&[byte]);
    }
    assert_eq!(out(&mut t), "\x1b_Gi=3;OK\x1b\\");
    assert_eq!(t.images().image_by_id(3).unwrap().size(), (4, 4));
    assert!(t.images().placements().is_empty());
    // a=q answers but stores nothing; q=1 silences OK, q=2 also errors.
    feed(&mut t, &apc("a=q,i=31,s=1,v=1,f=24", &[0, 0, 0]));
    assert_eq!(out(&mut t), "\x1b_Gi=31;OK\x1b\\");
    assert!(t.images().image_by_id(31).is_none());
    feed(&mut t, &apc("a=p,i=99", b""));
    assert!(out(&mut t).starts_with("\x1b_Gi=99;ENOENT:"));
    feed(&mut t, &apc("a=p,i=99,q=2", b""));
    assert_eq!(out(&mut t), "");
    // Text after the APC is unaffected.
    feed(&mut t, "\rok");
    assert_eq!(t.row_text(0), "ok");
}

#[test]
fn image_numbers_and_placement_ids() {
    let mut t = term(20, 10);
    feed(&mut t, &apc("a=t,f=24,s=1,v=1,I=13", &[0, 0, 0]));
    let r = out(&mut t);
    let id: u32 = r.strip_prefix("\x1b_Gi=").unwrap().split(',').next().unwrap().parse().unwrap();
    assert!(r.ends_with(",I=13;OK\x1b\\"), "{r}");
    feed(&mut t, &apc("a=p,I=13,p=5,C=1", b""));
    assert_eq!(out(&mut t), format!("\x1b_Gi={id},I=13,p=5;OK\x1b\\"));
    // Same image + placement id replaces (moves) the placement.
    feed(&mut t, "\x1b[3;4H");
    feed(&mut t, &apc("a=p,i={id},p=5,C=1".replace("{id}", &id.to_string()).as_str(), b""));
    assert_eq!(t.images().placements().len(), 1);
    assert_eq!(t.images().placements()[0].anchor(), (2, 3));
    assert_eq!(cursor(&t), (3, 2));
    feed(&mut t, &apc("a=t,i=1,I=2", b""));
    assert!(out(&mut t).contains("EINVAL"));
}

#[test]
fn scrolls_with_text_and_drops_with_scrollback() {
    let cfg = Config { scrollback: 5, ..Config::default() };
    let mut t = term_with(10, 4, cfg);
    feed(&mut t, &apc("a=T,f=24,s=10,v=20,i=1,q=1", &[0; 600]));
    feed(&mut t, "\r\n\n\n\n");
    // Screen scrolled by one line: the image is now just above the screen.
    assert_eq!(t.grid().abs_line(0), 1);
    assert_eq!(t.images().placements()[0].anchor(), (0, 0));
    assert!(t.visible_images().is_empty());
    t.scroll_display(1);
    assert_eq!(t.visible_images()[0].dest.y, 0);
    t.scroll_to_bottom();
    feed(&mut t, &"\n".repeat(6));
    assert!(t.images().placements().is_empty());
    // Data stays until quota pressure or an explicit delete.
    assert!(t.images().image_by_id(1).is_some());
}

#[test]
fn region_scroll_on_alt_screen() {
    let mut t = term(10, 6);
    feed(&mut t, "\x1b[?1049h\x1b[2;5r\x1b[3;1H");
    feed(&mut t, &apc("a=T,f=24,s=10,v=20,i=1,q=1,C=1", &[0; 600]));
    assert_eq!(t.images().placements()[0].anchor(), (2, 0));
    // Scroll the region up twice: the image moves with the text...
    feed(&mut t, "\x1b[5;1H\n");
    assert_eq!(t.images().placements()[0].anchor(), (1, 0));
    // ...and is dropped once it leaves the region.
    feed(&mut t, "\n");
    assert!(t.images().placements().is_empty());
    feed(&mut t, &apc("a=p,i=1,q=1,C=1", b""));
    feed(&mut t, "\x1b[?1049l");
    // Alternate-screen images are gone with it; the primary has none.
    assert!(t.images().is_empty());
    feed(&mut t, "\x1b[?1049h");
    assert!(t.images().is_empty());
}

#[test]
fn clear_screen_and_delete_commands() {
    let mut t = term(20, 10);
    let one = |t: &mut Terminal, ctrl: &str| feed(t, &apc(&format!("{ctrl},f=24,s=10,v=20,q=2,C=1"), &[0; 600]));
    one(&mut t, "a=T,i=1");
    feed(&mut t, "\x1b[2J");
    assert!(t.images().placements().is_empty());
    assert!(t.images().image_by_id(1).is_some());

    feed(&mut t, "\x1b[1;1H");
    one(&mut t, "a=T,i=2,z=5");
    feed(&mut t, "\x1b[5;5H");
    one(&mut t, "a=T,i=3");
    feed(&mut t, &apc("a=d,d=p,x=5,y=5", b""));
    assert_eq!(t.images().placements().len(), 1);
    assert!(t.images().image_by_id(3).is_some(), "lowercase keeps data");
    feed(&mut t, &apc("a=d,d=Z,z=5", b""));
    assert!(t.images().placements().is_empty());
    assert!(t.images().image_by_id(2).is_none(), "uppercase frees data");
    feed(&mut t, &apc("a=d,d=I,i=3", b""));
    assert!(t.images().image_by_id(3).is_none());
    feed(&mut t, &apc("a=p,i=1,q=2", b""));
    feed(&mut t, &apc("a=d", b""));
    assert!(t.images().placements().is_empty());
}

#[test]
fn reflow_moves_anchor() {
    let mut t = term(10, 5);
    feed(&mut t, "0123456789abcdef\r\n");
    feed(&mut t, &apc("a=T,f=24,s=10,v=20,i=1,q=2", &[0; 600]));
    assert_eq!(t.images().placements()[0].anchor(), (2, 0));
    // Wider: the wrapped line joins, the image's line moves up.
    t.resize(20, 5);
    assert_eq!(t.images().placements()[0].anchor(), (1, 0));
    assert_eq!(t.row_text(0), "0123456789abcdef");
}

#[test]
fn unicode_placeholders() {
    let mut t = term(20, 5);
    feed(&mut t, &apc("a=T,U=1,f=24,s=20,v=40,i=42,c=2,r=2,q=2", &[0; 20 * 40 * 3]));
    // A virtual placement draws nothing by itself and does not move the cursor.
    assert!(t.visible_images().is_empty());
    assert_eq!(cursor(&t), (0, 0));
    let d = |n| diacritic(n).unwrap();
    let p = PLACEHOLDER;
    feed(&mut t, &format!("\x1b[38;5;42m{p}{}{}{p}\r\n{p}{}{p}\x1b[39m", d(0), d(0), d(1)));
    let v = t.visible_images();
    // Row 0 is one run of two cells, row 1 inherits columns too.
    assert_eq!(v.len(), 2);
    assert!(v.iter().all(|v| v.from_placeholder && v.id == 42));
    assert_eq!((v[0].dest.x, v[0].dest.y, v[0].dest.w, v[0].dest.h), (0, 0, 20, 20));
    assert_eq!((v[1].dest.y, v[1].src.y, v[1].src.h), (20, 20.0, 20.0));
    // Placeholders are text: they reflow and scroll like it.
    assert_eq!(t.row_text(0).chars().next(), Some(p));
}

#[test]
fn relative_placements() {
    let mut t = term(20, 10);
    feed(&mut t, &apc("a=t,f=24,s=10,v=20,i=1,q=2", &[0; 600]));
    feed(&mut t, &apc("a=t,f=24,s=10,v=20,i=2,q=2", &[0; 600]));
    feed(&mut t, "\x1b[3;3H");
    feed(&mut t, &apc("a=p,i=1,p=1,q=2", b""));
    feed(&mut t, &apc("a=p,i=2,p=1,P=1,Q=1,H=2,V=1", b""));
    assert_eq!(out(&mut t), "\x1b_Gi=2,p=1;OK\x1b\\");
    let v = t.visible_images();
    let child = v.iter().find(|v| v.id == 2).unwrap();
    assert_eq!((child.dest.x, child.dest.y), (40, 60));
    // A cycle is refused.
    feed(&mut t, &apc("a=p,i=1,p=1,P=2,Q=1", b""));
    assert!(out(&mut t).contains("ECYCLE"));
    // Deleting the parent deletes the child.
    feed(&mut t, &apc("a=d,d=i,i=1", b""));
    assert!(t.images().placements().is_empty());
}

#[test]
fn compressed_png_and_files() {
    let mut t = term(20, 10);
    let z = miniz_oxide::deflate::compress_to_vec_zlib(&rgba(3, 3, [5, 6, 7, 255]), 6);
    feed(&mut t, &apc("a=t,s=3,v=3,o=z,i=1", &z));
    assert_eq!(out(&mut t), "\x1b_Gi=1;OK\x1b\\");
    feed(&mut t, &apc("a=t,f=100,i=2", &png(5, 4, [1, 2, 3, 255])));
    assert_eq!(out(&mut t), "\x1b_Gi=2;OK\x1b\\");
    assert_eq!(t.images().image_by_id(2).unwrap().size(), (5, 4));

    let path = std::env::temp_dir().join(format!("proteus-graphics-{}.png", std::process::id()));
    std::fs::write(&path, png(2, 2, [0, 0, 0, 255])).unwrap();
    let name = path.to_str().unwrap().as_bytes();
    feed(&mut t, &apc("a=t,t=f,f=100,i=3", name));
    assert_eq!(out(&mut t), "\x1b_Gi=3;OK\x1b\\");
    // Remote pane (OSC 7 from another host): file media are refused, opaquely.
    feed(&mut t, "\x1b]7;file://elsewhere.example/home\x07");
    feed(&mut t, &apc("a=t,t=f,f=100,i=4", name));
    assert_eq!(out(&mut t), "\x1b_Gi=4;EBADF:Failed to read image file\x1b\\");
    feed(&mut t, &apc("a=t,t=f,f=100,i=5", b"/etc/shadow"));
    assert_eq!(out(&mut t), "\x1b_Gi=5;EBADF:Failed to read image file\x1b\\");
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn animation_frames_and_playback() {
    let mut t = term(20, 10);
    feed(&mut t, &apc("a=T,f=24,s=2,v=2,i=1,q=2", &[0; 12]));
    feed(&mut t, &apc("a=f,i=1,f=24,s=2,v=2,z=50,q=2", &[255; 12]));
    feed(&mut t, &apc("a=f,i=1,f=24,s=1,v=1,x=1,y=1,c=1,X=1,q=2", &[7, 7, 7]));
    let img = t.images().image_by_id(1).unwrap();
    assert_eq!(img.frame_count(), 3);
    feed(&mut t, &apc("a=a,i=1,r=1,z=30", b""));
    feed(&mut t, &apc("a=a,i=1,s=3,v=2", b""));
    assert_eq!(t.images().image_by_id(1).unwrap().animation_state(), AnimationState::Running);
    let start = Instant::now();
    let next = t.tick_images(start).unwrap();
    assert!(next > start);
    t.tick_images(start + Duration::from_millis(31));
    let img = t.images().image_by_id(1).unwrap();
    assert_eq!(img.current_frame(), 1);
    assert_eq!(&img.pixels().unwrap()[..4], &[255, 255, 255, 255]);
    // Frame 3 = frame 1 with one replaced pixel.
    t.tick_images(start + Duration::from_millis(90));
    let img = t.images().image_by_id(1).unwrap();
    assert_eq!(img.current_frame(), 2);
    assert_eq!(&img.pixels().unwrap()[12..16], &[7, 7, 7, 255]);
    assert_eq!(&img.pixels().unwrap()[..4], &[0, 0, 0, 255]);
    // Client-driven: jump to a frame.
    feed(&mut t, &apc("a=a,i=1,s=1,c=2", b""));
    assert_eq!(t.images().image_by_id(1).unwrap().current_frame(), 1);
    // Compose frame 2 onto frame 1.
    feed(&mut t, &apc("a=c,i=1,r=2,c=1,w=1,h=1,C=1", b""));
    feed(&mut t, &apc("a=d,d=f,i=1", b""));
    assert_eq!(t.images().image_by_id(1).unwrap().frame_count(), 1);
    assert_eq!(&t.images().image_by_id(1).unwrap().pixels().unwrap()[..4], &[255, 255, 255, 255]);
}

#[test]
fn quota_evicts_least_recently_displayed() {
    let graphics = GraphicsConfig { pane_budget: 2 * MIB, ..GraphicsConfig::default() };
    let cfg = Config { graphics, ..Config::default() };
    let mut t = term_with(80, 24, cfg);
    let big = [0u8; 512 * 512 * 3];
    feed(&mut t, &apc("a=T,f=24,s=512,v=512,i=1,q=2,C=1", &big));
    feed(&mut t, &apc("a=T,f=24,s=512,v=512,i=2,q=2,C=1", &big));
    assert!(t.visible_images().iter().all(|v| v.pixels.is_some()));
    feed(&mut t, &apc("a=T,f=24,s=512,v=512,i=3,q=2,C=1", &big));
    assert!(t.images().image_by_id(1).unwrap().is_evicted());
    let v = t.visible_images();
    assert_eq!(v.iter().filter(|v| v.pixels.is_none()).count(), 1);
    assert!(t.images().bytes() <= 2 * MIB);
}

#[test]
fn sixel_image_at_cursor() {
    let mut t = term(20, 10);
    feed(&mut t, "x");
    // 12x12 red square: two bands.
    feed(&mut t, "\x1bP0;1q\"1;1;12;12#1;2;100;0;0#1!12~-!12~\x1b\\");
    let p = &t.images().placements()[0];
    assert_eq!(p.anchor(), (0, 1));
    assert_eq!(p.cells(), (2, 1));
    assert!(p.z() < 0);
    let img = t.images().image(p.image_key()).unwrap();
    assert_eq!(img.size(), (12, 12));
    assert_eq!(&img.pixels().unwrap()[..4], &[255, 0, 0, 255]);
    // Text continues on the line below the image.
    assert_eq!(cursor(&t), (1, 1));
    // Redrawing at the same spot replaces instead of piling up.
    feed(&mut t, "\x1b[1;2H\x1bPq#1!12~\x1b\\");
    assert_eq!(t.images().placements().len(), 1);
    assert_eq!(t.images().len(), 1);
    assert_eq!(out(&mut t), "");
    feed(&mut t, "\x1b[?1;1S\x1b[?2;1S\x1b[c");
    assert_eq!(out(&mut t), "\x1b[?1;0;256S\x1b[?2;0;200;200S\x1b[?62;4;22;52c");
}

#[test]
fn iterm2_inline_images() {
    let mut t = term(40, 10);
    let data = B64.encode(png(30, 10, [0, 0, 255, 255]));
    feed(&mut t, &format!("\x1b]1337;File=name=eA==;inline=1;width=6;preserveAspectRatio=1:{data}\x07"));
    let p = &t.images().placements()[0];
    // 6 cells = 60 px wide, aspect kept: 20 px high = 1 row.
    assert_eq!(p.cells(), (6, 1));
    let v = t.visible_images();
    assert_eq!((v[0].dest.w, v[0].dest.h), (60, 20));
    assert_eq!(cursor(&t), (6, 0));
    // inline=0 (download) is ignored.
    feed(&mut t, &format!("\x1b]1337;File=inline=0:{data}\x07"));
    assert_eq!(t.images().len(), 1);
    // Multipart transfer.
    feed(&mut t, "\r\n\x1b]1337;MultipartFile=inline=1;width=50%\x07");
    let (a, b) = data.split_at(20);
    feed(&mut t, &format!("\x1b]1337;FilePart={a}\x07\x1b]1337;FilePart={b}\x07\x1b]1337;FileEnd\x07"));
    assert_eq!(t.images().len(), 2);
    feed(&mut t, "\x1b]1337;ReportCellSize\x1b\\");
    assert_eq!(out(&mut t), "\x1b]1337;ReportCellSize=20.0;10.0;1.0\x1b\\");
}

#[test]
fn graphics_disabled() {
    let mut cfg = Config::default();
    cfg.graphics.enabled = false;
    let mut t = term_with(20, 5, cfg);
    feed(&mut t, &apc("a=T,f=24,s=1,v=1,i=1", &[0, 0, 0]));
    feed(&mut t, "\x1bPq#1!12~\x1b\\ok");
    assert!(t.images().is_empty());
    assert_eq!(out(&mut t), "");
    assert_eq!(t.row_text(0), "ok");
}

/// A config decoding in the background; the waker counts wake-ups.
fn background() -> (Config, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let woken = Arc::new(AtomicUsize::new(0));
    let w = woken.clone();
    let mut c = Config::default();
    c.graphics.waker = Some(proteus_core::graphics::DecodeWaker(Arc::new(move || {
        w.fetch_add(1, Ordering::SeqCst);
    })));
    (c, woken)
}

/// Feed nothing until `n` decodes have woken us.
fn settle(t: &mut Terminal, woken: &std::sync::atomic::AtomicUsize, n: usize) {
    let start = Instant::now();
    while woken.load(std::sync::atomic::Ordering::SeqCst) < n {
        assert!(start.elapsed() < Duration::from_secs(10), "decode timed out");
        std::thread::sleep(Duration::from_millis(2));
    }
    t.advance(b"");
}

#[test]
fn background_decode_places_first_and_replies_later() {
    let (c, woken) = background();
    let mut t = term_with(20, 10, c);
    feed(&mut t, &apc("a=T,f=100,i=3", &png(30, 40, [1, 2, 3, 255])));
    // Placed with its header's size right away; the cursor moved past it.
    assert_eq!(t.images().placements()[0].cells(), (3, 2));
    assert_eq!(cursor(&t), (3, 1));
    let img = t.images().image_by_id(3).unwrap();
    assert!(img.is_loading() && img.pixels().is_none());
    assert!(t.visible_images()[0].loading);
    assert_eq!(out(&mut t), "");
    settle(&mut t, &woken, 1);
    let img = t.images().image_by_id(3).unwrap();
    assert!(!img.is_loading());
    assert_eq!(&img.pixels().unwrap()[..4], &[1, 2, 3, 255]);
    assert_eq!(out(&mut t), "\x1b_Gi=3;OK\x1b\\");
}

#[test]
fn background_decode_failure_removes_the_image() {
    let (c, woken) = background();
    let mut t = term_with(20, 10, c);
    let mut bad = png(30, 40, [1, 2, 3, 255]);
    bad.truncate(bad.len() / 2);
    feed(&mut t, &apc("a=T,f=100,i=4", &bad));
    settle(&mut t, &woken, 1);
    assert!(t.images().image_by_id(4).is_none());
    assert!(t.images().placements().is_empty());
    assert!(out(&mut t).starts_with("\x1b_Gi=4;EBADPNG"));
}

#[test]
fn background_iterm_gif_animates() {
    use image::codecs::gif::GifEncoder;
    let (c, woken) = background();
    let mut t = term_with(20, 10, c);
    let mut buf = Vec::new();
    {
        let mut enc = GifEncoder::new(&mut buf);
        for color in [[255, 0, 0, 255], [0, 0, 255, 255]] {
            let f = image::Frame::from_parts(
                image::RgbaImage::from_pixel(8, 8, image::Rgba(color)),
                0,
                0,
                image::Delay::from_numer_denom_ms(200, 1),
            );
            enc.encode_frame(f).unwrap();
        }
    }
    feed(&mut t, &format!("\x1b]1337;File=inline=1:{}\x07", B64.encode(&buf)));
    settle(&mut t, &woken, 1);
    let img = t.images().images().next().unwrap();
    assert_eq!(img.frame_count(), 2);
    assert_eq!(img.animation_state(), AnimationState::Running);
}

#[test]
fn frame_commands_wait_for_decodes() {
    let (c, woken) = background();
    let mut t = term_with(20, 10, c);
    feed(&mut t, &apc("a=t,f=100,i=5,q=2", &png(4, 4, [0, 0, 0, 255])));
    // Adding a frame needs the decoded root frame; it runs once that is ready.
    feed(&mut t, &apc("a=f,i=5,f=32,s=4,v=4", &rgba(4, 4, [9, 9, 9, 255])));
    feed(&mut t, &apc("a=f,i=5,f=32,s=4,v=4", &rgba(4, 4, [8, 8, 8, 255])));
    settle(&mut t, &woken, 1);
    assert_eq!(out(&mut t), "\x1b_Gi=5;OK\x1b\\\x1b_Gi=5;OK\x1b\\");
    assert_eq!(t.images().image_by_id(5).unwrap().frame_count(), 3);
}
