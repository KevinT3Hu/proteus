use proteus_core::cell::CellFlags;
use proteus_core::color::{Color, Rgb};
use proteus_core::event::{ClipboardKind, Event};
use proteus_core::style::{Attrs, Underline};
use proteus_core::{BlockState, Config, KeyboardFlags, Point, TermMode, Terminal};

fn term(cols: usize, lines: usize) -> Terminal {
    Terminal::new(cols, lines, Config::default())
}

fn feed(t: &mut Terminal, s: &str) {
    t.advance(s.as_bytes());
}

fn reply(t: &mut Terminal, s: &str) -> String {
    feed(t, s);
    String::from_utf8(t.take_pty_output()).unwrap()
}

#[test]
fn print_and_autowrap() {
    let mut t = term(5, 3);
    feed(&mut t, "abcdefg");
    assert_eq!(t.row_text(0), "abcde");
    assert_eq!(t.row_text(1), "fg");
    assert!(t.grid().row(0).wrapped);
    assert_eq!((t.cursor().x, t.cursor().y), (2, 1));
}

#[test]
fn pending_wrap_then_cr() {
    let mut t = term(5, 3);
    feed(&mut t, "abcde\r\nx");
    assert_eq!(t.screen_text(), "abcde\nx");
    assert!(!t.grid().row(0).wrapped);
}

#[test]
fn wide_char_wraps_with_spacer() {
    let mut t = term(5, 3);
    feed(&mut t, "abcd中");
    assert_eq!(t.row_text(0), "abcd");
    assert!(t.grid().row(0).cells()[4].flags().contains(CellFlags::LEADING_WIDE_SPACER));
    assert_eq!(t.row_text(1), "中");
    assert!(t.grid().row(1).cells()[0].flags().contains(CellFlags::WIDE));
    assert!(t.grid().row(1).cells()[1].flags().contains(CellFlags::WIDE_SPACER));
    assert_eq!(t.cursor().x, 2);
}

#[test]
fn overwrite_half_of_wide_char() {
    let mut t = term(6, 2);
    feed(&mut t, "中文\x1b[1;2Hx");
    // Overwriting the spacer of 中 blanks its head.
    assert_eq!(t.row_text(0), " x文");
}

#[test]
fn combining_marks_join_cell() {
    let mut t = term(10, 2);
    feed(&mut t, "e\u{301}x");
    let row = &t.grid().row(0).cells();
    assert_eq!(row[0].ch(), 'e');
    assert_eq!(row[0].extra(), Some("\u{301}"));
    assert_eq!(row[1].ch(), 'x');
}

#[test]
fn grapheme_mode_vs16_widens() {
    let mut t = term(10, 2);
    feed(&mut t, "\x1b[?2027h\u{2764}\u{fe0f}x");
    let row = &t.grid().row(0).cells();
    assert!(row[0].flags().contains(CellFlags::WIDE));
    assert_eq!(row[2].ch(), 'x');
    // ZWJ family is one cluster.
    feed(&mut t, "\r\n\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}y");
    let row = &t.grid().row(1).cells();
    assert_eq!(row[0].extra(), Some("\u{200D}\u{1F469}\u{200D}\u{1F467}"));
    assert_eq!(row[2].ch(), 'y');
}

#[test]
fn cursor_movement_and_erase() {
    let mut t = term(10, 4);
    feed(&mut t, "0123456789\x1b[2;3Habc\x1b[1;5H\x1b[K\x1b[2;1H\x1b[2P");
    assert_eq!(t.row_text(0), "0123");
    assert_eq!(t.row_text(1), "abc");
    feed(&mut t, "\x1b[2J\x1b[H");
    assert_eq!(t.screen_text(), "");
    assert_eq!((t.cursor().x, t.cursor().y), (0, 0));
}

#[test]
fn insert_delete_chars_and_ech() {
    let mut t = term(8, 2);
    feed(&mut t, "abcdef\x1b[1;2H\x1b[2@");
    assert_eq!(t.row_text(0), "a  bcdef");
    feed(&mut t, "\x1b[3X");
    assert_eq!(t.row_text(0), "a   cdef");
    feed(&mut t, "\x1b[4h12\x1b[4l");
    assert_eq!(t.row_text(0), "a12   cd");
}

#[test]
fn scroll_region_and_scrollback() {
    let mut t = term(4, 4);
    feed(&mut t, "1\r\n2\r\n3\r\n4");
    // Region rows 2-3: scrolling inside it must not touch history or rows 1/4.
    feed(&mut t, "\x1b[2;3r\x1b[3;1H\n\n");
    assert_eq!(t.screen_text(), "1\n\n\n4");
    assert_eq!(t.grid().history_len(), 0);
    feed(&mut t, "\x1b[r\x1b[4;1H\n\n");
    assert_eq!(t.grid().history_len(), 2);
    assert_eq!(t.grid().abs_row(0).unwrap().text(0..4).trim_end(), "1");
}

#[test]
fn reverse_index_at_top_scrolls_down() {
    let mut t = term(4, 3);
    feed(&mut t, "a\r\nb\x1b[H\x1bM");
    assert_eq!(t.screen_text(), "\na\nb");
}

#[test]
fn alt_screen_1049_restores() {
    let mut t = term(10, 3);
    feed(&mut t, "shell$ \x1b[?1049h\x1b[Hfullscreen");
    assert!(t.is_alt_screen());
    assert_eq!(t.row_text(0), "fullscreen");
    feed(&mut t, "\x1b[?1049l");
    assert!(!t.is_alt_screen());
    assert_eq!(t.row_text(0), "shell$");
    assert_eq!(t.cursor().x, 7);
}

#[test]
fn alt_screen_has_no_scrollback() {
    let mut t = term(4, 2);
    feed(&mut t, "\x1b[?1049h1\r\n2\r\n3\r\n4");
    assert_eq!(t.grid().history_len(), 0);
    feed(&mut t, "\x1b[?1049l");
    assert_eq!(t.grid().history_len(), 0);
}

#[test]
fn sgr_attributes_and_colors() {
    let mut t = term(20, 2);
    feed(&mut t, "\x1b[1;3;4:3;38;2;10;20;30;48;5;200;58:2::1:2:3mX\x1b[0mY");
    let s = t.style(t.grid().row(0).cells()[0].style).clone();
    assert!(s.attrs.contains(Attrs::BOLD | Attrs::ITALIC));
    assert_eq!(s.underline, Underline::Curly);
    assert_eq!(s.fg, Color::Rgb(Rgb::new(10, 20, 30)));
    assert_eq!(s.bg, Color::Indexed(200));
    assert_eq!(s.underline_color, Color::Rgb(Rgb::new(1, 2, 3)));
    let y = t.style(t.grid().row(0).cells()[1].style);
    assert_eq!(*y, Default::default());
    feed(&mut t, "\x1b[38:5:17;91mZ");
    let z = t.style(t.grid().row(0).cells()[2].style);
    assert_eq!(z.fg, Color::Indexed(9));
}

#[test]
fn background_color_erase() {
    let mut t = term(4, 2);
    feed(&mut t, "\x1b[44m\x1b[2J");
    let s = t.style(t.grid().row(1).cells()[3].style);
    assert_eq!(s.bg, Color::Indexed(4));
}

#[test]
fn query_replies() {
    let mut t = term(10, 5);
    assert_eq!(reply(&mut t, "\x1b[3;4H\x1b[6n"), "\x1b[3;4R");
    assert_eq!(reply(&mut t, "\x1b[5n"), "\x1b[0n");
    assert_eq!(reply(&mut t, "\x1b[c"), "\x1b[?62;4;22;52c");
    assert!(reply(&mut t, "\x1b[>q").starts_with("\x1bP>|proteus "));
    assert_eq!(reply(&mut t, "\x1b[?2004$p"), "\x1b[?2004;2$y");
    assert_eq!(reply(&mut t, "\x1b[?2004h\x1b[?2004$p"), "\x1b[?2004;1$y");
    assert_eq!(reply(&mut t, "\x1b[?9999$p"), "\x1b[?9999;0$y");
    assert_eq!(reply(&mut t, "\x1b[18t"), "\x1b[8;5;10t");
    assert_eq!(reply(&mut t, "\x1b]10;?\x07"), "\x1b]10;rgb:d8d8/d8d8/d8d8\x07");
    assert_eq!(reply(&mut t, "\x1b]4;1;#102030\x1b\\\x1b]4;1;?\x1b\\"), "\x1b]4;1;rgb:1010/2020/3030\x1b\\");
    assert_eq!(reply(&mut t, "\x1bP$q\"q\x1b\\"), "\x1bP0$r\x1b\\");
    assert_eq!(reply(&mut t, "\x1bP$qr\x1b\\"), "\x1bP1$r1;5r\x1b\\");
    // XTGETTCAP "TN"
    assert_eq!(reply(&mut t, "\x1bP+q544E\x1b\\"), "\x1bP1+r544E=70726F74657573\x1b\\");
}

#[test]
fn kitty_keyboard_flag_stack() {
    let mut t = term(10, 2);
    feed(&mut t, "\x1b[>1u");
    assert_eq!(t.keyboard_flags(), KeyboardFlags::DISAMBIGUATE);
    feed(&mut t, "\x1b[>31u");
    assert_eq!(reply(&mut t, "\x1b[?u"), "\x1b[?31u");
    feed(&mut t, "\x1b[=1;3u");
    assert_eq!(t.keyboard_flags().bits(), 30);
    feed(&mut t, "\x1b[<u");
    assert_eq!(t.keyboard_flags(), KeyboardFlags::DISAMBIGUATE);
    // Alt screen has its own stack.
    feed(&mut t, "\x1b[?1049h");
    assert_eq!(t.keyboard_flags(), KeyboardFlags::empty());
    feed(&mut t, "\x1b[?1049l\x1b[<10u");
    assert_eq!(t.keyboard_flags(), KeyboardFlags::empty());
}

#[test]
fn modes_toggle() {
    let mut t = term(10, 2);
    feed(&mut t, "\x1b[?1h\x1b[?1000h\x1b[?1006h\x1b[?25l");
    let m = t.mode();
    assert!(m.contains(TermMode::APP_CURSOR | TermMode::MOUSE_NORMAL | TermMode::MOUSE_SGR));
    assert!(!m.contains(TermMode::SHOW_CURSOR));
    feed(&mut t, "\x1b[?1003h");
    assert!(!t.mode().contains(TermMode::MOUSE_NORMAL));
    assert!(t.mode().contains(TermMode::MOUSE_ANY_MOTION));
}

#[test]
fn title_and_stack() {
    let mut t = term(10, 2);
    feed(&mut t, "\x1b]2;one\x07\x1b[22t\x1b]0;two\x1b\\");
    assert_eq!(t.title(), "two");
    feed(&mut t, "\x1b[23t");
    assert_eq!(t.title(), "one");
    let ev = t.take_events();
    assert!(ev.contains(&Event::TitleChanged("two".into())));
}

#[test]
fn osc7_osc52_osc8() {
    let mut t = term(20, 2);
    feed(&mut t, "\x1b]7;file://host/home/u/a%20b\x07");
    assert_eq!(t.cwd().unwrap().path, "/home/u/a b");
    feed(&mut t, "\x1b]52;c;aGVsbG8=\x07\x1b]52;p;?\x1b\\");
    let ev = t.take_events();
    assert!(ev.contains(&Event::ClipboardStore { kind: ClipboardKind::Clipboard, text: "hello".into() }));
    assert!(ev.iter().any(|e| matches!(e, Event::ClipboardLoad { kind: ClipboardKind::Primary, .. })));
    feed(&mut t, "\x1b]8;id=x;https://example.com\x1b\\link\x1b]8;;\x1b\\ no");
    let cell = &t.grid().row(0).cells()[0];
    let link = t.style(cell.style).hyperlink.and_then(|id| t.hyperlink(id)).unwrap();
    assert_eq!(link.uri, "https://example.com");
    assert_eq!(link.id.as_deref(), Some("x"));
    assert!(t.style(t.grid().row(0).cells()[5].style).hyperlink.is_none());
}

#[test]
fn notifications_are_rate_limited() {
    let mut t = term(10, 2);
    for i in 0..10 {
        feed(&mut t, &format!("\x1b]9;msg {i}\x07"));
    }
    feed(&mut t, "\x1b]9;4;1;50\x07");
    let n = t.take_events().iter().filter(|e| matches!(e, Event::Notification { .. })).count();
    assert_eq!(n, Config::default().notify_burst);
}

/// Simulate a shell with integration: prompt, typed command, output, exit status.
fn run_cmd(t: &mut Terminal, cmd: &str, output: &str, status: i32) {
    feed(t, &format!("\x1b]133;A\x07$ \x1b]133;B\x07{cmd}\r\n\x1b]133;C\x07{output}\x1b]133;D;{status}\x07"));
}

#[test]
fn shell_integration_blocks() {
    let mut t = term(20, 10);
    run_cmd(&mut t, "echo hi", "hi\r\n", 0);
    run_cmd(&mut t, "false", "", 1);
    feed(&mut t, "\x1b]133;A\x07$ \x1b]133;B\x07");
    let blocks: Vec<_> = t.blocks().iter().cloned().collect();
    assert_eq!(blocks.len(), 3);
    assert_eq!(blocks[0].command.as_deref(), Some("echo hi"));
    assert_eq!(blocks[0].exit_code, Some(0));
    assert_eq!(blocks[0].output_lines(99), Some(1..2));
    assert!(blocks[1].failed());
    assert_eq!(blocks[1].output_lines(99), Some(3..3));
    assert_eq!(blocks[2].state, BlockState::Input);
    let id = blocks[0].id;
    assert_eq!(t.block_text(id, proteus_core::BlockPart::Output).as_deref(), Some("hi"));
    assert_eq!(t.blocks().prev_prompt(3), Some(2));
    assert_eq!(t.blocks().next_prompt(0), Some(2));
    let ev = t.take_events();
    assert!(ev.iter().any(|e| matches!(e, Event::CommandFinished { exit_code: Some(1), .. })));
}

#[test]
fn explicit_cmdline_wins() {
    let mut t = term(20, 5);
    feed(&mut t, "\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C;cmdline_url=ls%20-la\x07");
    assert_eq!(t.blocks().last().unwrap().command.as_deref(), Some("ls -la"));
    assert_eq!(t.blocks().last().unwrap().state, BlockState::Running);
}

#[test]
fn cmdlines_with_control_chars_are_dropped() {
    let mut t = term(30, 5);
    for bad in ["ls%0Arm%20-rf%20~", "ls%0D", "ls%09-l", "ls%7F", "ls%C2%85x", "ls%1B[8m"] {
        feed(&mut t, &format!("\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C;cmdline_url={bad}\x07"));
        let b = t.blocks().last().unwrap();
        assert_eq!((b.command.as_deref(), b.state), (Some("ls"), BlockState::Running), "{bad}");
    }
    feed(&mut t, "\x1b]133;A\x07$ \x1b]633;E;echo \u{85}rm\x07\x1b]133;C\x07");
    let b = t.blocks().last().unwrap();
    assert_eq!((b.command.as_deref(), b.state), (None, BlockState::Running));
    feed(&mut t, "\x1b]133;A\x07$ \x1b]633;E;echo a\\x3bb \u{e9}\x07\x1b]133;C\x07");
    assert_eq!(t.blocks().last().unwrap().command.as_deref(), Some("echo a;b \u{e9}"));
    feed(&mut t, "\x1b]133;A\x07$ \x1b]133;C;cmdline_url=git%20log%20%27%C3%A9%27\x07");
    assert_eq!(t.blocks().last().unwrap().command.as_deref(), Some("git log '\u{e9}'"));
}

#[test]
fn rep_is_bounded_and_needs_a_preceding_char() {
    let mut t = term(10, 5);
    feed(&mut t, "a\x1b[3b\x1b[3b");
    assert_eq!(t.row_text(0), "aaaa");
    feed(&mut t, "\r\nb\r\x1b[5b");
    assert_eq!(t.row_text(1), "b");
    feed(&mut t, "\x1b[2;3r\x1b[2Hc\x1b[65535b");
    assert_eq!(t.screen_text(), "aaaa\ncccccccccc\nc");
}

#[test]
fn ris_on_alt_screen_keeps_line_numbers_increasing() {
    let mut t = Terminal::new(10, 3, Config { scrollback: 100, ..Default::default() });
    feed(&mut t, &"x\r\n".repeat(50));
    feed(&mut t, "\x1b[?1049h");
    let end = t.primary_grid().end_line();
    feed(&mut t, "\x1bc");
    assert!(!t.is_alt_screen());
    assert!(t.grid().first_line() >= end, "{} < {end}", t.grid().first_line());
}

#[test]
fn resize_reflows_and_remaps_blocks() {
    let mut t = term(10, 5);
    run_cmd(&mut t, "x", "0123456789abcde\r\n", 0);
    feed(&mut t, "\x1b]133;A\x07$ ");
    assert_eq!(t.row_text(1), "0123456789");
    assert_eq!(t.row_text(2), "abcde");
    t.resize(20, 5);
    assert_eq!(t.row_text(1), "0123456789abcde");
    assert_eq!(t.row_text(2), "$");
    assert_eq!((t.cursor().x, t.cursor().y), (2, 2));
    let b: Vec<_> = t.blocks().iter().cloned().collect();
    assert_eq!(b[1].prompt_line, 2);
    assert_eq!(b[0].output_lines(99), Some(1..2));
    t.resize(5, 5);
    assert_eq!(t.row_text(t.cursor().y), "$");
    assert_eq!(t.cursor().x, 2);
}

#[test]
fn resize_in_alt_screen_keeps_primary() {
    let mut t = term(10, 3);
    feed(&mut t, "0123456789ab\x1b[?1049hTUI");
    t.resize(6, 3);
    feed(&mut t, "\x1b[?1049l");
    assert_eq!(t.row_text(0), "012345");
    assert_eq!(t.row_text(1), "6789ab");
}

#[test]
fn search_across_wrapped_lines() {
    use proteus_core::search::{SearchOptions, Searcher};
    let mut t = term(5, 4);
    feed(&mut t, "hello world\r\nHELLO");
    let s = Searcher::new("lowo", &SearchOptions::default()).unwrap();
    assert!(s.find_all(t.grid()).is_empty());
    let s = Searcher::new("o w", &SearchOptions::default()).unwrap();
    let m = s.find_all(t.grid());
    assert_eq!(m.len(), 1);
    assert_eq!((m[0].start, m[0].end), (Point::new(0, 4), Point::new(1, 1)));
    let s = Searcher::new("hello", &SearchOptions::default()).unwrap();
    assert_eq!(s.find_all(t.grid()).len(), 2);
    let s = Searcher::new("HELLO", &SearchOptions::default()).unwrap();
    assert_eq!(s.find_all(t.grid()).len(), 1);
}

#[test]
fn hints_on_screen() {
    use proteus_core::hints::{HintKind, builtin_patterns, find_hints};
    let mut t = term(80, 4);
    feed(&mut t, "see https://example.com/a_(b). at src/main.rs:12:3\r\ncommit 3f2a9c1 from 10.0.0.1 color #ff8800");
    let hints = find_hints(t.grid(), 0..4, &builtin_patterns(), "asdf");
    let got: Vec<_> = hints.iter().map(|h| (h.kind.clone(), h.text.as_str())).collect();
    assert_eq!(
        got,
        vec![
            (HintKind::Url, "https://example.com/a_(b)"),
            (HintKind::Path, "src/main.rs:12:3"),
            (HintKind::GitHash, "3f2a9c1"),
            (HintKind::Ipv4, "10.0.0.1"),
            (HintKind::HexColor, "#ff8800"),
        ]
    );
    assert!(hints.iter().all(|h| h.label.len() == 2));
}

#[test]
fn selection_word_line_semantic() {
    use proteus_core::selection::SelectionKind;
    let mut t = term(10, 6);
    run_cmd(&mut t, "ls", "foo bar\r\nbaz\r\n", 0);
    t.start_selection(SelectionKind::Word, Point::new(1, 5));
    assert_eq!(t.selection_text().as_deref(), Some("bar"));
    t.start_selection(SelectionKind::Line, Point::new(1, 0));
    assert_eq!(t.selection_text().as_deref(), Some("foo bar"));
    t.start_selection(SelectionKind::Semantic, Point::new(0, 0));
    assert_eq!(t.selection_text().as_deref(), Some("foo bar\nbaz"));
    t.start_selection(SelectionKind::Block, Point::new(1, 1));
    t.update_selection(Point::new(2, 2));
    assert_eq!(t.selection_text().as_deref(), Some("oo\naz"));
}

#[test]
fn full_reset() {
    let mut t = term(10, 3);
    feed(&mut t, "\x1b[?1049h\x1b[31mabc\x1bc");
    assert!(!t.is_alt_screen());
    assert_eq!(t.screen_text(), "");
    assert_eq!(t.cursor().style, Default::default());
}

#[test]
fn grapheme_clustering_default_from_config() {
    let cfg = proteus_core::Config { grapheme_clustering: true, ..Default::default() };
    let mut t = proteus_core::Terminal::new(10, 2, cfg);
    assert!(t.mode().contains(proteus_core::TermMode::GRAPHEME_CLUSTERING));
    // Family emoji joins one wide cell; survives RIS.
    t.advance("\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}x".as_bytes());
    assert_eq!(t.cursor().x, 3);
    t.advance(b"\x1bc");
    assert!(t.mode().contains(proteus_core::TermMode::GRAPHEME_CLUSTERING));
    let t = proteus_core::Terminal::new(10, 2, proteus_core::Config::default());
    assert!(!t.mode().contains(proteus_core::TermMode::GRAPHEME_CLUSTERING));
}

#[test]
fn insert_before_wide_char_preserves_pair() {
    let mut t = proteus_core::Terminal::new(8, 2, Default::default());
    t.advance("中x\x1b[1G\x1b[@".as_bytes());
    assert_eq!(t.row_text(0), " 中x");
    t.advance("\x1b[1G\x1b[4hA".as_bytes());
    assert_eq!(t.row_text(0), "A 中x");
}

#[test]
fn theme_reload_preserves_osc_and_reset_restores_configured_palette() {
    use proteus_core::color::Palette;
    let mut t = term(8, 2);
    feed(&mut t, "hello\x1b]11;#123456\x07\x1b]4;4;#abcdef\x07");
    let fg = Rgb::new(10, 20, 30);
    let bg = Rgb::new(240, 230, 220);
    let mut palette = Palette::new([fg; 16], fg, bg, Some(fg));
    palette.set_base_indexed(255, bg);
    t.take_damage();
    t.rebase_palette(&palette);
    assert!(t.take_damage().full);
    assert_eq!(t.row_text(0), "hello");
    assert_eq!(t.palette().foreground(), fg);
    assert_eq!(t.palette().background(), Rgb::new(0x12, 0x34, 0x56));
    assert_eq!(t.palette().indexed(4), Rgb::new(0xab, 0xcd, 0xef));
    feed(&mut t, "\x1b]111\x07\x1b]104;4\x07");
    assert_eq!(t.palette().background(), bg);
    assert_eq!(t.palette().indexed(4), fg);
    feed(&mut t, "\x1bc");
    assert_eq!(t.palette().indexed(255), bg);
    assert_eq!(t.palette().cursor(), Some(fg));
}

#[test]
fn style_compaction_keeps_cell_styles() {
    let mut t = Terminal::new(64, 4, Config { scrollback: 10, ..Config::default() });
    // More distinct true colors than the table holds before compacting.
    let mut s = String::new();
    for i in 0..70_000u32 {
        let (r, g, b) = (i >> 16, (i >> 8) & 0xff, i & 0xff);
        s.push_str(&format!("\x1b[38;2;{r};{g};{b}m#"));
    }
    s.push_str("\x1b[0;1;48;5;4mX\x1b[0m");
    feed(&mut t, &s);
    assert!(t.styles().len() < 70_000);
    let (x, y) = (t.cursor().x - 1, t.cursor().y);
    let st = t.style(t.grid().row(y).cells()[x].style);
    assert_eq!((st.attrs, st.bg), (Attrs::BOLD, Color::Indexed(4)));
    let prev = t.style(t.grid().row(y).cells()[x - 1].style);
    assert_eq!(prev.fg, Color::Rgb(Rgb::new(1, 17, 111)));
    // History keeps only the text of its rows, never their trailing blanks.
    feed(&mut t, "\x1b[0m\r\nshort\r\n\r\n\r\n\r\n");
    let row = (t.grid().first_line()..t.grid().end_line())
        .filter_map(|l| t.grid().abs_row(l))
        .find(|r| r.text(0..64).trim_end() == "short")
        .unwrap();
    assert_eq!(row.len(), 5);
}

/// Ground-state text is handled outside `vte` and printable ASCII is written
/// a run at a time. Fed byte by byte, multi-byte characters go through `vte`
/// and each ASCII character through `put_char` on its own; random chunks mix
/// both. All must leave the same screen and replies.
#[test]
fn bulk_text_matches_byte_by_byte_parsing() {
    let mut rng = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    const PIECES: &[&[u8]] = &[
        b"hello world ",
        b"abcdefghijklmnopqrstuvwxyz",
        b"x",
        b"\r\n",
        b"\n",
        b"\r",
        b"\x08",
        b"\t",
        b"\x7f",
        "中文".as_bytes(),
        "e\u{301}".as_bytes(),
        b"\xe4\xb8",
        b"\xff",
        b"\x9b",
        b"\x1b[31m",
        b"\x1b[0m",
        b"\x1b[4h",
        b"\x1b[4l",
        b"\x1b[?7l",
        b"\x1b[?7h",
        b"\x1b[3D",
        b"\x1b[5C",
        b"\x1b[2;7H",
        b"\x1b[K",
        b"\x1b[2P",
        b"\x1b[3@",
        b"\x1b[3b",
        b"\x1b[1\n2C",
        b"\x1b[5\x18x",
        b"\x1b]2;title\x07",
        b"\x1b]2;t\x1b\\",
        b"\x1bP$qm\x1b\\",
        b"\x1b[6n",
        b"\x1b7",
        b"\x1b8",
        b"\x1b(0",
        b"\x1b(B",
        b"\x0e",
        b"\x0f",
        b"\x1b[?2027h",
        "\u{200f}".as_bytes(),
        "\r\u{200f}".as_bytes(),
    ];
    let state = |t: &mut Terminal| {
        let rows: Vec<_> = (0..4).map(|y| t.grid().row(y).clone()).collect();
        let c = t.cursor();
        (rows, (c.x, c.y, c.pending_wrap), t.take_pty_output())
    };
    for _ in 0..300 {
        let mut input = Vec::new();
        for _ in 0..80 {
            input.extend_from_slice(PIECES[(next() % PIECES.len() as u64) as usize]);
        }
        let mut whole = Terminal::new(13, 4, Config::default());
        whole.advance(&input);
        let mut bytes = Terminal::new(13, 4, Config::default());
        for b in &input {
            bytes.advance(std::slice::from_ref(b));
        }
        let mut chunks = Terminal::new(13, 4, Config::default());
        let mut rest = &input[..];
        while !rest.is_empty() {
            let n = (next() % 9 + 1).min(rest.len() as u64) as usize;
            chunks.advance(&rest[..n]);
            rest = &rest[n..];
        }
        let expected = state(&mut bytes);
        assert_eq!(state(&mut whole), expected, "{:?}", String::from_utf8_lossy(&input));
        assert_eq!(state(&mut chunks), expected, "{:?}", String::from_utf8_lossy(&input));
    }
}
