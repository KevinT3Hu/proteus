//! Typography and initial window geometry, in logical pixels unless noted.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FontSettings {
    pub bold_family: Option<String>,
    pub italic_family: Option<String>,
    pub bold_italic_family: Option<String>,
    pub cell_width: Option<f64>,
    pub cell_height: Option<f64>,
    pub letter_spacing: f64,
    pub line_spacing: f64,
    pub offset_x: f64,
    pub offset_y: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct WindowSettings {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub columns: Option<u32>,
    pub rows: Option<u32>,
}

impl WindowSettings {
    /// Pixel dimensions win over grid dimensions, independently on each axis.
    pub fn size(&self, cell: (f64, f64), padding: (f64, f64), bars: u32, title: bool, gutter: f64) -> (u32, u32) {
        let x = (padding.0.max(gutter) + padding.0).ceil();
        let y = 2.0 * padding.1 + bars as f64 * (cell.1 + padding.1) + if title { cell.1 } else { 0.0 };
        (
            self.width.unwrap_or_else(|| (self.columns.unwrap_or(80) as f64 * cell.0 + x).ceil() as u32).max(100),
            self.height.unwrap_or_else(|| (self.rows.unwrap_or(24) as f64 * cell.1 + y).ceil() as u32).max(60),
        )
    }
}
