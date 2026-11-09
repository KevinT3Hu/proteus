//! The terminal state machine: VT parser callbacks, screens, cursor and modes.

mod graphics;

use std::collections::VecDeque;
use std::ops::Range;
use std::time::Instant;

use base64::Engine as _;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use unicode_segmentation::UnicodeSegmentation;
use vte::{Params, Perform};

use crate::blocks::{BlockId, BlockState, Blocks, Point};
use crate::cell::{Cell, CellFlags};
use crate::color::{Color, Palette, Rgb, Special};
use crate::event::{ClipboardKind, Cwd, Event, Terminator};
use crate::graphics::ImageStore;
use crate::graphics::placeholder::PLACEHOLDER;
use crate::graphics::sixel::SixelDecoder;
use crate::grid::{Grid, Line};
use crate::modes::{CursorShape, KeyboardFlags, TermMode};
use crate::selection::{Selection, SelectionKind, SelectionRange};
use crate::style::{Attrs, Hyperlink, HyperlinkTable, Style, StyleId, StyleTable, Underline};
use crate::text;
use crate::{ClipboardRead, Config};

const BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);
const MAX_TITLE: usize = 1024;
const MAX_TITLE_STACK: usize = 16;
const MAX_KBD_STACK: usize = 16;
const MAX_OSC52: usize = 8 << 20;
const MAX_DCS: usize = 4096;
const MAX_URI: usize = 2048;
/// Longest OSC 8 `id=`.
const MAX_LINK_ID: usize = 250;
/// Longest notification title or body, command line and OSC 7 URL.
const MAX_TEXT: usize = 4096;
const MAX_GRAPHEME_EXTRA: usize = 32;
/// Most parameters a control sequence carries (the parser's limit).
const PARAMS: usize = 32;
const STYLE_COMPACT_MIN: usize = 1 << 16;
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Charset {
    #[default]
    Ascii,
    DecSpecial,
}

/// Cursor position and the attributes applied to newly written cells.
#[derive(Debug, Clone, Default)]
pub struct Cursor {
    pub x: usize,
    pub y: usize,
    /// The last column was just written; the next printable wraps first (DECAWM).
    pub pending_wrap: bool,
    pub style: Style,
    charsets: [Charset; 4],
    gl: usize,
}

#[derive(Debug, Clone)]
struct SavedCursor {
    cursor: Cursor,
    origin: bool,
    autowrap: bool,
}

/// Rows of the viewport that changed since the last [`Terminal::take_damage`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Damage {
    /// Everything must be redrawn (scroll, mode switch, palette change...).
    pub full: bool,
    pub lines: Vec<bool>,
    /// Image placements or pixels changed (implied by `full`).
    pub images: bool,
}

impl Damage {
    fn new(lines: usize) -> Self {
        Self { full: true, lines: vec![false; lines], images: true }
    }
    pub fn is_line_damaged(&self, y: usize) -> bool {
        self.full || self.lines.get(y).copied().unwrap_or(false)
    }
    /// Images must be re-composited.
    pub fn images_changed(&self) -> bool {
        self.full || self.images
    }
    pub fn is_empty(&self) -> bool {
        !self.full && !self.images && !self.lines.iter().any(|&d| d)
    }
}

/// Which part of a command block to extract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockPart {
    Command,
    Output,
    /// Prompt, command and output.
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DcsKind {
    XtGetTcap,
    Decrqss,
    Sixel,
}

struct Dcs {
    kind: DcsKind,
    data: Vec<u8>,
    sixel: Option<Box<SixelDecoder>>,
}

/// A terminal: two screens (primary with scrollback, alternate), cursor, modes,
/// palette and shell-integration state.
pub struct Terminal {
    parser: vte::Parser,
    bidi_cache: std::cell::RefCell<crate::bidi::Cache>,
    config: Config,
    grid: Grid,
    /// The screen that is not shown (alternate while on primary and vice versa).
    inactive: Grid,
    cursor: Cursor,
    style_id: StyleId,
    erase_id: StyleId,
    /// The cursor style `style_id`/`erase_id` were interned from.
    synced_style: Option<Style>,
    /// Bumped when compaction renumbers style ids.
    style_generation: u64,
    /// Bumped whenever the grids may have changed.
    content_generation: u64,
    saved: Option<SavedCursor>,
    saved_inactive: Option<SavedCursor>,
    mode: TermMode,
    top: usize,
    bottom: usize,
    tabs: Vec<bool>,
    styles: StyleTable,
    style_limit: usize,
    link_limit: usize,
    links: HyperlinkTable,
    palette: Palette,
    cursor_shape: CursorShape,
    title: String,
    title_stack: Vec<String>,
    cwd: Option<Cwd>,
    blocks: Blocks,
    pending_command: Option<String>,
    /// Kitty keyboard flag stacks; index 0 is the base entry and is never popped.
    kbd: Vec<KeyboardFlags>,
    kbd_inactive: Vec<KeyboardFlags>,
    selection: Option<Selection>,
    events: Vec<Event>,
    output: Vec<u8>,
    damage: Damage,
    last_cursor: (usize, usize, bool),
    last_char: Option<(char, usize)>,
    dcs: Option<Dcs>,
    notify_times: VecDeque<Instant>,
    cell_px: (u32, u32),
    dark: bool,
    /// Images of the active screen and of the other one.
    images: ImageStore,
    images_inactive: ImageStore,
    apc: graphics::Apc,
    apc_buf: Vec<u8>,
    apc_overflow: bool,
    osc_len: usize,
    /// The first bytes of the OSC string being read (its number).
    osc_head: Vec<u8>,
    osc_drop: bool,
    local: bool,
    iterm_multipart: Option<(String, Vec<u8>)>,
    decodes: graphics::Decodes,
}

fn default_tabs(cols: usize) -> Vec<bool> {
    (0..cols).map(|i| i % 8 == 0 && i != 0).collect()
}

fn next_generation() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Blank both halves of a wide char occupying column `x` of `row`.
#[inline]
fn clear_wide(row: &mut [Cell], x: usize, erase: StyleId) {
    let Some(cell) = row.get(x) else { return };
    let flags = cell.flags();
    if flags.is_empty() {
        return;
    }
    if flags.contains(CellFlags::WIDE) {
        row[x] = Cell::blank(erase);
        if x + 1 < row.len() {
            row[x + 1] = Cell::blank(erase);
        }
    } else if flags.contains(CellFlags::WIDE_SPACER) {
        row[x] = Cell::blank(erase);
        if x > 0 {
            row[x - 1] = Cell::blank(erase);
        }
    }
}

impl Terminal {
    pub fn new(cols: usize, lines: usize, config: Config) -> Self {
        let cols = cols.max(2);
        let lines = lines.max(1);
        let budget = config.graphics.window_budget.clone();
        let mut mode = TermMode::default();
        mode.set(TermMode::GRAPHEME_CLUSTERING, config.grapheme_clustering);
        Self {
            parser: vte::Parser::new(),
            bidi_cache: Default::default(),
            grid: Grid::new(cols, lines, config.scrollback),
            inactive: Grid::new(cols, lines, 0),
            config,
            cursor: Cursor::default(),
            style_id: StyleId::DEFAULT,
            erase_id: StyleId::DEFAULT,
            synced_style: None,
            style_generation: next_generation(),
            content_generation: next_generation(),
            saved: None,
            saved_inactive: None,
            mode,
            top: 0,
            bottom: lines - 1,
            tabs: default_tabs(cols),
            styles: StyleTable::default(),
            style_limit: STYLE_COMPACT_MIN,
            link_limit: 4096,
            links: HyperlinkTable::default(),
            palette: Palette::default(),
            cursor_shape: CursorShape::default(),
            title: String::new(),
            title_stack: Vec::new(),
            cwd: None,
            blocks: Blocks::default(),
            pending_command: None,
            kbd: vec![KeyboardFlags::empty()],
            kbd_inactive: vec![KeyboardFlags::empty()],
            selection: None,
            events: Vec::new(),
            output: Vec::new(),
            damage: Damage::new(lines),
            last_cursor: (usize::MAX, usize::MAX, false),
            last_char: None,
            dcs: None,
            notify_times: VecDeque::new(),
            cell_px: (0, 0),
            dark: true,
            images: ImageStore::new(budget.clone()),
            images_inactive: ImageStore::new(budget),
            apc: graphics::Apc::Ground,
            apc_buf: Vec::new(),
            apc_overflow: false,
            osc_len: 0,
            osc_head: Vec::with_capacity(5),
            osc_drop: false,
            local: true,
            iterm_multipart: None,
            decodes: Default::default(),
        }
    }

    /// Feed bytes read from the PTY.
    pub fn advance(&mut self, bytes: &[u8]) {
        self.content_generation = next_generation();
        // Rows above the screen never change, so only layouts from the
        // block holding the top screen row onwards can go stale.
        let alt = self.is_alt_screen();
        let stale = crate::bidi::Cache::block_start(&self.grid, self.grid.abs_line(0));
        self.poll_images();
        let mut parser = std::mem::take(&mut self.parser);
        self.feed(&mut parser, bytes);
        self.parser = parser;
        let switched = alt != self.is_alt_screen();
        let cache = self.bidi_cache.get_mut();
        if switched {
            cache.clear();
        } else {
            cache.invalidate_from(stale);
        }
    }

    // --- accessors -------------------------------------------------------

    /// The active screen (alternate screen while a full-screen app runs).
    pub fn grid(&self) -> &Grid {
        &self.grid
    }
    /// The primary screen with scrollback, regardless of which screen is active.
    pub fn primary_grid(&self) -> &Grid {
        if self.is_alt_screen() { &self.inactive } else { &self.grid }
    }
    pub fn set_bidi(&mut self, mode: crate::bidi::Mode) {
        self.config.bidi = mode;
        self.bidi_cache.get_mut().clear();
        self.damage.full = true;
    }
    pub fn bidi_enabled(&self) -> bool {
        self.config.bidi.enabled(self.is_alt_screen())
    }
    /// Logical storage stays intact; this map is shared by drawing and hit testing.
    pub fn visual_row(&self, line: Line) -> std::sync::Arc<crate::bidi::RowLayout> {
        if !self.bidi_enabled() {
            return self.bidi_cache.borrow().identity();
        }
        self.bidi_cache.borrow_mut().row(&self.grid, line)
    }

    pub fn cols(&self) -> usize {
        self.grid.cols()
    }
    pub fn lines(&self) -> usize {
        self.grid.lines()
    }
    pub fn cursor(&self) -> &Cursor {
        &self.cursor
    }
    pub fn mode(&self) -> TermMode {
        self.mode
    }
    pub fn is_alt_screen(&self) -> bool {
        self.mode.contains(TermMode::ALT_SCREEN)
    }
    /// Mode 2026 is on: the host should hold rendering (with a timeout).
    pub fn is_synchronized(&self) -> bool {
        self.mode.contains(TermMode::SYNC_OUTPUT)
    }
    pub fn cursor_shape(&self) -> CursorShape {
        self.cursor_shape
    }
    /// Cursor should be drawn: DECTCEM set and the viewport shows the live screen.
    pub fn cursor_visible(&self) -> bool {
        self.mode.contains(TermMode::SHOW_CURSOR) && self.grid.display_offset() == 0
    }
    pub fn keyboard_flags(&self) -> KeyboardFlags {
        self.kbd.last().copied().unwrap_or_default()
    }
    pub fn title(&self) -> &str {
        &self.title
    }
    pub fn cwd(&self) -> Option<&Cwd> {
        self.cwd.as_ref()
    }
    pub fn palette(&self) -> &Palette {
        &self.palette
    }
    /// Reload configured colors without discarding application OSC overrides.
    pub fn rebase_palette(&mut self, palette: &Palette) {
        self.palette.rebase(palette);
        self.damage_all();
    }

    /// Replace the theme palette (keeps nothing of the old OSC overrides).
    pub fn set_palette(&mut self, palette: Palette) {
        self.palette = palette;
        self.damage_all();
    }
    pub fn styles(&self) -> &StyleTable {
        &self.styles
    }
    /// Changes whenever a style id may start to mean another style; unique
    /// across terminals.
    pub fn style_generation(&self) -> u64 {
        self.style_generation
    }
    /// Changes whenever the text of the grids may have changed; unique
    /// across terminals.
    pub fn content_generation(&self) -> u64 {
        self.content_generation
    }
    pub fn style(&self, id: StyleId) -> &Style {
        self.styles.get(id)
    }
    pub fn hyperlink(&self, id: crate::style::HyperlinkId) -> Option<&Hyperlink> {
        self.links.get(id)
    }
    pub fn blocks(&self) -> &Blocks {
        &self.blocks
    }
    pub fn config(&self) -> &Config {
        &self.config
    }

    // --- host interaction ------------------------------------------------

    /// Bytes the terminal wants to send to the application (query replies).
    pub fn take_pty_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.output)
    }

    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    /// Viewport rows changed since the last call. Cursor moves count as damage
    /// of the rows the cursor left and entered.
    pub fn take_damage(&mut self) -> Damage {
        self.damage_cursor();
        let lines = self.lines();
        std::mem::replace(&mut self.damage, Damage { full: false, lines: vec![false; lines], images: false })
    }

    /// Whether anything is damaged; clears the damage like
    /// [`Terminal::take_damage`] without allocating.
    pub fn clear_damage(&mut self) -> bool {
        self.damage_cursor();
        let d = &mut self.damage;
        let damaged = !d.is_empty();
        d.full = false;
        d.images = false;
        d.lines.fill(false);
        damaged
    }

    fn damage_cursor(&mut self) {
        let cur = (self.cursor.x, self.cursor.y, self.cursor_visible());
        if cur != self.last_cursor {
            let old = self.last_cursor.1;
            if old < self.lines() {
                self.damage_line(old);
            }
            self.damage_line(cur.1);
            self.last_cursor = cur;
        }
    }

    /// Size of one cell in physical pixels (for pixel-size reports).
    pub fn set_cell_pixel_size(&mut self, width: u32, height: u32) {
        self.cell_px = (width, height);
    }

    /// Color scheme changed (XDG portal); reported when mode 2031 is set.
    pub fn set_color_scheme(&mut self, dark: bool) {
        if self.dark != dark {
            self.dark = dark;
            if self.mode.contains(TermMode::COLOR_SCHEME_UPDATES) {
                self.report_color_scheme();
            }
        }
    }

    /// Answer an OSC 52 read request after the user approved it.
    pub fn respond_clipboard(&mut self, kind: ClipboardKind, text: &str, terminator: Terminator) {
        let sel = match kind {
            ClipboardKind::Clipboard => 'c',
            ClipboardKind::Primary => 'p',
        };
        let data = BASE64.encode(text.as_bytes());
        self.reply(&format!("\x1b]52;{sel};{data}{}", terminator.as_str()));
    }

    // --- viewport --------------------------------------------------------

    /// Scroll the viewport by `delta` rows (positive = back in history).
    pub fn scroll_display(&mut self, delta: isize) {
        if self.grid.scroll_display(delta) {
            self.damage.full = true;
        }
    }

    /// Scroll so absolute line `abs` is at the top of the viewport (clamped).
    pub fn scroll_to_line(&mut self, abs: Line) {
        if self.grid.scroll_to_line(abs) {
            self.damage.full = true;
        }
    }

    /// Scroll as little as possible so absolute line `abs` is visible.
    pub fn reveal_line(&mut self, abs: Line) {
        let top = self.grid.viewport_line(0);
        let lines = self.lines() as Line;
        if abs < top {
            self.scroll_to_line(abs);
        } else if abs >= top + lines {
            self.scroll_to_line(abs + 1 - lines);
        }
    }

    pub fn scroll_to_bottom(&mut self) {
        if self.grid.reset_display() {
            self.damage.full = true;
        }
    }

    /// Scroll so the previous/next prompt is at the top of the viewport.
    pub fn scroll_to_prompt(&mut self, forward: bool) -> bool {
        if self.is_alt_screen() {
            return false;
        }
        let top = self.grid.viewport_line(0);
        let target = if forward { self.blocks.next_prompt(top) } else { self.blocks.prev_prompt(top) };
        match target {
            Some(line) => {
                self.grid.scroll_to_line(line);
                self.damage.full = true;
                true
            }
            None => false,
        }
    }

    /// Visible screen text (live screen, not the viewport), rows trimmed.
    pub fn screen_text(&self) -> String {
        let mut lines: Vec<String> = (0..self.lines()).map(|y| self.row_text(y)).collect();
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }

    /// Primary screen and its retained history, without escape sequences.
    /// Soft-wrapped rows are joined; an alternate-screen application is excluded.
    pub fn scrollback_text(&self) -> String {
        let grid = self.primary_grid();
        text::extract(
            grid,
            Point::new(grid.first_line(), 0),
            Point::new(grid.end_line().saturating_sub(1), grid.cols() - 1),
            false,
        )
        .trim_end_matches('\n')
        .to_owned()
    }

    pub fn row_text(&self, y: usize) -> String {
        self.grid.row(y).text(0..self.cols()).trim_end().to_string()
    }

    /// Text between two absolute points (inclusive).
    pub fn text_range(&self, start: Point, end: Point) -> String {
        text::extract(&self.grid, start, end, false)
    }

    fn live_end(&self) -> Line {
        self.grid.abs_line(self.cursor.y) + 1
    }

    // --- selection -------------------------------------------------------

    pub fn start_selection(&mut self, kind: SelectionKind, at: Point) {
        self.selection = Some(Selection::new(kind, at));
        self.damage.full = true;
    }

    pub fn update_selection(&mut self, to: Point) {
        if let Some(sel) = &mut self.selection {
            sel.update(to);
            self.damage.full = true;
        }
    }

    pub fn clear_selection(&mut self) {
        if self.selection.take().is_some() {
            self.damage.full = true;
        }
    }

    pub fn selection(&self) -> Option<&Selection> {
        self.selection.as_ref()
    }

    pub fn selection_range(&self) -> Option<SelectionRange> {
        let blocks = if self.is_alt_screen() { None } else { Some(&self.blocks) };
        let selection = self.selection.as_ref()?;
        if selection.kind == SelectionKind::Block && self.bidi_enabled() {
            let a = selection.anchor;
            let b = selection.cursor;
            let visual =
                Selection::new(SelectionKind::Block, Point::new(a.line, self.visual_row(a.line).visual(a.col)));
            let visual = Selection { cursor: Point::new(b.line, self.visual_row(b.line).visual(b.col)), ..visual };
            visual.range(&self.grid, blocks, &self.config.word_separators, self.live_end())
        } else {
            selection.range(&self.grid, blocks, &self.config.word_separators, self.live_end())
        }
    }

    pub fn selection_text(&self) -> Option<String> {
        let r = self.selection_range()?;
        if r.rect && self.bidi_enabled() {
            let mut out = String::new();
            for line in r.start.line..=r.end.line {
                if line > r.start.line {
                    out.push('\n');
                }
                let layout = self.visual_row(line);
                let Some(row) = self.grid.abs_row(line) else {
                    continue;
                };
                let mut text = String::new();
                for (col, cell) in row.cells.iter().enumerate() {
                    if cell.is_spacer() {
                        continue;
                    }
                    let x = layout.visual(col);
                    if x <= r.end.col && x + cell.width().max(1) > r.start.col {
                        cell.push_text(&mut text);
                    }
                }
                out.push_str(text.trim_end_matches(' '));
            }
            Some(out)
        } else {
            Some(text::extract(&self.grid, r.start, r.end, r.rect))
        }
    }

    // --- blocks ----------------------------------------------------------

    /// Text of part of a block (primary screen only).
    pub fn block_text(&self, id: BlockId, part: BlockPart) -> Option<String> {
        let b = self.blocks.get(id)?;
        let grid = self.primary_grid();
        let last = grid.cols() - 1;
        let live_end = if self.is_alt_screen() { grid.end_line() } else { self.live_end() };
        match part {
            BlockPart::Command => b.command.clone(),
            BlockPart::Output => {
                let r = b.output_lines(live_end)?;
                if r.is_empty() {
                    return Some(String::new());
                }
                Some(text::extract(grid, Point::new(r.start, 0), Point::new(r.end - 1, last), false))
            }
            BlockPart::All => {
                let end = b.output_lines(live_end).map_or(b.prompt_line + 1, |r| r.end.max(b.prompt_line + 1));
                Some(text::extract(grid, Point::new(b.prompt_line, 0), Point::new(end - 1, last), false))
            }
        }
    }

    pub fn set_block_folded(&mut self, id: BlockId, folded: bool) -> bool {
        let changed = self.blocks.set_folded(id, folded);
        if changed {
            self.damage.full = true;
        }
        changed
    }

    /// Output line ranges of all blocks (for "search output only").
    pub fn output_ranges(&self) -> Vec<Range<Line>> {
        let live_end = self.live_end();
        self.blocks.iter().filter_map(|b| b.output_lines(live_end)).filter(|r| !r.is_empty()).collect()
    }

    // --- resize ----------------------------------------------------------

    /// Resize both screens. The primary screen reflows soft-wrapped lines.
    pub fn resize(&mut self, cols: usize, lines: usize) {
        self.content_generation = next_generation();
        self.bidi_cache.get_mut().clear();
        let cols = cols.max(2);
        let lines = lines.max(1);
        if cols == self.cols() && lines == self.lines() {
            return;
        }
        let alt = self.is_alt_screen();
        let (primary, alt_grid) =
            if alt { (&mut self.inactive, &mut self.grid) } else { (&mut self.grid, &mut self.inactive) };

        // Reflow the primary screen around its cursor (the saved one while in alt screen).
        let saved_primary = if alt { &mut self.saved_inactive } else { &mut self.saved };
        let (px, py, pw) = if alt {
            match saved_primary {
                Some(s) => (s.cursor.x, s.cursor.y, s.cursor.pending_wrap),
                None => (0, primary.lines() - 1, false),
            }
        } else {
            (self.cursor.x, self.cursor.y, self.cursor.pending_wrap)
        };
        let r = primary.reflow(cols, lines, px, py.min(primary.lines() - 1), pw);
        self.blocks.remap(&r.line_map);
        let (primary_images, alt_images) = if alt {
            (&mut self.images_inactive, &mut self.images)
        } else {
            (&mut self.images, &mut self.images_inactive)
        };
        primary_images.remap(&r.line_map, cols);
        if alt {
            if let Some(s) = saved_primary {
                s.cursor.x = r.cursor_x;
                s.cursor.y = r.cursor_y;
                s.cursor.pending_wrap = r.pending_wrap;
            }
            let old_y = self.cursor.y.min(alt_grid.lines() - 1);
            self.cursor.y = alt_grid.resize_simple(cols, lines, old_y);
            alt_images.shift(self.cursor.y as i64 - old_y as i64);
        } else {
            self.cursor.x = r.cursor_x;
            self.cursor.y = r.cursor_y;
            self.cursor.pending_wrap = r.pending_wrap;
            alt_grid.resize_simple(cols, lines, 0);
            alt_images.clear();
        }

        for s in [&mut self.saved, &mut self.saved_inactive].into_iter().flatten() {
            s.cursor.x = s.cursor.x.min(cols - 1);
            s.cursor.y = s.cursor.y.min(lines - 1);
        }
        self.cursor.x = self.cursor.x.min(cols - 1);
        self.cursor.y = self.cursor.y.min(lines - 1);
        self.top = 0;
        self.bottom = lines - 1;
        self.tabs = default_tabs(cols);
        self.selection = None;
        self.damage = Damage::new(lines);
        if self.mode.contains(TermMode::IN_BAND_RESIZE) {
            self.report_size();
        }
    }

    // --- internal helpers ------------------------------------------------

    fn reply(&mut self, s: &str) {
        self.output.extend_from_slice(s.as_bytes());
    }

    fn damage_line(&mut self, y: usize) {
        if self.grid.display_offset() > 0 {
            self.damage.full = true;
        } else if let Some(d) = self.damage.lines.get_mut(y) {
            *d = true;
        }
    }

    fn damage_range(&mut self, top: usize, bottom: usize) {
        if self.grid.display_offset() > 0 {
            self.damage.full = true;
            return;
        }
        let end = (bottom + 1).min(self.damage.lines.len());
        if top < end {
            self.damage.lines[top..end].fill(true);
        }
    }

    fn damage_all(&mut self) {
        self.damage.full = true;
        self.damage.images = true;
    }

    /// Re-intern the cursor style after it changed.
    fn sync_style(&mut self) {
        if self.styles.len() > self.style_limit {
            self.compact_styles();
        }
        if self.synced_style.as_ref().is_some_and(|s| *s == self.cursor.style) {
            return;
        }
        let style = &self.cursor.style;
        self.style_id = if *style == Style::default() { StyleId::DEFAULT } else { self.styles.intern(style) };
        // The erase style only depends on the background.
        if self.synced_style.as_ref().is_none_or(|s| s.bg != style.bg) {
            self.erase_id = self.styles.intern(&style.erased());
        }
        self.synced_style = Some(style.clone());
    }

    /// Rebuild the style table keeping only styles still referenced by cells.
    /// Keeps memory bounded under true-color animations.
    fn compact_styles(&mut self) {
        let old = std::mem::take(&mut self.styles).into_styles();
        self.synced_style = None;
        self.style_generation = next_generation();
        let mut remap = vec![u32::MAX; old.len()];
        let fallback = Style::default();
        for grid in [&mut self.grid, &mut self.inactive] {
            for row in grid.rows_mut() {
                for cell in &mut row.cells {
                    let i = cell.style.0 as usize;
                    let id = match remap.get(i) {
                        Some(&id) if id != u32::MAX => id,
                        _ => {
                            let id = self.styles.intern(old.get(i).unwrap_or(&fallback)).0;
                            if let Some(slot) = remap.get_mut(i) {
                                *slot = id;
                            }
                            id
                        }
                    };
                    cell.style = StyleId(id);
                }
            }
        }
        drop(remap);
        drop(old);
        self.style_limit = STYLE_COMPACT_MIN.max(self.styles.len() * 2);
        let mut live = std::collections::HashSet::new();
        for style in (0..self.styles.len()).map(|i| self.styles.get(StyleId(i as u32))) {
            live.extend(style.hyperlink);
        }
        live.extend(self.cursor.style.hyperlink);
        for saved in [&self.saved, &self.saved_inactive].into_iter().flatten() {
            live.extend(saved.cursor.style.hyperlink);
        }
        self.links.retain(&live);
        self.link_limit = 4096.max(self.links.len() * 2);
    }

    fn clamp_cursor(&mut self) {
        self.cursor.x = self.cursor.x.min(self.cols() - 1);
        self.cursor.y = self.cursor.y.min(self.lines() - 1);
    }

    // --- printing --------------------------------------------------------

    fn print_char(&mut self, c: char) {
        let charset = self.cursor.charsets[self.cursor.gl];
        if charset == Charset::Ascii && (' '..='~').contains(&c) {
            self.last_char = Some((c, 1));
            self.put_char(c, 1);
            return;
        }
        let c = match charset {
            Charset::DecSpecial => text::dec_special(c),
            Charset::Ascii => c,
        };
        // Image placeholders are always one column, even with ambiguous = wide.
        let width = if c == PLACEHOLDER { 1 } else { text::char_width(c, self.config.ambiguous_wide) };
        if crate::bidi::is_control(c) && self.prev_cell().is_none() {
            let cell = self.grid.cell_mut(self.cursor.x, self.cursor.y);
            if cell.prefix().map_or(0, str::len) < MAX_GRAPHEME_EXTRA {
                cell.push_prefix(c);
            }
            self.damage_line(self.cursor.y);
            return;
        }
        if width == 0 || self.extends_cluster(c) {
            self.append_to_prev(c);
            return;
        }
        self.last_char = Some((c, width));
        self.put_char(c, width);
    }

    /// Cell holding the most recently printed character.
    fn prev_cell(&self) -> Option<(usize, usize)> {
        let y = self.cursor.y;
        let x = if self.cursor.pending_wrap { self.cursor.x } else { self.cursor.x.checked_sub(1)? };
        let row = self.grid.row(y);
        if x > 0 && row.cells[x].flags().contains(CellFlags::WIDE_SPACER) { Some((x - 1, y)) } else { Some((x, y)) }
    }

    /// With mode 2027, a printable char that continues the previous grapheme
    /// cluster (ZWJ sequences, regional-indicator pairs...) joins its cell.
    fn extends_cluster(&self, c: char) -> bool {
        if c.is_ascii() || !self.mode.contains(TermMode::GRAPHEME_CLUSTERING) {
            return false;
        }
        let Some((x, y)) = self.prev_cell() else { return false };
        let cell = &self.grid.row(y).cells[x];
        if cell.is_empty() {
            return false;
        }
        let mut s = String::with_capacity(16);
        cell.push_text(&mut s);
        s.push(c);
        s.graphemes(true).nth(1).is_none()
    }

    fn append_to_prev(&mut self, c: char) {
        let Some((x, y)) = self.prev_cell() else { return };
        let amb = self.config.ambiguous_wide;
        let grapheme = self.mode.contains(TermMode::GRAPHEME_CLUSTERING);
        let cols = self.cols();
        let cell = self.grid.cell_mut(x, y);
        if cell.extra().is_some_and(|e| e.len() >= MAX_GRAPHEME_EXTRA) {
            return;
        }
        cell.push_extra(c);
        let widen = grapheme && !cell.flags().contains(CellFlags::WIDE) && {
            let mut s = String::new();
            cell.push_text(&mut s);
            text::str_width(&s, amb) == 2
        };
        let style = cell.style;
        self.damage_line(y);
        // e.g. U+2764 + VS16: the cluster becomes emoji presentation, two columns.
        if widen && x + 1 < cols && !self.cursor.pending_wrap && self.cursor.x == x + 1 {
            self.clear_wide(x + 1, y);
            self.grid.cell_mut(x, y).insert_flags(CellFlags::WIDE);
            *self.grid.cell_mut(x + 1, y) = Cell::new(' ', style, CellFlags::WIDE_SPACER);
            if x + 2 >= cols {
                self.cursor.x = cols - 1;
                self.cursor.pending_wrap = self.mode.contains(TermMode::AUTOWRAP);
            } else {
                self.cursor.x = x + 2;
            }
        }
    }

    /// Blank both halves of a wide char occupying column `x`.
    fn clear_wide(&mut self, x: usize, y: usize) {
        clear_wide(&mut self.grid.row_mut(y).cells, x, self.erase_id);
    }

    fn put_char(&mut self, c: char, width: usize) {
        let cols = self.cols();
        let autowrap = self.mode.contains(TermMode::AUTOWRAP);
        if self.cursor.pending_wrap && autowrap {
            self.wrap();
        }
        if width == 2 && self.cursor.x + 1 >= cols {
            if autowrap {
                let (x, y) = (self.cursor.x, self.cursor.y);
                self.clear_wide(x, y);
                *self.grid.cell_mut(x, y) = Cell::new(' ', self.style_id, CellFlags::LEADING_WIDE_SPACER);
                self.wrap();
            } else {
                self.cursor.x = cols - 2;
            }
        }
        if self.mode.contains(TermMode::INSERT) {
            self.insert_blank(width);
        }
        let (x, y) = (self.cursor.x, self.cursor.y);
        let (style, erase) = (self.style_id, self.erase_id);
        let row = &mut self.grid.row_mut(y).cells;
        if width == 1 && row[x].flags().is_empty() && !row[x].has_text() {
            row[x] = Cell::new(c, style, CellFlags::empty());
        } else {
            let prefix = if row[x].ch() == ' ' { row[x].take_prefix() } else { None };
            clear_wide(row, x, erase);
            if width == 2 {
                clear_wide(row, x + 1, erase);
                row[x] = Cell::new(c, style, CellFlags::WIDE);
                row[x + 1] = Cell::new(' ', style, CellFlags::WIDE_SPACER);
            } else {
                row[x] = Cell::new(c, style, CellFlags::empty());
            }
            if prefix.is_some() {
                row[x].set_prefix(prefix);
            }
        }
        self.damage_line(y);
        if x + width >= cols {
            self.cursor.x = cols - 1;
            self.cursor.pending_wrap = autowrap;
        } else {
            self.cursor.x = x + width;
            self.cursor.pending_wrap = false;
        }
    }

    /// Soft wrap: mark the row as continued and move to the next line.
    fn wrap(&mut self) {
        let y = self.cursor.y;
        self.grid.row_mut(y).wrapped = true;
        self.cursor.x = 0;
        self.linefeed();
    }

    fn repeat(&mut self, last: Option<(char, usize)>, n: usize) {
        if let Some((c, w)) = last {
            for _ in 0..n.min(self.cols() * (self.bottom - self.top + 1)) {
                self.put_char(c, w);
            }
        }
    }

    // --- cursor movement & scrolling ------------------------------------

    fn linefeed(&mut self) {
        self.cursor.pending_wrap = false;
        if self.cursor.y == self.bottom {
            self.scroll_up(1);
        } else if self.cursor.y + 1 < self.lines() {
            self.cursor.y += 1;
        }
    }

    fn reverse_index(&mut self) {
        self.cursor.pending_wrap = false;
        if self.cursor.y == self.top {
            self.scroll_down(1);
        } else if self.cursor.y > 0 {
            self.cursor.y -= 1;
        }
    }

    /// Scroll the region up; a full-screen region on the primary screen feeds scrollback.
    fn scroll_up(&mut self, n: usize) {
        let (top, bottom) = (self.grid.abs_line(self.top) as i64, self.grid.abs_line(self.bottom) as i64 + 1);
        let pushed = self.grid.scroll_up(self.top, self.bottom, n, self.erase_id, true);
        self.damage.images |= !self.images.placements().is_empty();
        if pushed == 0 {
            self.images.scroll_region(top, bottom, -(n.min(self.bottom + 1 - self.top) as i64));
        }
        if pushed > 0 {
            let first = self.grid.first_line();
            self.images.prune(first as i64);
            if !self.is_alt_screen() {
                self.blocks.prune(first);
            }
            if self.selection.is_some_and(|s| s.anchor.line.min(s.cursor.line) < first) {
                self.selection = None;
            }
            if self.grid.display_offset() > 0 {
                return;
            }
        }
        self.damage_range(self.top, self.bottom);
    }

    fn scroll_down(&mut self, n: usize) {
        self.scroll_images(self.top, self.bottom, n.min(self.bottom + 1 - self.top) as i64);
        self.grid.scroll_down(self.top, self.bottom, n, self.erase_id);
        self.damage_range(self.top, self.bottom);
    }

    /// Text of screen rows `top..=bottom` moved by `delta` rows.
    fn scroll_images(&mut self, top: usize, bottom: usize, delta: i64) {
        if !self.images.placements().is_empty() {
            let (a, b) = (self.grid.abs_line(top) as i64, self.grid.abs_line(bottom) as i64 + 1);
            self.images.scroll_region(a, b, delta);
            self.damage.images = true;
        }
    }

    /// CUP/HVP/VPA: `y` is relative to the scroll region in origin mode.
    fn goto(&mut self, x: usize, y: usize) {
        let (base, max) =
            if self.mode.contains(TermMode::ORIGIN) { (self.top, self.bottom) } else { (0, self.lines() - 1) };
        self.cursor.y = (base + y).min(max);
        self.cursor.x = x.min(self.cols() - 1);
        self.cursor.pending_wrap = false;
    }

    fn move_up(&mut self, n: usize) {
        let min = if self.cursor.y >= self.top { self.top } else { 0 };
        self.cursor.y = self.cursor.y.saturating_sub(n).max(min);
        self.cursor.pending_wrap = false;
    }

    fn move_down(&mut self, n: usize) {
        let max = if self.cursor.y <= self.bottom { self.bottom } else { self.lines() - 1 };
        self.cursor.y = (self.cursor.y + n).min(max);
        self.cursor.pending_wrap = false;
    }

    fn move_right(&mut self, n: usize) {
        self.cursor.x = (self.cursor.x + n).min(self.cols() - 1);
        self.cursor.pending_wrap = false;
    }

    fn move_left(&mut self, n: usize) {
        self.cursor.x = self.cursor.x.saturating_sub(n);
        self.cursor.pending_wrap = false;
    }

    fn set_x(&mut self, x: usize) {
        self.cursor.x = x.min(self.cols() - 1);
        self.cursor.pending_wrap = false;
    }

    fn tab_forward(&mut self, n: usize) {
        let cols = self.cols();
        let mut x = self.cursor.x;
        for _ in 0..n {
            x = (x + 1..cols).find(|&i| self.tabs[i]).unwrap_or(cols - 1);
        }
        if x != self.cursor.x {
            self.cursor.x = x;
            self.cursor.pending_wrap = false;
        }
    }

    fn tab_backward(&mut self, n: usize) {
        let mut x = self.cursor.x;
        for _ in 0..n {
            x = (0..x).rev().find(|&i| self.tabs[i]).unwrap_or(0);
        }
        self.cursor.x = x;
        self.cursor.pending_wrap = false;
    }

    // --- erasing & editing ----------------------------------------------

    /// Erase columns `x0..x1` of row `y` with the current background (BCE).
    fn erase_range(&mut self, y: usize, x0: usize, x1: usize) {
        let cols = self.cols();
        let x1 = x1.min(cols);
        if x0 >= x1 {
            return;
        }
        self.clear_wide(x0, y);
        self.clear_wide(x1 - 1, y);
        let erase = self.erase_id;
        let row = self.grid.row_mut(y);
        for c in &mut row.cells[x0..x1] {
            *c = Cell::blank(erase);
        }
        if x1 == cols {
            row.wrapped = false;
        }
        self.damage_line(y);
    }

    fn clear_row(&mut self, y: usize) {
        let erase = self.erase_id;
        self.grid.row_mut(y).clear(erase);
        self.damage_line(y);
    }

    fn erase_display(&mut self, mode: usize) {
        let (x, y) = (self.cursor.x, self.cursor.y);
        let (cols, lines) = (self.cols(), self.lines());
        match mode {
            0 => {
                self.erase_range(y, x, cols);
                for l in y + 1..lines {
                    self.clear_row(l);
                }
            }
            1 => {
                for l in 0..y {
                    self.clear_row(l);
                }
                self.erase_range(y, 0, x + 1);
            }
            2 => {
                for l in 0..lines {
                    self.clear_row(l);
                }
                self.selection = None;
                // Like kitty: clearing the screen clears its images too.
                let (a, b) = (self.grid.abs_line(0) as i64, self.grid.abs_line(lines - 1) as i64 + 1);
                let s = &self.images;
                let doomed: Vec<u64> =
                    s.placements().iter().filter(|p| s.intersects_lines(p, a, b)).map(|p| p.seq).collect();
                if !doomed.is_empty() {
                    self.images.remove_placements(|p| doomed.contains(&p.seq));
                    self.images.free_anonymous();
                    self.damage.images = true;
                }
            }
            3 => {
                self.grid.clear_history();
                self.images.prune(self.grid.first_line() as i64);
                self.damage.images = true;
                if !self.is_alt_screen() {
                    self.blocks.prune(self.grid.first_line());
                }
                self.selection = None;
                self.damage_all();
            }
            _ => {}
        }
    }

    fn erase_line(&mut self, mode: usize) {
        let (x, y, cols) = (self.cursor.x, self.cursor.y, self.cols());
        match mode {
            0 => self.erase_range(y, x, cols),
            1 => self.erase_range(y, 0, x + 1),
            2 => self.erase_range(y, 0, cols),
            _ => {}
        }
    }

    fn erase_chars(&mut self, n: usize) {
        let (x, y) = (self.cursor.x, self.cursor.y);
        self.erase_range(y, x, x.saturating_add(n));
    }

    /// ICH: shift the rest of the line right by `n` blanks.
    fn insert_blank(&mut self, n: usize) {
        let (x, y, cols) = (self.cursor.x, self.cursor.y, self.cols());
        let n = n.min(cols - x);
        // Inserting before a lead cell moves the entire character. Only
        // insertion inside its spacer splits the pair.
        if self.grid.row(y).cells[x].flags().contains(CellFlags::WIDE_SPACER) {
            self.clear_wide(x, y);
        }
        let erase = self.erase_id;
        let cells = &mut self.grid.row_mut(y).cells;
        cells[x..].rotate_right(n);
        for c in &mut cells[x..x + n] {
            *c = Cell::blank(erase);
        }
        // A wide char pushed into the last column lost its spacer.
        if cells[cols - 1].flags().contains(CellFlags::WIDE) {
            cells[cols - 1] = Cell::blank(erase);
        }
        self.cursor.pending_wrap = false;
        self.damage_line(y);
    }

    /// DCH: delete `n` cells, pulling the rest of the line left.
    fn delete_chars(&mut self, n: usize) {
        let (x, y, cols) = (self.cursor.x, self.cursor.y, self.cols());
        let n = n.min(cols - x);
        self.clear_wide(x, y);
        self.clear_wide(x + n - 1, y);
        let erase = self.erase_id;
        let cells = &mut self.grid.row_mut(y).cells;
        cells[x..].rotate_left(n);
        for c in &mut cells[cols - n..] {
            *c = Cell::blank(erase);
        }
        self.cursor.pending_wrap = false;
        self.damage_line(y);
    }

    fn insert_lines(&mut self, n: usize) {
        let y = self.cursor.y;
        if y < self.top || y > self.bottom {
            return;
        }
        self.scroll_images(y, self.bottom, n.min(self.bottom + 1 - y) as i64);
        self.grid.scroll_down(y, self.bottom, n, self.erase_id);
        self.cursor.x = 0;
        self.cursor.pending_wrap = false;
        self.damage_range(y, self.bottom);
    }

    fn delete_lines(&mut self, n: usize) {
        let y = self.cursor.y;
        if y < self.top || y > self.bottom {
            return;
        }
        self.scroll_images(y, self.bottom, -(n.min(self.bottom + 1 - y) as i64));
        self.grid.scroll_up(y, self.bottom, n, self.erase_id, false);
        self.cursor.x = 0;
        self.cursor.pending_wrap = false;
        self.damage_range(y, self.bottom);
    }

    // --- cursor save/restore, screens, resets ----------------------------

    fn save_cursor(&mut self) {
        self.saved = Some(SavedCursor {
            cursor: self.cursor.clone(),
            origin: self.mode.contains(TermMode::ORIGIN),
            autowrap: self.mode.contains(TermMode::AUTOWRAP),
        });
    }

    fn restore_cursor(&mut self) {
        match self.saved.clone() {
            Some(s) => {
                self.cursor = s.cursor;
                self.mode.set(TermMode::ORIGIN, s.origin);
                self.mode.set(TermMode::AUTOWRAP, s.autowrap);
            }
            None => {
                self.cursor = Cursor::default();
                self.mode.remove(TermMode::ORIGIN);
            }
        }
        self.clamp_cursor();
        self.sync_style();
    }

    fn swap_screens(&mut self) {
        std::mem::swap(&mut self.grid, &mut self.inactive);
        std::mem::swap(&mut self.saved, &mut self.saved_inactive);
        std::mem::swap(&mut self.kbd, &mut self.kbd_inactive);
        std::mem::swap(&mut self.images, &mut self.images_inactive);
        self.mode.toggle(TermMode::ALT_SCREEN);
        self.grid.reset_display();
        self.inactive.reset_display();
        self.selection = None;
        self.damage_all();
    }

    fn enter_alt(&mut self, save_cursor: bool, clear: bool) {
        if !self.is_alt_screen() {
            if save_cursor {
                self.save_cursor();
            }
            self.swap_screens();
        }
        if clear {
            self.grid.clear_screen(self.erase_id);
            self.images.clear();
        }
        self.clamp_cursor();
    }

    fn leave_alt(&mut self, restore_cursor: bool, clear_alt: bool) {
        if !self.is_alt_screen() {
            return;
        }
        if clear_alt {
            self.grid.clear_screen(self.erase_id);
        }
        // The alternate screen's images never outlive it.
        self.images.clear();
        self.swap_screens();
        if restore_cursor {
            self.restore_cursor();
        }
        self.clamp_cursor();
    }

    /// DECSTR.
    fn soft_reset(&mut self) {
        self.mode.remove(TermMode::INSERT | TermMode::ORIGIN | TermMode::APP_CURSOR | TermMode::APP_KEYPAD);
        self.mode.insert(TermMode::AUTOWRAP | TermMode::SHOW_CURSOR);
        self.top = 0;
        self.bottom = self.lines() - 1;
        self.cursor.style = Style::default();
        self.cursor.charsets = Default::default();
        self.cursor.gl = 0;
        self.cursor.pending_wrap = false;
        self.saved = None;
        self.sync_style();
    }

    /// RIS: everything back to power-on state, scrollback included.
    fn full_reset(&mut self) {
        let mut fresh = Terminal::new(self.cols(), self.lines(), self.config.clone());
        fresh.palette = std::mem::take(&mut self.palette);
        fresh.palette.reset_all();
        fresh.events = std::mem::take(&mut self.events);
        fresh.output = std::mem::take(&mut self.output);
        fresh.notify_times = std::mem::take(&mut self.notify_times);
        fresh.cell_px = self.cell_px;
        fresh.dark = self.dark;
        fresh.local = self.local;
        fresh.cwd = self.cwd.take();
        fresh.apc = self.apc;
        fresh.apc_buf = std::mem::take(&mut self.apc_buf);
        fresh.apc_overflow = self.apc_overflow;
        fresh.osc_len = self.osc_len;
        fresh.osc_head = std::mem::take(&mut self.osc_head);
        fresh.osc_drop = self.osc_drop;
        fresh.grid.set_first_line(self.grid.end_line().max(self.inactive.end_line()));
        *self = fresh;
        self.events.push(Event::TitleChanged(String::new()));
    }

    /// DECALN: fill the screen with 'E'.
    fn alignment_test(&mut self) {
        self.top = 0;
        self.bottom = self.lines() - 1;
        self.mode.remove(TermMode::ORIGIN);
        for y in 0..self.lines() {
            let row = self.grid.row_mut(y);
            row.cells.fill(Cell::new('E', StyleId::DEFAULT, CellFlags::empty()));
            row.wrapped = false;
        }
        self.goto(0, 0);
        self.damage_all();
    }

    fn set_scroll_region(&mut self, top: usize, bottom: usize) {
        let (top, bottom) = (top - 1, bottom.min(self.lines()) - 1);
        if top < bottom {
            self.top = top;
            self.bottom = bottom;
            self.goto(0, 0);
        }
    }

    // --- modes -----------------------------------------------------------

    fn private_mode(m: u16) -> Option<TermMode> {
        Some(match m {
            1 => TermMode::APP_CURSOR,
            5 => TermMode::REVERSE_VIDEO,
            6 => TermMode::ORIGIN,
            7 => TermMode::AUTOWRAP,
            9 => TermMode::MOUSE_X10,
            12 => TermMode::CURSOR_BLINK,
            25 => TermMode::SHOW_CURSOR,
            47 | 1047 | 1049 => TermMode::ALT_SCREEN,
            1000 => TermMode::MOUSE_NORMAL,
            1002 => TermMode::MOUSE_BUTTON_MOTION,
            1003 => TermMode::MOUSE_ANY_MOTION,
            1004 => TermMode::FOCUS_EVENTS,
            1006 => TermMode::MOUSE_SGR,
            1007 => TermMode::ALTERNATE_SCROLL,
            1016 => TermMode::MOUSE_SGR_PIXELS,
            2004 => TermMode::BRACKETED_PASTE,
            2026 => TermMode::SYNC_OUTPUT,
            2027 => TermMode::GRAPHEME_CLUSTERING,
            2031 => TermMode::COLOR_SCHEME_UPDATES,
            2048 => TermMode::IN_BAND_RESIZE,
            80 => TermMode::SIXEL_DISPLAY,
            8452 => TermMode::SIXEL_CURSOR_RIGHT,
            _ => return None,
        })
    }

    fn set_private_modes(&mut self, args: &[u16], on: bool) {
        for &m in args {
            match m {
                47 => {
                    if on {
                        self.enter_alt(false, false)
                    } else {
                        self.leave_alt(false, false)
                    }
                }
                1047 => {
                    if on {
                        self.enter_alt(false, false)
                    } else {
                        self.leave_alt(false, true)
                    }
                }
                1049 => {
                    if on {
                        self.enter_alt(true, true)
                    } else {
                        self.leave_alt(true, false)
                    }
                }
                1048 => {
                    if on {
                        self.save_cursor()
                    } else {
                        self.restore_cursor()
                    }
                }
                9 | 1000 | 1002 | 1003 => {
                    self.mode.remove(TermMode::MOUSE_ANY);
                    if on {
                        self.mode.insert(Self::private_mode(m).expect("mouse mode"));
                    }
                }
                _ => {
                    let Some(bit) = Self::private_mode(m) else { continue };
                    self.mode.set(bit, on);
                    match m {
                        5 => self.damage_all(),
                        6 => self.goto(0, 0),
                        2048 if on => self.report_size(),
                        _ => {}
                    }
                }
            }
        }
    }

    fn set_ansi_modes(&mut self, args: &[u16], on: bool) {
        for &m in args {
            match m {
                4 => self.mode.set(TermMode::INSERT, on),
                20 => self.mode.set(TermMode::LINE_FEED_NEW_LINE, on),
                _ => {}
            }
        }
    }

    /// DECRQM reply: 1 = set, 2 = reset, 0 = not recognized.
    fn report_mode(&mut self, m: u16, private: bool) {
        let state = if private {
            match Self::private_mode(m) {
                Some(bit) => {
                    if self.mode.contains(bit) {
                        1
                    } else {
                        2
                    }
                }
                None => 0,
            }
        } else {
            match m {
                4 => {
                    if self.mode.contains(TermMode::INSERT) {
                        1
                    } else {
                        2
                    }
                }
                20 => {
                    if self.mode.contains(TermMode::LINE_FEED_NEW_LINE) {
                        1
                    } else {
                        2
                    }
                }
                _ => 0,
            }
        };
        let q = if private { "?" } else { "" };
        self.reply(&format!("\x1b[{q}{m};{state}$y"));
    }

    fn set_cursor_style(&mut self, n: usize) {
        self.cursor_shape = match n {
            0..=2 => CursorShape::Block,
            3 | 4 => CursorShape::Underline,
            5 | 6 => CursorShape::Bar,
            _ => return,
        };
        self.mode.set(TermMode::CURSOR_BLINK, n == 0 || n % 2 == 1);
        self.damage_line(self.cursor.y);
    }

    fn report_size(&mut self) {
        let (w, h) = self.cell_px;
        let (cols, lines) = (self.cols(), self.lines());
        self.reply(&format!("\x1b[48;{lines};{cols};{};{}t", lines as u32 * h, cols as u32 * w));
    }

    fn report_color_scheme(&mut self) {
        let v = if self.dark { 1 } else { 2 };
        self.reply(&format!("\x1b[?997;{v}n"));
    }

    fn device_status(&mut self, n: usize, private: bool) {
        let y =
            if self.mode.contains(TermMode::ORIGIN) { self.cursor.y.saturating_sub(self.top) } else { self.cursor.y };
        match (n, private) {
            (5, false) => self.reply("\x1b[0n"),
            (6, false) => self.reply(&format!("\x1b[{};{}R", y + 1, self.cursor.x + 1)),
            (6, true) => self.reply(&format!("\x1b[?{};{}R", y + 1, self.cursor.x + 1)),
            (996, true) => self.report_color_scheme(),
            _ => {}
        }
    }

    /// XTWINOPS subset: size reports and the title stack. Never reports the title.
    fn window_op(&mut self, op: usize, arg: usize) {
        let (w, h) = self.cell_px;
        let (cols, lines) = (self.cols() as u32, self.lines() as u32);
        match op {
            14 => self.reply(&format!("\x1b[4;{};{}t", lines * h, cols * w)),
            16 => self.reply(&format!("\x1b[6;{h};{w}t")),
            18 => self.reply(&format!("\x1b[8;{lines};{cols}t")),
            22 if arg == 0 || arg == 2 => {
                if self.title_stack.len() >= MAX_TITLE_STACK {
                    self.title_stack.remove(0);
                }
                self.title_stack.push(self.title.clone());
            }
            23 if arg == 0 || arg == 2 => {
                if let Some(t) = self.title_stack.pop() {
                    self.set_title(t);
                }
            }
            _ => {}
        }
    }

    fn set_title(&mut self, mut title: String) {
        if title.len() > MAX_TITLE {
            let mut end = MAX_TITLE;
            while !title.is_char_boundary(end) {
                end -= 1;
            }
            title.truncate(end);
        }
        if title != self.title {
            self.title = title.clone();
            self.events.push(Event::TitleChanged(title));
        }
    }

    // --- kitty keyboard protocol flags ----------------------------------

    fn kbd_push(&mut self, flags: usize) {
        if self.kbd.len() > MAX_KBD_STACK {
            self.kbd.remove(1);
        }
        self.kbd.push(KeyboardFlags::from_bits_truncate(flags as u8));
    }

    fn kbd_pop(&mut self, n: usize) {
        if n >= self.kbd.len() - 1 {
            self.kbd = vec![KeyboardFlags::empty()];
        } else {
            self.kbd.truncate(self.kbd.len() - n);
        }
    }

    fn kbd_set(&mut self, flags: usize, how: usize) {
        let f = KeyboardFlags::from_bits_truncate(flags as u8);
        let top = self.kbd.last_mut().expect("base entry");
        match how {
            1 => *top = f,
            2 => *top |= f,
            3 => *top &= !f,
            _ => {}
        }
    }

    // --- SGR -------------------------------------------------------------

    fn sgr(&mut self, params: &Params) {
        let mut buf: [&[u16]; PARAMS] = [&[]; PARAMS];
        let mut n = 0;
        for p in params.iter().take(PARAMS) {
            buf[n] = p;
            n += 1;
        }
        let ps = &buf[..n];
        let st = &mut self.cursor.style;
        if ps.is_empty() {
            let link = st.hyperlink;
            *st = Style { hyperlink: link, ..Style::default() };
        }
        let mut i = 0;
        while i < ps.len() {
            let p = ps[i];
            match p[0] {
                0 => {
                    let link = st.hyperlink;
                    *st = Style { hyperlink: link, ..Style::default() };
                }
                1 => st.attrs.insert(Attrs::BOLD),
                2 => st.attrs.insert(Attrs::DIM),
                3 => st.attrs.insert(Attrs::ITALIC),
                4 => {
                    st.underline = match p.get(1) {
                        None | Some(1) => Underline::Single,
                        Some(0) => Underline::None,
                        Some(2) => Underline::Double,
                        Some(3) => Underline::Curly,
                        Some(4) => Underline::Dotted,
                        Some(5) => Underline::Dashed,
                        Some(_) => Underline::Single,
                    }
                }
                5 | 6 => st.attrs.insert(Attrs::BLINK),
                7 => st.attrs.insert(Attrs::INVERSE),
                8 => st.attrs.insert(Attrs::HIDDEN),
                9 => st.attrs.insert(Attrs::STRIKE),
                21 => st.underline = Underline::Double,
                22 => st.attrs.remove(Attrs::BOLD | Attrs::DIM),
                23 => st.attrs.remove(Attrs::ITALIC),
                24 => st.underline = Underline::None,
                25 => st.attrs.remove(Attrs::BLINK),
                27 => st.attrs.remove(Attrs::INVERSE),
                28 => st.attrs.remove(Attrs::HIDDEN),
                29 => st.attrs.remove(Attrs::STRIKE),
                n @ 30..=37 => st.fg = Color::Indexed((n - 30) as u8),
                n @ 40..=47 => st.bg = Color::Indexed((n - 40) as u8),
                n @ 90..=97 => st.fg = Color::Indexed((n - 90 + 8) as u8),
                n @ 100..=107 => st.bg = Color::Indexed((n - 100 + 8) as u8),
                39 => st.fg = Color::Default,
                49 => st.bg = Color::Default,
                59 => st.underline_color = Color::Default,
                53 => st.attrs.insert(Attrs::OVERLINE),
                55 => st.attrs.remove(Attrs::OVERLINE),

                38 | 48 | 58 => {
                    let (color, used) = parse_extended_color(ps, i);
                    i += used;
                    if let Some(c) = color {
                        match p[0] {
                            38 => st.fg = c,
                            48 => st.bg = c,
                            _ => st.underline_color = c,
                        }
                    }
                }
                _ => {}
            }
            i += 1;
        }
        self.sync_style();
    }
}

/// Parse `38;5;n`, `38;2;r;g;b` or the colon forms `38:5:n`, `38:2::r:g:b`,
/// `38:2:r:g:b`. Returns the color and how many extra *params* were consumed.
fn parse_extended_color(ps: &[&[u16]], i: usize) -> (Option<Color>, usize) {
    let to8 = |v: u16| v.min(255) as u8;
    let p = ps[i];
    if p.len() > 1 {
        let color = match p[1] {
            5 => p.get(2).map(|&n| Color::Indexed(to8(n))),
            2 => {
                let rgb = match p.len() {
                    n if n >= 6 => &p[3..6],
                    5 => &p[2..5],
                    _ => return (None, 0),
                };
                Some(Color::Rgb(Rgb::new(to8(rgb[0]), to8(rgb[1]), to8(rgb[2]))))
            }
            _ => None,
        };
        return (color, 0);
    }
    match ps.get(i + 1).map(|s| s[0]) {
        Some(5) => match ps.get(i + 2) {
            Some(n) => (Some(Color::Indexed(to8(n[0]))), 2),
            None => (None, 1),
        },
        Some(2) if i + 4 < ps.len() => {
            let c = Rgb::new(to8(ps[i + 2][0]), to8(ps[i + 3][0]), to8(ps[i + 4][0]));
            (Some(Color::Rgb(c)), 4)
        }
        Some(2) => (None, ps.len() - i - 1),
        _ => (None, 0),
    }
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn join_params(params: &[&[u8]], from: usize) -> String {
    let parts: Vec<&[u8]> = params.get(from..).unwrap_or_default().to_vec();
    lossy(&parts.join(&b';'))
}

fn truncated(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

/// A command line reported by the program; one with control characters
/// could hide lines from what is shown and is dropped.
fn command_text(s: String) -> Option<String> {
    (!s.chars().any(char::is_control)).then(|| truncated(s, MAX_TEXT))
}

// --- OSC handlers ---------------------------------------------------------

impl Terminal {
    fn osc(&mut self, params: &[&[u8]], term: Terminator) {
        let Some(&cmd) = params.first() else { return };
        match cmd {
            b"0" | b"2" => self.set_title(join_params(params, 1)),
            b"4" => self.osc_palette(params, term),
            b"104" => {
                if params.len() < 2 || params[1].is_empty() {
                    self.palette.reset_indexed(None);
                } else {
                    for p in &params[1..] {
                        if let Ok(i) = lossy(p).parse::<u8>() {
                            self.palette.reset_indexed(Some(i));
                        }
                    }
                }
                self.damage_all();
            }
            b"10" | b"11" | b"12" => self.osc_special(params, term),
            b"110" | b"111" | b"112" => {
                let which = match cmd {
                    b"110" => Special::Foreground,
                    b"111" => Special::Background,
                    _ => Special::Cursor,
                };
                self.palette.set_special(which, None);
                self.damage_all();
            }
            b"7" => {
                let url = join_params(params, 1);
                if url.len() <= MAX_TEXT
                    && let Some(cwd) = text::parse_cwd_url(&url)
                {
                    if self.cwd.as_ref() != Some(&cwd) {
                        self.events.push(Event::CwdChanged(cwd.clone()));
                    }
                    self.cwd = Some(cwd);
                }
            }
            b"8" => self.osc_hyperlink(params),
            b"52" => self.osc_clipboard(params, term),
            b"133" => self.osc_shell_mark(params, false),
            b"633" => self.osc_shell_mark(params, true),
            b"1337" if self.config.graphics.enabled => self.osc_iterm(params, term),
            b"9" => {
                // OSC 9;4 is ConEmu progress, not a notification.
                if !(params.get(1) == Some(&&b"4"[..]) && params.len() > 2) {
                    self.notify(None, join_params(params, 1));
                }
            }
            b"777" if params.get(1) == Some(&&b"notify"[..]) => {
                let title = params.get(2).map(|t| lossy(t));
                self.notify(title, join_params(params, 3));
            }
            _ => {}
        }
    }

    /// OSC 4;index;spec[;index;spec...] — set or query (`?`) palette entries.
    fn osc_palette(&mut self, params: &[&[u8]], term: Terminator) {
        for pair in params[1..].chunks(2) {
            let [idx, spec] = pair else { break };
            let Ok(i) = lossy(idx).parse::<u8>() else { continue };
            if *spec == b"?" {
                let c = self.palette.indexed(i).to_x11();
                self.reply(&format!("\x1b]4;{i};{c}{}", term.as_str()));
            } else if let Some(c) = Rgb::parse(&lossy(spec)) {
                self.palette.set_indexed(i, c);
                self.damage_all();
            }
        }
    }

    /// OSC 10/11/12 — successive params address the following special colors.
    fn osc_special(&mut self, params: &[&[u8]], term: Terminator) {
        let first: u8 = lossy(params[0]).parse().unwrap_or(10);
        for (which, p) in (first..).zip(&params[1..]) {
            let sp = match which {
                10 => Special::Foreground,
                11 => Special::Background,
                12 => Special::Cursor,
                _ => break,
            };
            if *p == b"?" {
                let c = self.palette.special(sp).to_x11();
                self.reply(&format!("\x1b]{which};{c}{}", term.as_str()));
            } else if let Some(c) = Rgb::parse(&lossy(p)) {
                self.palette.set_special(sp, Some(c));
                self.damage_all();
            }
        }
    }

    /// OSC 8;params;uri — start (or end, with empty uri) a hyperlink.
    fn osc_hyperlink(&mut self, params: &[&[u8]]) {
        if self.links.len() >= self.link_limit {
            self.compact_styles();
        }
        let uri = join_params(params, 2);
        self.cursor.style.hyperlink = if uri.is_empty() || uri.len() > MAX_URI {
            None
        } else {
            let id = params
                .get(1)
                .and_then(|p| lossy(p).split(':').find_map(|kv| kv.strip_prefix("id=").map(str::to_string)))
                .filter(|id| id.len() <= MAX_LINK_ID);
            Some(self.links.intern(Hyperlink { id, uri }))
        };
        self.sync_style();
    }

    /// OSC 52;selection;base64|? — writes are allowed by policy, reads go through the host.
    fn osc_clipboard(&mut self, params: &[&[u8]], term: Terminator) {
        let sel = params.get(1).copied().unwrap_or(b"c");
        let Some(&data) = params.get(2) else { return };
        let primary = !sel.is_empty() && !sel.contains(&b'c') && sel.iter().any(|b| matches!(b, b'p' | b's'));
        let kind = if primary { ClipboardKind::Primary } else { ClipboardKind::Clipboard };
        if data == b"?" {
            if self.config.osc52_read != ClipboardRead::Deny {
                self.events.push(Event::ClipboardLoad { kind, terminator: term });
            }
            return;
        }
        if !self.config.osc52_write || data.len() > MAX_OSC52 {
            if data.len() > MAX_OSC52 {
                tracing::debug!(kind = "osc 52", size = data.len(), max = MAX_OSC52, "dropping oversized string");
            }
            return;
        }
        if let Ok(bytes) = BASE64.decode(data) {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            self.events.push(Event::ClipboardStore { kind, text });
        }
    }

    fn notify(&mut self, title: Option<String>, body: String) {
        let now = Instant::now();
        while self.notify_times.front().is_some_and(|t| now.duration_since(*t) > self.config.notify_window) {
            self.notify_times.pop_front();
        }
        if self.notify_times.len() >= self.config.notify_burst {
            tracing::debug!(
                burst = self.config.notify_burst,
                window = ?self.config.notify_window,
                "notification dropped by rate limit"
            );
            return;
        }
        self.notify_times.push_back(now);
        let title = title.map(|t| truncated(t, MAX_TEXT));
        self.events.push(Event::Notification { title, body: truncated(body, MAX_TEXT) });
    }

    /// OSC 133 (FinalTerm) and OSC 633 (VS Code) shell-integration marks.
    fn osc_shell_mark(&mut self, params: &[&[u8]], vscode: bool) {
        // Marks on the alternate screen would point into the wrong grid.
        if self.is_alt_screen() {
            return;
        }
        let Some(kind) = params.get(1).and_then(|p| p.first()) else { return };
        let kv = |key: &str| params[2..].iter().find_map(|p| lossy(p).strip_prefix(key).map(str::to_string));
        match kind {
            b'A' => self.mark_prompt(),
            b'B' => self.mark_input(),
            b'C' => {
                let cmd = kv("cmdline_url=").map(|s| text::percent_decode(&s)).or_else(|| kv("cmdline="));
                self.mark_output(cmd.and_then(command_text));
            }
            b'D' => {
                let code = params.get(2).and_then(|p| lossy(p).trim().parse::<i32>().ok());
                self.mark_finished(code);
            }
            b'E' if vscode => {
                let cmd = join_params(params, 2);
                // Only the first `;` separates the nonce; VS Code escapes the rest.
                let cmd = cmd.split(';').next().unwrap_or_default();
                self.pending_command = command_text(cmd.replace("\\x3b", ";").replace("\\\\", "\\"));
            }
            _ => {}
        }
    }

    fn cursor_point(&self) -> Point {
        Point::new(self.grid.abs_line(self.cursor.y), self.cursor.x)
    }

    fn mark_prompt(&mut self) {
        let here = self.cursor_point();
        // A running block whose `D` never came ends at the new prompt.
        if let Some(b) = self.blocks.last_mut()
            && b.state == BlockState::Running
        {
            b.state = BlockState::Finished;
            b.finished_at = Some(Instant::now());
            b.end = Some(Point::new(here.line, 0));
            let ev = Event::CommandFinished { block: b.id, exit_code: None, duration: b.duration() };
            self.events.push(ev);
        }
        self.pending_command = None;
        let cwd = self.cwd.as_ref().map(|c| c.path.clone());
        let b = self.blocks.start(here.line);
        b.cwd = cwd;
        let id = b.id;
        self.events.push(Event::PromptStarted { block: id });
        self.damage_line(self.cursor.y);
    }

    fn mark_input(&mut self) {
        let here = self.cursor_point();
        if let Some(b) = self.blocks.last_mut()
            && b.state == BlockState::Prompt
        {
            b.input_start = Some(here);
            b.state = BlockState::Input;
        }
    }

    fn mark_output(&mut self, cmd: Option<String>) {
        let here = self.cursor_point();
        let last_col = self.cols() - 1;
        let Some(b) = self.blocks.last() else { return };
        if !matches!(b.state, BlockState::Prompt | BlockState::Input) {
            return;
        }
        // Without an explicit command line, read back what was typed after `B`.
        let typed = b.input_start.and_then(|start| {
            let end = if here.line > start.line {
                Point::new(here.line - 1, last_col)
            } else if here.col > start.col {
                Point::new(here.line, here.col - 1)
            } else {
                return None;
            };
            let s = text::extract(&self.grid, start, end, false).trim().to_string();
            (!s.is_empty()).then_some(s)
        });
        let command = cmd.filter(|c| !c.is_empty()).or(self.pending_command.take()).or(typed);
        let b = self.blocks.last_mut().expect("checked above");
        b.state = BlockState::Running;
        b.output_start = Some(here.line);
        b.started_at = Some(Instant::now());
        b.command = command.clone();
        let id = b.id;
        self.events.push(Event::CommandStarted { block: id, command });
        self.damage_line(self.cursor.y);
    }

    fn mark_finished(&mut self, exit_code: Option<i32>) {
        let here = self.cursor_point();
        let Some(b) = self.blocks.last_mut() else { return };
        match b.state {
            BlockState::Running => {
                b.state = BlockState::Finished;
                b.finished_at = Some(Instant::now());
                b.end = Some(here);
                b.exit_code = exit_code;
                let ev = Event::CommandFinished { block: b.id, exit_code, duration: b.duration() };
                self.events.push(ev);
                self.damage_all();
            }
            // Empty command line: nothing ran.
            BlockState::Prompt | BlockState::Input => b.state = BlockState::Finished,
            BlockState::Finished => {}
        }
    }
}

// --- DCS handlers -----------------------------------------------------------

/// Terminfo capabilities answered via XTGETTCAP. `None` = boolean capability.
fn termcap(name: &str) -> Option<Option<&'static str>> {
    Some(match name {
        "TN" | "name" => Some("proteus"),
        "Co" | "colors" => Some("256"),
        "RGB" => Some("8/8/8"),
        "Tc" | "Su" | "fullkbd" => None,
        "Smulx" => Some("\\E[4:%p1%dm"),
        "Setulc" => Some("\\E[58:2::%p1%{65536}%/%d:%p1%{256}%/%{255}%&%d:%p1%{255}%&%dm"),
        "Ss" => Some("\\E[%p1%d q"),
        "Se" => Some("\\E[2 q"),
        "Sync" => Some("\\E[?2026%?%p1%{1}%-%tl%eh%;"),
        "BE" => Some("\\E[?2004h"),
        "BD" => Some("\\E[?2004l"),
        "PS" => Some("\\E[200~"),
        "PE" => Some("\\E[201~"),
        _ => return None,
    })
}

fn hex_encode(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02X}")).collect()
}

fn hex_decode(s: &str) -> Option<String> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let bytes: Option<Vec<u8>> =
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect();
    String::from_utf8(bytes?).ok()
}

fn color_sgr(c: Color, base: u16, bright: u16, ext: u16) -> Option<String> {
    match c {
        Color::Default => None,
        Color::Indexed(i) if i < 8 => Some((base + i as u16).to_string()),
        Color::Indexed(i) if i < 16 => Some((bright + i as u16 - 8).to_string()),
        Color::Indexed(i) => Some(format!("{ext}:5:{i}")),
        Color::Rgb(c) => Some(format!("{ext}:2::{}:{}:{}", c.r, c.g, c.b)),
    }
}

fn sgr_report(s: &Style) -> String {
    let mut parts = vec!["0".to_string()];
    for (a, n) in [
        (Attrs::BOLD, "1"),
        (Attrs::DIM, "2"),
        (Attrs::ITALIC, "3"),
        (Attrs::BLINK, "5"),
        (Attrs::INVERSE, "7"),
        (Attrs::HIDDEN, "8"),
        (Attrs::STRIKE, "9"),
        (Attrs::OVERLINE, "53"),
    ] {
        if s.attrs.contains(a) {
            parts.push(n.to_string());
        }
    }
    let ul = match s.underline {
        Underline::None => None,
        Underline::Single => Some("4"),
        Underline::Double => Some("4:2"),
        Underline::Curly => Some("4:3"),
        Underline::Dotted => Some("4:4"),
        Underline::Dashed => Some("4:5"),
    };
    parts.extend(ul.map(str::to_string));
    parts.extend(color_sgr(s.fg, 30, 90, 38));
    parts.extend(color_sgr(s.bg, 40, 100, 48));
    if let Color::Indexed(i) = s.underline_color {
        parts.push(format!("58:5:{i}"));
    } else if let Color::Rgb(c) = s.underline_color {
        parts.push(format!("58:2::{}:{}:{}", c.r, c.g, c.b));
    }
    parts.join(";")
}

impl Terminal {
    fn dcs_finish(&mut self, dcs: Dcs) {
        if let Some(sixel) = dcs.sixel {
            self.sixel_finish(*sixel);
            return;
        }
        let data = String::from_utf8_lossy(&dcs.data).into_owned();
        match dcs.kind {
            DcsKind::Sixel => {}
            DcsKind::XtGetTcap => {
                for hex in data.split(';') {
                    if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                        self.reply("\x1bP0+r\x1b\\");
                        continue;
                    }
                    let cap = hex_decode(hex).and_then(|n| termcap(&n));
                    match cap {
                        Some(Some(v)) => self.reply(&format!("\x1bP1+r{hex}={}\x1b\\", hex_encode(v))),
                        Some(None) => self.reply(&format!("\x1bP1+r{hex}\x1b\\")),
                        None => self.reply(&format!("\x1bP0+r{hex}\x1b\\")),
                    }
                }
            }
            DcsKind::Decrqss => {
                let body = match data.as_str() {
                    "m" => Some(format!("{}m", sgr_report(&self.cursor.style))),
                    "r" => Some(format!("{};{}r", self.top + 1, self.bottom + 1)),
                    " q" => {
                        let base = match self.cursor_shape {
                            CursorShape::Block => 1,
                            CursorShape::Underline => 3,
                            CursorShape::Bar => 5,
                        };
                        let n = if self.mode.contains(TermMode::CURSOR_BLINK) { base } else { base + 1 };
                        Some(format!("{n} q"))
                    }
                    _ => None,
                };
                match body {
                    Some(b) => self.reply(&format!("\x1bP1$r{b}\x1b\\")),
                    None => self.reply("\x1bP0$r\x1b\\"),
                }
            }
        }
    }
}

// --- parser callbacks -------------------------------------------------------

impl Perform for Terminal {
    fn print(&mut self, c: char) {
        self.print_char(c);
    }

    fn execute(&mut self, byte: u8) {
        self.last_char = None;
        match byte {
            0x07 => self.events.push(Event::Bell),
            0x08 => self.move_left(1),
            0x09 => self.tab_forward(1),
            0x0a..=0x0c => {
                self.linefeed();
                if self.mode.contains(TermMode::LINE_FEED_NEW_LINE) {
                    self.cursor.x = 0;
                }
            }
            0x0d => {
                self.cursor.x = 0;
                self.cursor.pending_wrap = false;
            }
            0x0e => self.cursor.gl = 1,
            0x0f => self.cursor.gl = 0,
            0x84 => self.linefeed(),
            0x85 => {
                self.cursor.x = 0;
                self.linefeed();
            }
            0x88 => self.tabs[self.cursor.x] = true,
            0x8d => self.reverse_index(),
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        self.last_char = None;
        if ignore {
            return;
        }
        match (intermediates, byte) {
            ([], b'7') => self.save_cursor(),
            ([], b'8') => self.restore_cursor(),
            ([b'#'], b'8') => self.alignment_test(),
            ([], b'D') => self.linefeed(),
            ([], b'E') => {
                self.cursor.x = 0;
                self.linefeed();
            }
            ([], b'H') => self.tabs[self.cursor.x] = true,
            ([], b'M') => self.reverse_index(),
            ([], b'c') => self.full_reset(),
            ([], b'=') => self.mode.insert(TermMode::APP_KEYPAD),
            ([], b'>') => self.mode.remove(TermMode::APP_KEYPAD),
            ([i @ (b'(' | b')' | b'*' | b'+')], c) => {
                let slot = (i - b'(') as usize;
                self.cursor.charsets[slot] = if c == b'0' { Charset::DecSpecial } else { Charset::Ascii };
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        let last = self.last_char.take();
        if ignore {
            return;
        }
        let mut argbuf = [0u16; PARAMS];
        let mut n = 0;
        for p in params.iter().take(PARAMS) {
            argbuf[n] = p[0];
            n += 1;
        }
        let args = &argbuf[..n];
        // Most parameters treat 0 like "missing".
        let arg = |i: usize, d: usize| match args.get(i) {
            None | Some(0) => d,
            Some(&v) => v as usize,
        };
        let raw = |i: usize| args.get(i).map_or(0, |&v| v as usize);
        match (intermediates, action) {
            ([], '@') => self.insert_blank(arg(0, 1)),
            ([], 'A') => self.move_up(arg(0, 1)),
            ([], 'B' | 'e') => self.move_down(arg(0, 1)),
            ([], 'C' | 'a') => self.move_right(arg(0, 1)),
            ([], 'D') => self.move_left(arg(0, 1)),
            ([], 'E') => {
                self.move_down(arg(0, 1));
                self.cursor.x = 0;
            }
            ([], 'F') => {
                self.move_up(arg(0, 1));
                self.cursor.x = 0;
            }
            ([], 'G' | '`') => self.set_x(arg(0, 1) - 1),
            ([], 'H' | 'f') => self.goto(arg(1, 1) - 1, arg(0, 1) - 1),
            ([], 'I') => self.tab_forward(arg(0, 1)),
            ([] | [b'?'], 'J') => self.erase_display(raw(0)),
            ([] | [b'?'], 'K') => self.erase_line(raw(0)),
            ([], 'L') => self.insert_lines(arg(0, 1)),
            ([], 'M') => self.delete_lines(arg(0, 1)),
            ([], 'P') => self.delete_chars(arg(0, 1)),
            ([], 'S') => self.scroll_up(arg(0, 1)),
            ([], 'T') if args.len() <= 1 => self.scroll_down(arg(0, 1)),
            ([], 'X') => self.erase_chars(arg(0, 1)),
            ([], 'Z') => self.tab_backward(arg(0, 1)),
            ([], 'b') => self.repeat(last, arg(0, 1)),
            ([], 'c') if raw(0) == 0 => self.reply("\x1b[?62;4;22;52c"),
            ([b'>'], 'c') if raw(0) == 0 => {
                let v = version_number();
                self.reply(&format!("\x1b[>1;{v};0c"));
            }
            ([], 'd') => {
                let x = self.cursor.x;
                self.goto(x, arg(0, 1) - 1);
            }
            ([], 'g') => match raw(0) {
                0 => self.tabs[self.cursor.x] = false,
                3 => self.tabs.iter_mut().for_each(|t| *t = false),
                _ => {}
            },

            ([], 'h') => self.set_ansi_modes(args, true),
            ([], 'l') => self.set_ansi_modes(args, false),
            ([b'?'], 'h') => self.set_private_modes(args, true),
            ([b'?'], 'l') => self.set_private_modes(args, false),
            ([], 'm') => self.sgr(params),
            ([], 'n') => self.device_status(raw(0), false),
            ([b'?'], 'n') => self.device_status(raw(0), true),
            ([b' '], 'q') => self.set_cursor_style(raw(0)),
            ([b'>'], 'q') if raw(0) == 0 => self.reply(&format!("\x1bP>|proteus {VERSION}\x1b\\")),
            ([b'!'], 'p') => self.soft_reset(),
            ([b'$'], 'p') => self.report_mode(raw(0) as u16, false),
            ([b'?', b'$'], 'p') => self.report_mode(raw(0) as u16, true),
            ([], 'r') => {
                let lines = self.lines();
                self.set_scroll_region(arg(0, 1), arg(1, lines));
            }
            ([], 's') => self.save_cursor(),
            ([], 'u') => self.restore_cursor(),
            ([b'?'], 'u') => {
                let f = self.keyboard_flags().bits();
                self.reply(&format!("\x1b[?{f}u"));
            }
            ([b'>'], 'u') => self.kbd_push(raw(0)),
            ([b'<'], 'u') => self.kbd_pop(arg(0, 1)),
            ([b'='], 'u') => self.kbd_set(raw(0), arg(1, 1)),
            ([], 't') => self.window_op(raw(0), raw(1)),
            ([b'?'], 'S') => self.xtsmgraphics(raw(0), raw(1)),
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        self.last_char = None;
        if std::mem::take(&mut self.osc_drop) {
            return;
        }
        let term = if bell_terminated { Terminator::Bel } else { Terminator::St };
        self.osc(params, term);
    }

    fn hook(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        self.last_char = None;
        let kind = match (intermediates, action) {
            (b"+", 'q') => Some(DcsKind::XtGetTcap),
            (b"$", 'q') => Some(DcsKind::Decrqss),
            (b"", 'q') => Some(DcsKind::Sixel),
            _ => None,
        };
        let sixel = match kind {
            Some(DcsKind::Sixel) if !ignore => match self.new_sixel(params.iter().nth(1).map_or(0, |p| p[0])) {
                Some(dec) => Some(dec),
                None => {
                    self.dcs = None;
                    return;
                }
            },
            _ => None,
        };
        self.dcs = kind.filter(|_| !ignore).map(|kind| Dcs { kind, data: Vec::new(), sixel });
    }

    fn put(&mut self, byte: u8) {
        if let Some(d) = &mut self.dcs {
            if let Some(sixel) = &mut d.sixel {
                sixel.put(byte);
            } else if d.data.len() < MAX_DCS {
                d.data.push(byte);
            }
        }
    }

    fn unhook(&mut self) {
        if let Some(d) = self.dcs.take() {
            if d.sixel.is_none() && d.data.len() >= MAX_DCS {
                tracing::debug!(kind = "dcs", max = MAX_DCS, "oversized string truncated");
            }
            self.dcs_finish(d);
        }
    }
}

/// `major * 10000 + minor * 100 + patch`, as reported in DA2.
fn version_number() -> u32 {
    let mut it = VERSION.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let (a, b, c) = (it.next().unwrap_or(0), it.next().unwrap_or(0), it.next().unwrap_or(0));
    a * 10000 + b * 100 + c
}

#[cfg(test)]
mod resource_tests {
    use super::*;

    #[test]
    fn hyperlink_churn_reclaims_metadata_and_preserves_saved_cursor() {
        let mut t = Terminal::new(8, 2, Config { scrollback: 2, ..Config::default() });
        t.advance(b"\x1b]8;;https://saved.example\x1b\\\x1b7");
        for i in 0..20_000 {
            t.advance(format!("\x1b]8;id={i};https://example.com/{i}\x1b\\x\r\n").as_bytes());
        }
        assert!(t.links.len() < 8192, "link metadata must track live cells, not total output");
        assert!(t.links.links_capacity_for_test() < 8192);
        t.advance(b"\x1b8");
        let id = t.cursor.style.hyperlink.unwrap();
        assert_eq!(t.hyperlink(id).unwrap().uri, "https://saved.example");
    }
}
