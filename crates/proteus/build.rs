//! Builds the bundled plugins (`plugins/*`, for `wasm32-wasip2`) so the
//! terminal can embed them. `PROTEUS_FILES_WASM=path` uses a prebuilt
//! component instead; `PROTEUS_NO_BUNDLE=1` embeds none. If the build
//! fails (no `wasm32-wasip2` target), the terminal is built without it.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("../..");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("proteus_files.wasm");
    for p in ["plugins/files/src", "plugins/files/Cargo.toml", "plugins/files/Cargo.lock", "crates/proteus-plugin/src"]
    {
        println!("cargo:rerun-if-changed={}", root.join(p).display());
    }
    println!("cargo:rerun-if-changed={}", root.join("crates/proteus-plugin/wit").display());
    println!("cargo:rerun-if-env-changed=PROTEUS_FILES_WASM");
    println!("cargo:rerun-if-env-changed=PROTEUS_NO_BUNDLE");

    let wasm = if std::env::var_os("PROTEUS_NO_BUNDLE").is_some() {
        Err("PROTEUS_NO_BUNDLE is set".to_string())
    } else if let Some(p) = std::env::var_os("PROTEUS_FILES_WASM") {
        std::fs::read(&p).map_err(|e| format!("{}: {e}", Path::new(&p).display()))
    } else {
        build(&root.join("plugins/files"))
    };
    let bytes = wasm.unwrap_or_else(|e| {
        println!("cargo:warning=built without the bundled file manager: {e}");
        Vec::new()
    });
    std::fs::write(&out, bytes).unwrap();
}

fn build(dir: &Path) -> Result<Vec<u8>, String> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut c = Command::new(cargo);
    c.args(["build", "--release", "--target", "wasm32-wasip2", "--quiet"]).current_dir(dir);
    // Settings of the outer build are not for this one.
    for (k, _) in std::env::vars_os() {
        let k = k.to_string_lossy().into_owned();
        if k.starts_with("CARGO_") && k != "CARGO_HOME" || k == "RUSTFLAGS" || k == "RUSTC_WRAPPER" {
            c.env_remove(&k);
        }
    }
    let status = c.status().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!("cargo build in {} failed", dir.display()));
    }
    let wasm = dir.join("target/wasm32-wasip2/release/proteus_files.wasm");
    std::fs::read(&wasm).map_err(|e| format!("{}: {e}", wasm.display()))
}
