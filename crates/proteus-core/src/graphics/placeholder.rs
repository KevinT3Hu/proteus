//! Kitty Unicode placeholders: `U+10EEEE` cells whose colors and combining
//! diacritics encode which part of which image they show.
//!
//! Placeholders are ordinary text, so they scroll, reflow and pass through
//! multiplexers like any other character.

use crate::cell::Row;
use crate::color::Color;
use crate::style::StyleTable;

/// The placeholder character.
pub const PLACEHOLDER: char = '\u{10EEEE}';

/// Row/column diacritics, from kitty's `rowcolumn-diacritics.txt`; index = value.
static DIACRITICS: [u32; 297] = [
    0x0305, 0x030D, 0x030E, 0x0310, 0x0312, 0x033D, 0x033E, 0x033F, 0x0346, 0x034A, 0x034B, 0x034C, 0x0350, 0x0351,
    0x0352, 0x0357, 0x035B, 0x0363, 0x0364, 0x0365, 0x0366, 0x0367, 0x0368, 0x0369, 0x036A, 0x036B, 0x036C, 0x036D,
    0x036E, 0x036F, 0x0483, 0x0484, 0x0485, 0x0486, 0x0487, 0x0592, 0x0593, 0x0594, 0x0595, 0x0597, 0x0598, 0x0599,
    0x059C, 0x059D, 0x059E, 0x059F, 0x05A0, 0x05A1, 0x05A8, 0x05A9, 0x05AB, 0x05AC, 0x05AF, 0x05C4, 0x0610, 0x0611,
    0x0612, 0x0613, 0x0614, 0x0615, 0x0616, 0x0617, 0x0657, 0x0658, 0x0659, 0x065A, 0x065B, 0x065D, 0x065E, 0x06D6,
    0x06D7, 0x06D8, 0x06D9, 0x06DA, 0x06DB, 0x06DC, 0x06DF, 0x06E0, 0x06E1, 0x06E2, 0x06E4, 0x06E7, 0x06E8, 0x06EB,
    0x06EC, 0x0730, 0x0732, 0x0733, 0x0735, 0x0736, 0x073A, 0x073D, 0x073F, 0x0740, 0x0741, 0x0743, 0x0745, 0x0747,
    0x0749, 0x074A, 0x07EB, 0x07EC, 0x07ED, 0x07EE, 0x07EF, 0x07F0, 0x07F1, 0x07F3, 0x0816, 0x0817, 0x0818, 0x0819,
    0x081B, 0x081C, 0x081D, 0x081E, 0x081F, 0x0820, 0x0821, 0x0822, 0x0823, 0x0825, 0x0826, 0x0827, 0x0829, 0x082A,
    0x082B, 0x082C, 0x082D, 0x0951, 0x0953, 0x0954, 0x0F82, 0x0F83, 0x0F86, 0x0F87, 0x135D, 0x135E, 0x135F, 0x17DD,
    0x193A, 0x1A17, 0x1A75, 0x1A76, 0x1A77, 0x1A78, 0x1A79, 0x1A7A, 0x1A7B, 0x1A7C, 0x1B6B, 0x1B6D, 0x1B6E, 0x1B6F,
    0x1B70, 0x1B71, 0x1B72, 0x1B73, 0x1CD0, 0x1CD1, 0x1CD2, 0x1CDA, 0x1CDB, 0x1CE0, 0x1DC0, 0x1DC1, 0x1DC3, 0x1DC4,
    0x1DC5, 0x1DC6, 0x1DC7, 0x1DC8, 0x1DC9, 0x1DCB, 0x1DCC, 0x1DD1, 0x1DD2, 0x1DD3, 0x1DD4, 0x1DD5, 0x1DD6, 0x1DD7,
    0x1DD8, 0x1DD9, 0x1DDA, 0x1DDB, 0x1DDC, 0x1DDD, 0x1DDE, 0x1DDF, 0x1DE0, 0x1DE1, 0x1DE2, 0x1DE3, 0x1DE4, 0x1DE5,
    0x1DE6, 0x1DFE, 0x20D0, 0x20D1, 0x20D4, 0x20D5, 0x20D6, 0x20D7, 0x20DB, 0x20DC, 0x20E1, 0x20E7, 0x20E9, 0x20F0,
    0x2CEF, 0x2CF0, 0x2CF1, 0x2DE0, 0x2DE1, 0x2DE2, 0x2DE3, 0x2DE4, 0x2DE5, 0x2DE6, 0x2DE7, 0x2DE8, 0x2DE9, 0x2DEA,
    0x2DEB, 0x2DEC, 0x2DED, 0x2DEE, 0x2DEF, 0x2DF0, 0x2DF1, 0x2DF2, 0x2DF3, 0x2DF4, 0x2DF5, 0x2DF6, 0x2DF7, 0x2DF8,
    0x2DF9, 0x2DFA, 0x2DFB, 0x2DFC, 0x2DFD, 0x2DFE, 0x2DFF, 0xA66F, 0xA67C, 0xA67D, 0xA6F0, 0xA6F1, 0xA8E0, 0xA8E1,
    0xA8E2, 0xA8E3, 0xA8E4, 0xA8E5, 0xA8E6, 0xA8E7, 0xA8E8, 0xA8E9, 0xA8EA, 0xA8EB, 0xA8EC, 0xA8ED, 0xA8EE, 0xA8EF,
    0xA8F0, 0xA8F1, 0xAAB0, 0xAAB2, 0xAAB3, 0xAAB7, 0xAAB8, 0xAABE, 0xAABF, 0xAAC1, 0xFE20, 0xFE21, 0xFE22, 0xFE23,
    0xFE24, 0xFE25, 0xFE26, 0x10A0F, 0x10A38, 0x1D185, 0x1D186, 0x1D187, 0x1D188, 0x1D189, 0x1D1AA, 0x1D1AB, 0x1D1AC,
    0x1D1AD, 0x1D242, 0x1D243, 0x1D244,
];

/// Number encoded by a row/column diacritic.
pub fn diacritic_value(c: char) -> Option<u32> {
    DIACRITICS.binary_search(&(c as u32)).ok().map(|i| i as u32)
}

/// Diacritic encoding `n` (0..297).
pub fn diacritic(n: u32) -> Option<char> {
    DIACRITICS.get(n as usize).and_then(|&c| char::from_u32(c))
}

/// A horizontal run of placeholder cells showing consecutive columns of one
/// image row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    /// First grid column of the run.
    pub x: usize,
    pub len: usize,
    pub image_id: u32,
    /// 0 = any virtual placement of the image.
    pub placement_id: u32,
    /// Image cell row and first image cell column shown by the run.
    pub row: u32,
    pub col: u32,
}

fn color_id(c: Color) -> u32 {
    match c {
        Color::Default => 0,
        Color::Indexed(i) => i as u32,
        Color::Rgb(c) => (c.r as u32) << 16 | (c.g as u32) << 8 | c.b as u32,
    }
}

#[derive(Clone, Copy)]
struct Prev {
    x: usize,
    fg: u32,
    ul: u32,
    row: u32,
    col: u32,
    msb: u32,
}

/// Resolve the placeholder cells of a row, applying kitty's inheritance rules
/// for omitted diacritics.
pub fn runs(row: &Row, styles: &StyleTable) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::new();
    let mut prev: Option<Prev> = None;
    for (x, cell) in row.cells.iter().enumerate() {
        if cell.ch() != PLACEHOLDER {
            continue;
        }
        let style = styles.get(cell.style);
        let (fg, ul) = (color_id(style.fg), color_id(style.underline_color));
        let mut d = [0u32; 3];
        let mut n = 0;
        for c in cell.extra().unwrap_or("").chars() {
            match diacritic_value(c) {
                Some(v) if n < 3 => {
                    d[n] = v;
                    n += 1;
                }
                _ => break,
            }
        }
        let p = prev.filter(|p| p.x + 1 == x && p.fg == fg && p.ul == ul);
        let (r, c, msb) = match n {
            0 => p.map_or((0, 0, 0), |p| (p.row, p.col + 1, p.msb)),
            1 => match p.filter(|p| p.row == d[0]) {
                Some(p) => (d[0], p.col + 1, p.msb),
                None => (d[0], 0, 0),
            },
            2 => {
                let msb = p.filter(|p| p.row == d[0] && p.col + 1 == d[1]).map_or(0, |p| p.msb);
                (d[0], d[1], msb)
            }
            _ => (d[0], d[1], d[2]),
        };
        prev = Some(Prev { x, fg, ul, row: r, col: c, msb });
        let image_id = (fg & 0xff_ffff) | (msb & 0xff) << 24;
        if image_id == 0 {
            continue;
        }
        if let Some(last) = out.last_mut()
            && last.x + last.len == x
            && last.image_id == image_id
            && last.placement_id == ul
            && last.row == r
            && last.col + last.len as u32 == c
        {
            last.len += 1;
            continue;
        }
        out.push(Run { x, len: 1, image_id, placement_id: ul, row: r, col: c });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_sorted() {
        assert!(DIACRITICS.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(diacritic_value('\u{0305}'), Some(0));
        assert_eq!(diacritic_value('\u{030D}'), Some(1));
        assert_eq!(diacritic(2), Some('\u{030E}'));
        assert_eq!(diacritic_value('a'), None);
    }
}
