//! Exercise the parser with PTY-like fragmented streams, independent of read boundaries.
use proteus_core::{Config, Terminal};

fn transcript() -> Vec<u8> {
    concat!(
        "\x1b]2;工作 terminal\x1b\\",
        "\x1b]7;file://localhost/tmp/project\x07",
        "\x1b[?2026h\x1b[?2004h\x1b[?1004h",
        "\x1b[38;2;120;80;200mhello 中 e\u{301} 🌍\x1b[0m\r\n",
        "\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\\r\n",
        "\x1b[?1049h\x1b[2J\x1b[Heditor\x1b[3;5H中\x1b[1;4mstatus\x1b[0m",
        "\x1b[2;1H\x1b[2L\x1b[M\x1b[?1049l",
        "\x1b[6n\x1b[c\x1b[?2026l",
        "\x1b]52;c;dGVzdA==\x07done\r\n"
    )
    .as_bytes()
    .to_vec()
}

#[test]
fn pty_chunk_boundaries_do_not_change_terminal_state() {
    let bytes = transcript();
    let mut reference = Terminal::new(40, 8, Config::default());
    reference.advance(&bytes);
    let expected_events = reference.take_events();
    let expected_replies = reference.take_pty_output();
    for chunk in 1..=bytes.len() {
        let mut t = Terminal::new(40, 8, Config::default());
        for part in bytes.chunks(chunk) {
            t.advance(part);
        }
        assert_eq!(t.screen_text(), reference.screen_text(), "chunk {chunk}");
        assert_eq!(t.cursor().x, reference.cursor().x);
        assert_eq!(t.cursor().y, reference.cursor().y);
        assert_eq!(t.mode(), reference.mode());
        assert_eq!(t.take_events(), expected_events, "events at chunk {chunk}");
        assert_eq!(t.take_pty_output(), expected_replies, "replies at chunk {chunk}");
        for row in 0..t.lines() {
            assert_eq!(t.grid().row(row), reference.grid().row(row), "cells at chunk {chunk}, row {row}");
        }
    }
}

#[test]
fn repeated_resize_preserves_unicode_logical_text() {
    use proteus_core::Point;
    let mut t = Terminal::new(80, 24, Config::default());
    let input = "hello 中 e\u{301} 🌍 repeated text ".repeat(12);
    t.advance(input.as_bytes());
    let extract = |t: &Terminal| {
        let g = t.grid();
        t.text_range(Point::new(g.first_line(), 0), Point::new(g.end_line() - 1, t.cols() - 1)).trim_end().to_owned()
    };
    let expected = extract(&t);
    for width in [20, 120, 5, 80, 13, 40, 80] {
        t.resize(width, 24);
        assert_eq!(extract(&t), expected, "resize to {width}");
    }
}

#[test]
fn export_keeps_primary_history_and_excludes_fullscreen_ui() {
    let mut t = Terminal::new(8, 3, Config::default());
    t.advance(b"first\r\nsecond\r\nthird\r\nfourth");
    let expected = t.scrollback_text();
    assert!(expected.starts_with("first\nsecond\n"));
    t.advance(b"\x1b[?1049h\x1b[2Jeditor");
    assert_eq!(t.scrollback_text(), expected);
    assert!(!t.scrollback_text().contains("editor"));
    t.advance(b"\x1b[?1049l");
    t.resize(4, 3);
    assert_eq!(t.scrollback_text(), expected);
}

#[test]
fn fragmented_invalid_utf8_recovers_at_the_next_printable_character() {
    for input in [b"x\xe4\xb8 Y".as_slice(), b"x\xf0\x9f\x8c Y", b"x\xc3 Y"] {
        let mut reference = Terminal::new(40, 3, Config::default());
        reference.advance(input);
        for chunk in 1..=input.len() {
            let mut t = Terminal::new(40, 3, Config::default());
            for bytes in input.chunks(chunk) {
                t.advance(bytes);
            }
            assert_eq!(t.screen_text(), reference.screen_text());
            assert!(t.screen_text().ends_with(" Y"));
        }
    }
}

/// Run explicitly with `cargo test -p proteus-core --test replay -- --ignored --nocapture`.
/// Reports measurements, with no machine-dependent pass/fail timing threshold.
#[test]
#[ignore]
fn sustained_output_measurement() {
    let line = "\x1b[32mproject/src/main.rs\x1b[0m:123: hello 世界 e\u{301}\r\n";
    let bytes = line.repeat(100_000);
    let mut t = Terminal::new(120, 40, Config::default());
    let start = std::time::Instant::now();
    for chunk in bytes.as_bytes().chunks(8192) {
        t.advance(chunk);
    }
    let elapsed = start.elapsed();
    eprintln!(
        "{} bytes in {:?}: {:.2} MiB/s; {} retained rows",
        bytes.len(),
        elapsed,
        bytes.len() as f64 / elapsed.as_secs_f64() / 1048576.0,
        t.grid().end_line() - t.grid().first_line()
    );
    assert!(t.screen_text().contains("世界"));
}
