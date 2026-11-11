mod notices;

use sha2::{Digest, Sha256};
use std::{
    env,
    error::Error,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

type Result<T = ()> = std::result::Result<T, Box<dyn Error>>;

fn run(command: &mut Command) -> Result {
    let status = command.status()?;
    if !status.success() {
        return Err(format!("{command:?} failed: {status}").into());
    }
    Ok(())
}

fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result {
    let to = to.as_ref();
    fs::create_dir_all(to.parent().ok_or("destination has no parent")?)?;
    fs::copy(from, to)?;
    Ok(())
}

fn normalize_permissions(path: &Path) -> Result {
    use std::os::unix::fs::PermissionsExt;
    let directory = path.is_dir();
    let executable = path.ends_with("usr/bin/proteus");
    fs::set_permissions(path, fs::Permissions::from_mode(if directory || executable { 0o755 } else { 0o644 }))?;
    if directory {
        for entry in fs::read_dir(path)? {
            normalize_permissions(&entry?.path())?;
        }
    }
    Ok(())
}

fn main() -> Result {
    let args: Vec<_> = env::args().skip(1).collect();
    if args.is_empty() || args == ["--help"] || args == ["help"] {
        println!(
            "cargo xtask dist [--deb] [--skip-build]\n\nBuild native Linux release tarball and SHA256SUMS in target/dist.\n--deb also builds a Debian package (requires dpkg-deb).\n--skip-build packages existing release binaries; it never builds them."
        );
        return Ok(());
    }
    if args[0] != "dist" || args[1..].iter().any(|a| a != "--deb" && a != "--skip-build") {
        return Err("expected dist [--deb] [--skip-build]; use --help".into());
    }
    if env::consts::OS != "linux" {
        return Err("release packaging currently supports native Linux builds only".into());
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf();
    env::set_current_dir(&root)?;
    let manifest: toml::Value = toml::from_str(&fs::read_to_string("Cargo.toml")?)?;
    let version = manifest["workspace"]["package"]["version"].as_str().ok_or("missing workspace version")?;
    if !version.bytes().all(|b| b.is_ascii_alphanumeric() || b".-+".contains(&b)) {
        return Err("unsafe release version".into());
    }
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let host = Command::new(&rustc).arg("-vV").output()?;
    if !host.status.success() {
        return Err("rustc -vV failed".into());
    }
    let host = String::from_utf8(host.stdout)?;
    let host = host.lines().find_map(|l| l.strip_prefix("host: ")).ok_or("rustc did not report host")?;
    let deb_arch = match env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        _ => return Err("supported architectures: x86_64, aarch64".into()),
    };
    for tool in ["tar"].into_iter().chain(args.iter().any(|a| a == "--deb").then_some("dpkg-deb")) {
        let status =
            Command::new(tool).arg("--version").output().map_err(|e| format!("required packaging tool {tool}: {e}"))?;
        if !status.status.success() {
            return Err(format!("{tool} --version failed").into());
        }
    }
    let maintainer = env::var("PROTEUS_DEB_MAINTAINER")
        .unwrap_or_else(|_| "Proteus contributors <maintainers@proteus.invalid>".into());
    if maintainer.trim().is_empty() || maintainer.contains(['\r', '\n']) {
        return Err("PROTEUS_DEB_MAINTAINER must be a single nonempty line".into());
    }
    let target = root.join("target");
    if !args.iter().any(|a| a == "--skip-build") {
        let libdir = Command::new(&rustc).args(["--print", "target-libdir", "--target", "wasm32-wasip2"]).output()?;
        let libdir = String::from_utf8(libdir.stdout)?;
        let installed = fs::read_dir(libdir.trim()).is_ok_and(|entries| {
            entries.filter_map(std::result::Result::ok).any(|e| e.file_name().to_string_lossy().starts_with("libcore-"))
        });
        if libdir.trim().is_empty() || !installed {
            return Err("selected Rust compiler lacks wasm32-wasip2; install it with rustup target add wasm32-wasip2, or select a compiler that provides it using RUSTC".into());
        }
        run(Command::new("cargo")
            .args(["build", "--locked", "--release", "--package", "proteus", "--target", host])
            .env("CARGO_TARGET_DIR", &target))?;
        run(Command::new("cargo")
            .args([
                "build",
                "--locked",
                "--release",
                "--manifest-path",
                "plugins/files/Cargo.toml",
                "--target",
                "wasm32-wasip2",
            ])
            .env("CARGO_TARGET_DIR", &target))?;
    }
    let binary = target.join(host).join("release/proteus");
    let plugin = target.join("wasm32-wasip2/release/proteus_files.wasm");
    for path in [&binary, &plugin] {
        if !path.is_file() {
            return Err(format!("missing {}; run without --skip-build", path.display()).into());
        }
    }
    let name = format!("proteus-{version}-{host}");
    let dist = target.join("dist");
    fs::create_dir_all(&dist)?;
    let stage = dist.join(&name);
    if stage.exists() {
        fs::remove_dir_all(&stage)?;
    }
    let usr = stage.join("usr");
    copy(binary, usr.join("bin/proteus"))?;
    copy("packaging/dev.proteus.Proteus.desktop", usr.join("share/applications/dev.proteus.Proteus.desktop"))?;
    copy(
        "assets/branding/proteus-icon-faithful.svg",
        usr.join("share/icons/hicolor/scalable/apps/dev.proteus.Proteus.svg"),
    )?;
    copy("crates/proteus-pty/terminfo/proteus.terminfo", usr.join("share/proteus/proteus.terminfo"))?;
    copy(plugin, usr.join("share/proteus/plugins/files/proteus_files.wasm"))?;
    let plugin_manifest = fs::read_to_string("plugins/files/proteus-plugin.toml")?
        .replace("target/wasm32-wasip2/release/proteus_files.wasm", "proteus_files.wasm");
    fs::write(usr.join("share/proteus/plugins/files/proteus-plugin.toml"), plugin_manifest)?;
    for entry in fs::read_dir("docs")? {
        let path = entry?.path();
        if path.extension().is_some_and(|s| s == "md") {
            copy(&path, usr.join("share/doc/proteus").join(path.file_name().unwrap()))?;
        }
    }
    for entry in fs::read_dir("assets/branding")? {
        let path = entry?.path();
        copy(&path, usr.join("share/proteus/branding").join(path.file_name().unwrap()))?;
    }
    copy("examples/workspace/.proteus-workspace", usr.join("share/doc/proteus/examples/.proteus-workspace"))?;
    copy("LICENSE", usr.join("share/doc/proteus/LICENSE"))?;
    fs::write(usr.join("share/doc/proteus/THIRD-PARTY-NOTICES.txt"), notices::generate(version, host)?)?;
    normalize_permissions(&stage)?;
    run(Command::new("tar").arg("-czf").arg(dist.join(format!("{name}.tar.gz"))).arg("-C").arg(&dist).arg(&name))?;
    let mut artifacts = vec![format!("{name}.tar.gz")];
    if args.iter().any(|a| a == "--deb") {
        fs::create_dir_all(stage.join("DEBIAN"))?;
        let copyright = usr.join("share/doc/proteus/copyright");
        fs::write(&copyright, notices::debian_copyright()?)?;
        fs::write(
            stage.join("DEBIAN/control"),
            format!(
                "Package: proteus\nVersion: {version}\nSection: x11\nPriority: optional\nArchitecture: {deb_arch}\nMaintainer: {maintainer}\nDepends: libc6, libgcc-s1, libstdc++6, libwayland-client0, libxkbcommon0, libfontconfig1\nRecommends: fonts-dejavu-core, ncurses-bin\nDescription: Wayland terminal with workspaces and extensible panes\n"
            ),
        )?;
        normalize_permissions(&stage)?;
        let deb = format!("proteus_{version}_{deb_arch}.deb");
        run(Command::new("dpkg-deb").args(["--root-owner-group", "--build"]).arg(&stage).arg(dist.join(&deb)))?;
        fs::remove_dir_all(stage.join("DEBIAN"))?;
        fs::remove_file(copyright)?;
        artifacts.push(deb);
    }
    let mut sums = String::new();
    for artifact in &artifacts {
        let hash = Sha256::digest(fs::read(dist.join(artifact))?);
        let hex: String = hash.iter().map(|byte| format!("{byte:02x}")).collect();
        sums.push_str(&format!("{hex}  {artifact}\n"));
        println!("{}", dist.join(artifact).display());
    }
    let checksums = dist.join(format!("{name}.SHA256SUMS"));
    fs::write(&checksums, sums)?;
    println!("{}", checksums.display());
    Ok(())
}
