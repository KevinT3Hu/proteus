//! Cell styles and hyperlinks, interned so each cell stores a small id.

use std::collections::HashMap;
use std::hash::BuildHasher;

use bitflags::bitflags;

use crate::color::Color;

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct Attrs: u16 {
        const BOLD = 1 << 0;
        const DIM = 1 << 1;
        const ITALIC = 1 << 2;
        const BLINK = 1 << 3;
        const INVERSE = 1 << 4;
        const HIDDEN = 1 << 5;
        const STRIKE = 1 << 6;
        const OVERLINE = 1 << 7;
    }
}

/// Underline style (`CSI 4:n m`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Underline {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

/// Interned hyperlink id (OSC 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HyperlinkId(pub u32);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Hyperlink {
    /// The `id=` parameter; cells with the same id + uri belong to one link.
    pub id: Option<String>,
    pub uri: String,
}

/// Full visual style of a cell.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    /// `Color::Default` means "same as foreground".
    pub underline_color: Color,
    pub underline: Underline,
    pub attrs: Attrs,
    pub hyperlink: Option<HyperlinkId>,
}

impl Style {
    /// The style used for erased cells: only the background survives (BCE).
    pub fn erased(&self) -> Style {
        Style { bg: self.bg, ..Style::default() }
    }
}

/// Interned style id. `StyleId::DEFAULT` is always the default style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct StyleId(pub u32);

impl StyleId {
    pub const DEFAULT: StyleId = StyleId(0);
}

/// Intern table for styles. Ids are stable until the terminal compacts it.
/// The index holds only ids; styles are stored once.
pub struct StyleTable {
    styles: Vec<Style>,
    index: hashbrown::HashTable<u32>,
    /// Randomly seeded: styles come from application output.
    hasher: foldhash::fast::RandomState,
}

impl std::fmt::Debug for StyleTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StyleTable").field("len", &self.styles.len()).finish()
    }
}

impl Default for StyleTable {
    fn default() -> Self {
        let mut table = Self { styles: Vec::new(), index: hashbrown::HashTable::new(), hasher: Default::default() };
        table.intern(&Style::default());
        table
    }
}

impl StyleTable {
    pub fn intern(&mut self, style: &Style) -> StyleId {
        let hash = self.hasher.hash_one(style);
        if let Some(&id) = self.index.find(hash, |&id| self.styles[id as usize] == *style) {
            return StyleId(id);
        }
        let id = self.styles.len() as u32;
        self.styles.push(style.clone());
        let (styles, hasher) = (&self.styles, &self.hasher);
        self.index.insert_unique(hash, id, |&id| hasher.hash_one(&styles[id as usize]));
        StyleId(id)
    }

    /// All styles by id, consuming the table.
    pub(crate) fn into_styles(self) -> Vec<Style> {
        self.styles
    }

    pub fn get(&self, id: StyleId) -> &Style {
        self.styles.get(id.0 as usize).unwrap_or(&self.styles[0])
    }

    pub fn len(&self) -> usize {
        self.styles.len()
    }

    pub fn is_empty(&self) -> bool {
        false
    }
}

/// Intern table for hyperlinks.
#[derive(Debug, Default)]
pub struct HyperlinkTable {
    links: Vec<Option<Hyperlink>>,
    free: Vec<u32>,
    map: HashMap<Hyperlink, HyperlinkId>,
}

impl HyperlinkTable {
    #[cfg(test)]
    pub(crate) fn links_capacity_for_test(&self) -> usize {
        self.links.len()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn retain(&mut self, live: &std::collections::HashSet<HyperlinkId>) {
        self.map.retain(|_, id| live.contains(id));
        for (i, slot) in self.links.iter_mut().enumerate() {
            if slot.is_some() && !live.contains(&HyperlinkId(i as u32)) {
                *slot = None;
                self.free.push(i as u32);
            }
        }
    }

    pub fn intern(&mut self, link: Hyperlink) -> HyperlinkId {
        if let Some(&id) = self.map.get(&link) {
            return id;
        }
        let id = if let Some(slot) = self.free.pop() {
            self.links[slot as usize] = Some(link.clone());
            HyperlinkId(slot)
        } else {
            let id = HyperlinkId(self.links.len() as u32);
            self.links.push(Some(link.clone()));
            id
        };
        self.map.insert(link, id);
        id
    }

    pub fn get(&self, id: HyperlinkId) -> Option<&Hyperlink> {
        self.links.get(id.0 as usize).and_then(Option::as_ref)
    }
}
