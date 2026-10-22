//! Plugins shipped inside the terminal (built by `build.rs`).

use proteus_wasm::store::Store;

/// (manifest, component); the component is empty when the build had no
/// `wasm32-wasip2` target.
const FILES: (&str, &[u8]) = (
    include_str!("../../../plugins/files/proteus-plugin.toml"),
    include_bytes!(concat!(env!("OUT_DIR"), "/proteus_files.wasm")),
);

fn all() -> impl Iterator<Item = (&'static str, &'static [u8])> {
    [FILES].into_iter().filter(|(_, w)| !w.is_empty())
}

/// Install new bundled plugins and update changed ones (not those the
/// user removed or replaced). Returns the errors.
pub fn install(store: &Store) -> Vec<String> {
    all().filter_map(|(m, w)| store.install_bundled(m, w, false).err()).collect()
}

/// `+plugin install ID` of a bundled plugin: back after a removal.
pub fn reinstall(store: &Store, id: &str) -> Option<Result<(), String>> {
    let (m, w) = all().find(|(m, _)| proteus_wasm::manifest::Manifest::parse(m).is_ok_and(|m| m.id == id))?;
    Some(store.install_bundled(m, w, true).map(|_| ()))
}
