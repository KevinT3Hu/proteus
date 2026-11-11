use crate::Result;
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

struct Package {
    name: String,
    version: String,
    license: String,
    repository: String,
    dir: PathBuf,
}

const LICENSE_PREFIXES: [&str; 5] = ["LICEN", "COPYING", "COPYRIGHT", "NOTICE", "UNLICENSE"];

/// Packages linked into the artifact built from `manifest` for `platform`,
/// starting at the local package `root`. Build dependencies are kept because
/// some of them (such as `lua-src`) compile native code into the binary.
fn shipped(manifest: &str, platform: &str, root: &str) -> Result<Vec<Package>> {
    let output = Command::new("cargo")
        .args(["metadata", "--locked", "--format-version", "1", "--filter-platform", platform, "--manifest-path"])
        .arg(manifest)
        .output()?;
    if !output.status.success() {
        return Err(format!("cargo metadata for {manifest} failed: {}", String::from_utf8_lossy(&output.stderr)).into());
    }
    let metadata: Value = serde_json::from_slice(&output.stdout)?;
    let packages: HashMap<&str, &Value> = metadata["packages"]
        .as_array()
        .ok_or("cargo metadata has no packages")?
        .iter()
        .filter_map(|p| Some((p["id"].as_str()?, p)))
        .collect();
    let nodes: HashMap<&str, &Value> = metadata["resolve"]["nodes"]
        .as_array()
        .ok_or("cargo metadata has no resolve graph")?
        .iter()
        .filter_map(|n| Some((n["id"].as_str()?, n)))
        .collect();
    let start = packages
        .values()
        .find(|p| p["name"] == root && p["source"].is_null())
        .and_then(|p| p["id"].as_str())
        .ok_or_else(|| format!("{root} not found in {manifest}"))?;
    let mut seen = HashSet::new();
    let mut stack = vec![start];
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        for dep in nodes.get(id).and_then(|n| n["deps"].as_array()).into_iter().flatten() {
            let linked = dep["dep_kinds"]
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|k| k["kind"].is_null() || k["kind"] == "build"));
            if let (true, Some(pkg)) = (linked, dep["pkg"].as_str()) {
                stack.push(pkg);
            }
        }
    }
    let mut result = Vec::new();
    for id in seen {
        let p = packages[id];
        if p["source"].is_null() {
            continue;
        }
        let text = |key: &str| p[key].as_str().unwrap_or_default().to_string();
        let manifest_path = PathBuf::from(text("manifest_path"));
        result.push(Package {
            name: text("name"),
            version: text("version"),
            license: text("license"),
            repository: text("repository"),
            dir: manifest_path.parent().ok_or("package manifest has no directory")?.to_path_buf(),
        });
    }
    Ok(result)
}

fn license_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_uppercase();
        if path.is_file() && LICENSE_PREFIXES.iter().any(|p| name.starts_with(p)) {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn normalize_repository(url: &str) -> String {
    let url = url.trim().to_lowercase();
    let url = url.split("/tree/").next().unwrap_or_default();
    url.trim_end_matches('/').trim_end_matches(".git").to_string()
}

/// The copyright and permission notice embedded in vendored Lua's `lua.h`.
fn lua_notice(dir: &Path) -> Result<Option<String>> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if !path.file_name().is_some_and(|n| n.to_string_lossy().starts_with("lua-5.4")) {
            continue;
        }
        let Ok(source) = fs::read_to_string(path.join("lua.h")) else { continue };
        let Some(start) = source.find("* Copyright (C) 1994") else { continue };
        let end = source[start..].find("*****/").map_or(source.len(), |e| start + e);
        let notice: Vec<_> = source[start..end].lines().map(|l| l.trim_start_matches('*').trim()).collect();
        return Ok(Some(notice.join("\n").trim().to_string()));
    }
    Ok(None)
}

/// Third-party notices for everything linked into the release artifacts.
/// Fails rather than omitting a component whose licence text is unknown.
pub fn generate(version: &str, host: &str) -> Result<String> {
    let mut packages = BTreeMap::new();
    for (manifest, platform, root) in
        [("Cargo.toml", host, "proteus"), ("plugins/files/Cargo.toml", "wasm32-wasip2", "proteus-files")]
    {
        for p in shipped(manifest, platform, root)? {
            packages.entry((p.name.clone(), p.version.clone())).or_insert(p);
        }
    }
    let mut by_repository: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for p in packages.values() {
        let files = license_files(&p.dir)?;
        if !files.is_empty() && !p.repository.is_empty() {
            by_repository.entry(normalize_repository(&p.repository)).or_insert(files);
        }
    }
    let apache = fs::read_to_string("packaging/licenses/Apache-2.0.txt")?;
    let mut out = format!(
        "Third-party software notices for Proteus {version}\n\n\
         Proteus is licensed under the GNU Affero General Public License, version 3\n\
         only (see LICENSE). The release binaries include the third-party components\n\
         below, each under its own licence.\n"
    );
    let mut printed: HashMap<String, String> = HashMap::new();
    let mut missing = Vec::new();
    let mut emit = |out: &mut String, label: String, text: String| {
        let text = text.trim().to_string();
        match printed.get(&text) {
            Some(first) => out.push_str(&format!("\n--- {label}: same text as {first} ---\n")),
            None => {
                out.push_str(&format!("\n--- {label} ---\n\n{text}\n"));
                printed.insert(text, label);
            }
        }
    };
    for p in packages.values() {
        out.push_str(&format!(
            "\n================================================================================\n\
             {} {}\nLicense: {}\n",
            p.name, p.version, p.license
        ));
        if !p.repository.is_empty() {
            out.push_str(&format!("Repository: {}\n", p.repository));
        }
        let mut files = license_files(&p.dir)?;
        if files.is_empty() {
            files = by_repository.get(&normalize_repository(&p.repository)).cloned().unwrap_or_default();
        }
        for file in &files {
            let text = String::from_utf8_lossy(&fs::read(file)?).into_owned();
            let label = format!("{} {} {}", p.name, p.version, file.file_name().unwrap_or_default().to_string_lossy());
            emit(&mut out, label, text);
        }
        if files.is_empty() {
            if p.license.split([' ', '/', '(', ')']).any(|id| id == "Apache-2.0") {
                emit(&mut out, format!("{} {} Apache-2.0", p.name, p.version), apache.clone());
            } else {
                missing.push(format!("{} {} ({})", p.name, p.version, p.license));
            }
        }
        if p.name == "lua-src" {
            let notice = lua_notice(&p.dir)?.ok_or("lua-src: vendored Lua 5.4 licence notice not found")?;
            emit(&mut out, "Lua 5.4 (vendored by lua-src)".into(), notice);
        }
    }
    if !missing.is_empty() {
        return Err(format!("no licence text found for: {}", missing.join(", ")).into());
    }
    Ok(out)
}

/// A machine-readable Debian copyright file (DEP-5) for the package.
pub fn debian_copyright() -> Result<String> {
    let license = fs::read_to_string("LICENSE")?;
    let body: String = license
        .trim_end()
        .lines()
        .map(|l| if l.trim().is_empty() { " .\n".to_string() } else { format!(" {l}\n") })
        .collect();
    Ok(format!(
        "Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/\n\
         Upstream-Name: proteus\n\
         Comment: Third-party components linked into the binaries and their licences\n \
         are listed in /usr/share/doc/proteus/THIRD-PARTY-NOTICES.txt.\n\
         \n\
         Files: *\n\
         Copyright: Proteus contributors\n\
         License: AGPL-3.0-only\n{body}"
    ))
}
