//! Local translation catalogs following the terminal's configured language.
pub use proteus_i18n::{Catalog, FluentArgs, FluentValue};

/// Create a catalog using the host locale rather than the WASI environment.
pub fn catalog() -> Result<Catalog, String> {
    Catalog::new(&crate::host::locale()?)
}
/// Call on `Event::ConfigChanged`, then redraw and refresh action labels/status.
pub fn sync(catalog: &mut Catalog) -> Result<(), String> {
    catalog.set_locale(&crate::host::locale()?)
}
