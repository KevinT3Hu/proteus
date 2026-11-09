//! Output a hostile program can send: no replies it controls, no panics,
//! bounded memory.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use proteus_core::event::Event;
use proteus_core::selection::SelectionKind;
use proteus_core::{Config, Point, Terminal};

fn term(cols: usize, lines: usize) -> Terminal {
    let mut t = Terminal::new(cols, lines, Config::default());
    t.set_cell_pixel_size(8, 16);
    t
}

fn reply(t: &mut Terminal, s: &[u8]) -> String {
    t.advance(s);
    String::from_utf8_lossy(&t.take_pty_output()).into_owned()
}

fn apc(ctrl: &str, payload: &[u8]) -> String {
    format!("\x1b_G{ctrl};{}\x1b\\", B64.encode(payload))
}

#[test]
fn xtgettcap_never_echoes_request_bytes() {
    let mut t = term(20, 5);
    let out = reply(&mut t, b"\x1bP+q\x15touch /tmp/pwned\r;544E\x1b\\");
    assert_eq!(out, "\x1bP0+r\x1b\\\x1bP1+r544E=70726F74657573\x1b\\");
    assert!(!out.contains('\r') && !out.contains('\x15'));
}

#[test]
fn kitty_frame_offsets_outside_the_image_are_rejected() {
    let mut t = term(20, 5);
    t.advance(apc("a=t,f=32,s=1,v=1,i=1,q=1", &[1, 2, 3, 4]).as_bytes());
    for ctrl in ["a=f,i=1,x=2,X=1,f=32,s=1,v=1", "a=f,i=1,y=9,f=32,s=1,v=1", "a=f,i=1,x=4294967295,f=32,s=1,v=1"] {
        let out = reply(&mut t, apc(ctrl, &[9, 9, 9, 9]).as_bytes());
        assert!(out.contains("EINVAL"), "{ctrl}: {out:?}");
    }
    assert_eq!(t.images().image_by_id(1).unwrap().frame_count(), 1);
}

#[test]
fn oversized_osc_strings_are_dropped_and_parsing_recovers() {
    let mut t = term(20, 5);
    let mut data = b"\x1b]2;".to_vec();
    data.extend(std::iter::repeat_n(b'x', 64 << 20));
    // In pieces, as from a PTY, then terminated.
    for chunk in data.chunks(8192) {
        t.advance(chunk);
    }
    t.advance(b"\x07after");
    assert_eq!(t.title(), "");
    assert_eq!(t.row_text(0), "after");
    // An unterminated one never grows without bound either; ESC ends it.
    t.advance(b"\x1b]2;");
    for _ in 0..(64 << 20) / 8192 {
        t.advance(&[b'y'; 8192]);
    }
    t.advance(b"\x1b\\\x1b]2;ok\x1b\\");
    assert_eq!(t.title(), "ok");
}

#[test]
fn ris_keeps_the_oversized_osc_guard() {
    let mut t = term(20, 5);
    t.advance(b"\x1bc\x1b]2;");
    for _ in 0..(64 << 20) / 8192 {
        t.advance(&[b'y'; 8192]);
    }
    t.advance(b"\x07after");
    assert_eq!(t.title(), "");
    assert_eq!(t.row_text(0), "after");
}

#[test]
fn ris_keeps_file_media_disabled() {
    let mut t = term(20, 5);
    let path = std::env::temp_dir().join(format!("proteus-hostile-ris-{}.rgba", std::process::id()));
    std::fs::write(&path, [1, 2, 3, 4]).unwrap();
    let name = B64.encode(path.to_str().unwrap());
    let send =
        |t: &mut Terminal, pre: &str| reply(t, format!("{pre}\x1b_Ga=t,t=f,f=32,s=1,v=1,i=1;{name}\x1b\\").as_bytes());
    t.set_local(false);
    let failed = send(&mut t, "");
    assert_ne!(failed, "\x1b_Gi=1;OK\x1b\\");
    assert_eq!(send(&mut t, "\x1bc"), failed);
    t.set_local(true);
    t.advance(b"\x1b]7;file://elsewhere.example/tmp\x07");
    assert_eq!(send(&mut t, "\x1bc"), failed);
    assert!(t.images().image_by_id(1).is_none());
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn large_but_allowed_osc_still_works() {
    let mut t = term(20, 5);
    let text = "z".repeat(1 << 20);
    t.advance(format!("\x1b]52;c;{}\x07", B64.encode(&text)).as_bytes());
    let stored = t.take_events().into_iter().any(|e| matches!(e, Event::ClipboardStore { text: s, .. } if s == text));
    assert!(stored);
    t.advance(b"\x1b]2;title\x1b\\");
    assert_eq!(t.title(), "title");
}

#[test]
fn long_osc8_ids_and_notifications_are_bounded() {
    let mut t = term(20, 2);
    let id = "i".repeat(10_000);
    t.advance(format!("\x1b]8;id={id};https://example.com\x1b\\L\x1b]8;;\x1b\\").as_bytes());
    let cell = &t.grid().row(0).cells[0];
    let link = t.style(cell.style).hyperlink.and_then(|h| t.hyperlink(h)).unwrap();
    assert_eq!(link.uri, "https://example.com");
    assert!(link.id.is_none());
    t.advance(format!("\x1b]777;notify;{};{}\x07", "t".repeat(100_000), "b".repeat(100_000)).as_bytes());
    let events = t.take_events();
    let Some(Event::Notification { title, body }) = events.iter().find(|e| matches!(e, Event::Notification { .. }))
    else {
        panic!("no notification in {events:?}")
    };
    assert!(title.as_ref().unwrap().len() <= 4096 && body.len() <= 4096);
}

#[test]
fn blocks_from_empty_command_lines_are_pruned_with_their_scrollback() {
    let config = Config { scrollback: 100, ..Default::default() };
    let mut t = Terminal::new(20, 5, config);
    // Enter on an empty prompt: A, B, then the next A with no C or D.
    for _ in 0..5000 {
        t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07\r\n");
    }
    assert!(t.blocks().len() <= 110, "{} blocks kept", t.blocks().len());
}

/// Random bytes biased towards control sequences, fed in random chunks with
/// resizes and reads in between. Deterministic, so a failure reproduces.
#[test]
fn random_output_never_panics() {
    let mut rng = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    const PIECES: &[&[u8]] = &[
        b"\x1b[",
        b"\x1b]",
        b"\x1bP",
        b"\x1b_G",
        b"\x1b\\",
        b"\x07",
        b";",
        b"?",
        b"$q",
        b"+q",
        b"a=T,f=32,s=2,v=2,",
        b"a=f,",
        b"a=p,",
        b"a=d,",
        b"a=c,",
        b"a=a,",
        b"i=1,",
        b"x=9,",
        b"X=1,",
        b"c=1,",
        b"r=1,",
        b"U=1,",
        b"133;A",
        b"133;C",
        b"133;D",
        b"8;id=a;http://x",
        b"52;c;",
        b"1337;File=inline=1:",
        b"q#0;2;0;0;0#0!9~-",
        b"\r\n",
        b"\x1b[?1049h",
        b"\x1b[?1049l",
        b"\x1b[2J",
        b"\x1b[5;10r",
        b"\x1bM",
        b"\x1b[3L",
        b"\x1b[4P",
        b"\xe4\xb8\xad",
        b"e\xcc\x81",
        b"\xf0\x9f\x98\x80",
        b"\xff",
        b"AAAA",
        b"999999999",
        b"\x1b[38:2::1:2:3m",
    ];
    let mut t = term(30, 10);
    for round in 0..3000 {
        let mut chunk = Vec::new();
        for _ in 0..(next() % 32) {
            match next() % 4 {
                0 => chunk.push((next() % 256) as u8),
                1 => chunk.extend_from_slice(&(next() % 1000).to_string().into_bytes()),
                _ => chunk.extend_from_slice(PIECES[(next() % PIECES.len() as u64) as usize]),
            }
        }
        t.advance(&chunk);
        t.take_pty_output();
        t.take_events();
        if round % 50 == 0 {
            t.resize(1 + (next() % 60) as usize, 1 + (next() % 20) as usize);
        }
        if round % 20 == 0 {
            let _ = t.visible_images();
            let _ = t.screen_text();
            let line = t.grid().abs_line(0);
            t.start_selection(SelectionKind::Simple, Point::new(line, 0));
            t.update_selection(Point::new(line + next() % 5, (next() % 30) as usize));
            let _ = t.selection_text();
            t.clear_selection();
        }
    }
    let _ = t.scrollback_text();
}

#[test]
fn placements_and_images_are_capped() {
    use proteus_core::graphics::{MAX_IMAGES, MAX_PLACEMENTS};
    let mut t = term(20, 5);
    t.advance(apc("a=t,f=32,s=1,v=1,i=1,q=2", &[1, 2, 3, 4]).as_bytes());
    for _ in 0..MAX_PLACEMENTS + 100 {
        t.advance(b"\x1b_Ga=p,i=1,C=1,q=2\x1b\\");
    }
    assert_eq!(t.images().placements().len(), MAX_PLACEMENTS);
    for i in 2..MAX_IMAGES as u32 + 100 {
        t.advance(apc(&format!("a=t,f=32,s=1,v=1,i={i},q=2"), &[1, 2, 3, 4]).as_bytes());
    }
    assert!(t.images().len() <= MAX_IMAGES);
}

#[test]
fn file_media_need_a_local_pane_and_fail_alike() {
    let mut t = term(20, 5);
    let path = std::env::temp_dir().join(format!("proteus-hostile-{}.rgba", std::process::id()));
    std::fs::write(&path, [1, 2, 3, 4]).unwrap();
    let name = B64.encode(path.to_str().unwrap());
    let send =
        |t: &mut Terminal, file: &str| reply(t, format!("\x1b_Ga=t,t=f,f=32,s=1,v=1,i=1;{file}\x1b\\").as_bytes());
    assert_eq!(send(&mut t, &name), "\x1b_Gi=1;OK\x1b\\");
    // Missing files, short files and bad data get the same answer.
    let missing = B64.encode("/nonexistent/x");
    assert_eq!(send(&mut t, &missing), send(&mut t, &B64.encode(path.with_extension("none").to_str().unwrap())));
    let failed = send(&mut t, &missing);
    let short = reply(&mut t, format!("\x1b_Ga=t,t=f,f=32,s=9,v=9,i=1;{name}\x1b\\").as_bytes());
    assert_eq!(short, failed);
    // A remote program in the foreground (or an OSC 7 cwd elsewhere) cannot read files.
    t.set_local(false);
    assert_eq!(send(&mut t, &name), failed);
    t.set_local(true);
    t.advance(b"\x1b]7;file://elsewhere.example/tmp\x07");
    assert_eq!(send(&mut t, &name), failed);
    std::fs::remove_file(&path).unwrap();
}
