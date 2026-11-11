use proteus_core::{Config, Point, Terminal, bidi::Mode, selection::SelectionKind};
fn term(text: &str, cols: usize) -> Terminal {
    let mut t = Terminal::new(cols, 6, Config::default());
    t.advance(text.as_bytes());
    t
}
fn display(t: &Terminal, row: usize) -> String {
    let g = t.grid();
    let line = g.abs_line(row);
    let layout = t.visual_row(line);
    let mut out = String::new();
    let mut last = None;
    for x in 0..t.cols() {
        let col = layout.logical(x);
        if last == Some(col) {
            continue;
        }
        last = Some(col);
        let c = &g.row(row).cells()[col];
        if c.is_spacer() {
            continue;
        }
        out.push(c.ch());
        if let Some(extra) = c.extra() {
            out.extend(extra.chars().filter(|&c| !proteus_core::bidi::is_control(c)));
        }
    }
    out.trim_end().into()
}
#[test]
fn mixed_text_digits_and_combining_marks_use_unicode_order() {
    let t = term("abc אבג 123 xyz", 30);
    assert_eq!(display(&t, 0), "abc 123 גבא xyz");
    let t = term("שלום", 20);
    assert_eq!(display(&t, 0), "םולש");
    assert_eq!(t.row_text(0), "שלום");
    let t = term("שָב", 10);
    assert_eq!(display(&t, 0), "בשָ");
}
#[test]
fn wide_cells_keep_both_halves_and_maps_round_trip() {
    let t = term("אב中גד 👩‍💻", 30);
    let g = t.grid();
    let map = t.visual_row(g.abs_line(0));
    for (x, c) in g.row(0).cells().iter().enumerate() {
        if c.is_spacer() {
            continue;
        }
        assert_eq!(map.logical(map.visual(x)), x);
        if c.width() == 2 {
            assert_eq!(map.logical(map.visual(x) + 1), x);
            assert_eq!(map.visual(x + 1), map.visual(x) + 1);
        }
    }
}
#[test]
fn formatting_at_column_zero_and_soft_wrap_context_are_preserved() {
    let t = term("\u{202e}abcde\u{202c}", 3);
    assert_eq!(display(&t, 0), "cba");
    assert_eq!(display(&t, 1), "ed");
    assert_eq!(t.text_range(Point::new(0, 0), Point::new(1, 1)), "\u{202e}abcde\u{202c}");
    let mut t = term("\u{2067}אבג\u{2069}", 20);
    t.advance(b"\x1b[6n");
    assert_eq!(t.take_pty_output(), b"\x1b[1;4R");
    assert_eq!(t.row_text(0), "\u{2067}אבג\u{2069}");
}
#[test]
fn selection_highlights_visual_cells_but_copies_logical_text() {
    let mut t = term("ab אבג cd", 20);
    let line = t.grid().abs_line(0);
    let map = t.visual_row(line);
    t.start_selection(SelectionKind::Simple, Point::new(line, map.logical(3)));
    t.update_selection(Point::new(line, map.logical(5)));
    assert_eq!(t.selection_text().as_deref(), Some("אבג"));
    t.start_selection(SelectionKind::Block, Point::new(line, map.logical(3)));
    t.update_selection(Point::new(line, map.logical(4)));
    assert_eq!(t.selection_text().as_deref(), Some("בג"));
    let range = t.selection_range().unwrap();
    assert!(range.rect);
    assert_eq!((range.start.col, range.end.col), (3, 4));
}
#[test]
fn modes_resize_and_output_invalidate_presentation_without_mutating_storage() {
    let mut t = term("אבג", 12);
    assert_eq!(display(&t, 0), "גבא");
    t.set_bidi(Mode::Off);
    assert_eq!(display(&t, 0), "אבג");
    t.set_bidi(Mode::Auto);
    t.advance(b"\x1b[?1049h\x1b[H");
    t.advance("אבג".as_bytes());
    assert_eq!(display(&t, 0), "אבג");
    t.set_bidi(Mode::On);
    assert_eq!(display(&t, 0), "גבא");
    t.advance(b"\x1b[?1049l\rXYZ\x1b[K");
    assert_eq!(display(&t, 0), "XYZ");
    t.advance("\rאבגדה".as_bytes());
    t.resize(3, 6);
    assert_eq!(
        t.text_range(Point::new(t.grid().first_line(), 0), Point::new(t.grid().end_line() - 1, 2)).trim_end(),
        "אבגדה"
    );
    assert_eq!(display(&t, 0), "גבא");
    assert_eq!(display(&t, 1), "הד");
}

#[test]
fn cached_layouts_follow_new_output_and_scrollback_keeps_its_layout() {
    let mut t = term("abc", 20);
    let first = t.grid().abs_line(0);
    assert!(!t.visual_row(first).has_rtl());
    t.advance("\r\u{05d0}\u{05d1} x".as_bytes());
    assert!(t.visual_row(first).has_rtl());
    let order = |t: &Terminal| {
        let layout = t.visual_row(first);
        (0..20).map(|x| layout.logical(x)).collect::<Vec<_>>()
    };
    let shown = order(&t);
    t.advance(b"\r\n1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n7");
    assert!(t.grid().screen_y(first).is_none(), "the line scrolled into history");
    assert_eq!(order(&t), shown);
}

#[test]
fn long_soft_wrapped_lines_are_laid_out_in_blocks() {
    let mut t = Terminal::new(10, 4, Config::default());
    let mut text = "\u{05d0}".repeat(10);
    text.push_str(&"a".repeat(10 * 300));
    t.advance(text.as_bytes());
    let first = t.grid().first_line();
    assert!(t.grid().abs_row(first).unwrap().wrapped);
    assert!(t.visual_row(first).has_rtl());
    // Rows far from the RTL text do not pay for resolving it.
    let far = t.grid().end_line() - 1;
    assert!(!t.visual_row(far).has_rtl());
}
