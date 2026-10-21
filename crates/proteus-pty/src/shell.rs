//! Automatic shell-integration injection (kitty/Ghostty style) and terminfo install.
//!
//! - bash: `bash --posix` with `ENV` pointing at our script, which restores
//!   normal mode and sources the user's startup files.
//! - zsh: `ZDOTDIR` points at our `.zshenv`, which restores `ZDOTDIR`.
//! - fish: our directory is prepended to `XDG_DATA_DIRS` (`vendor_conf.d`).

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::CommandSpec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
}

const FILES: &[(&str, &str)] = &[
    ("bash/proteus.bash", include_str!("../shell-integration/bash/proteus.bash")),
    ("zsh/.zshenv", include_str!("../shell-integration/zsh/.zshenv")),
    ("zsh/proteus.zsh", include_str!("../shell-integration/zsh/proteus.zsh")),
    ("fish/vendor_conf.d/proteus.fish", include_str!("../shell-integration/fish/vendor_conf.d/proteus.fish")),
];

pub fn detect(spec: &CommandSpec) -> Option<Shell> {
    match spec.program_name().to_str()?.trim_start_matches('-') {
        "bash" => Some(Shell::Bash),
        "zsh" => Some(Shell::Zsh),
        "fish" => Some(Shell::Fish),
        _ => None,
    }
}

/// `$XDG_RUNTIME_DIR/proteus` (falls back to the temp dir, per user).
pub fn runtime_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d).join("proteus"),
        None => std::env::temp_dir().join(format!("proteus-{}", rustix::process::getuid().as_raw())),
    }
}

/// `$XDG_DATA_HOME/proteus` (default `~/.local/share/proteus`).
pub fn data_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".local/share"),
    };
    Some(base.join("proteus"))
}

/// Write the integration scripts under `dir` (only files whose content changed).
pub fn install(dir: &Path) -> io::Result<()> {
    for (rel, content) in FILES {
        let path = dir.join(rel);
        if std::fs::read(&path).is_ok_and(|old| old == content.as_bytes()) {
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Write-then-rename so a concurrently starting shell never reads half a file.
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(&tmp, content)?;
        std::fs::rename(&tmp, &path)?;
    }
    Ok(())
}

/// Rewrite `spec` so the shell loads the integration from `dir` (see [`install`]).
/// Returns the shell that was set up, or `None` if the program is not a
/// supported shell or its arguments make injection unsafe.
pub fn inject(spec: &mut CommandSpec, dir: &Path) -> Option<Shell> {
    let shell = detect(spec)?;
    spec.set_env("PROTEUS_SHELL_INTEGRATION_DIR", dir);
    match shell {
        Shell::Bash => {
            // `--posix` + ENV only works for a plain interactive start; login shells
            // would read profile files twice. Custom arguments opt out.
            if spec.args.iter().any(|a| a != "-i") {
                return None;
            }
            if let Some(env) = spec.get_env("ENV") {
                spec.set_env("PROTEUS_BASH_ENV", env);
            }
            spec.set_env("ENV", dir.join("bash/proteus.bash"));
            spec.set_env("PROTEUS_BASH_INJECT", "1");
            spec.args.insert(0, "--posix".into());
        }
        Shell::Zsh => {
            if let Some(z) = spec.get_env("ZDOTDIR") {
                spec.set_env("PROTEUS_ZSH_ZDOTDIR", z);
            }
            spec.set_env("ZDOTDIR", dir.join("zsh"));
        }
        Shell::Fish => {
            let ours = dir.as_os_str().to_owned();
            let rest = spec
                .get_env("XDG_DATA_DIRS")
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
            let mut v = OsString::from(&ours);
            v.push(":");
            v.push(rest);
            spec.set_env("XDG_DATA_DIRS", v);
            spec.set_env("PROTEUS_FISH_XDG_DATA_DIR", ours);
        }
    }
    Some(shell)
}

/// Compile the `proteus` terminfo entry into `dir` with `tic` (once). On
/// success, returns the directory to put in `TERMINFO_DIRS`.
pub fn install_terminfo(dir: &Path) -> Option<PathBuf> {
    // ncurses uses either `p/proteus` or the hex layout `70/proteus`.
    if dir.join("p/proteus").is_file() || dir.join("70/proteus").is_file() {
        return Some(dir.to_path_buf());
    }
    std::fs::create_dir_all(dir).ok()?;
    let src = dir.join("proteus.terminfo");
    std::fs::write(&src, crate::TERMINFO_SRC).ok()?;
    let ok = Command::new("tic")
        .arg("-x")
        .arg("-o")
        .arg(dir)
        .arg(&src)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    let _ = std::fs::remove_file(&src);
    ok.then(|| dir.to_path_buf())
}

/// Use `TERM=proteus` if its terminfo is available in `terminfo_dir`;
/// otherwise children keep the `xterm-256color` fallback.
pub fn apply_term(spec: &mut CommandSpec, terminfo_dir: Option<&Path>) {
    let Some(dir) = terminfo_dir else { return };
    let mut dirs = OsString::from(dir);
    // An empty entry means "the system default locations".
    dirs.push(":");
    if let Some(existing) = spec.get_env("TERMINFO_DIRS").filter(|v| !v.is_empty()) {
        dirs.push(existing);
    }
    spec.set_env("TERMINFO_DIRS", dirs);
    spec.set_env("TERM", crate::TERM_NAME);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inject_per_shell() {
        let dir = Path::new("/run/x");
        let mut bash = CommandSpec::new("/bin/bash");
        assert_eq!(inject(&mut bash, dir), Some(Shell::Bash));
        assert_eq!(bash.args, vec![OsString::from("--posix")]);
        assert_eq!(bash.get_env("ENV"), Some("/run/x/bash/proteus.bash".into()));

        let mut script = CommandSpec::new("bash").arg("-c").arg("true");
        assert_eq!(inject(&mut script, dir), None);

        let mut zsh = CommandSpec::new("/usr/bin/zsh");
        inject(&mut zsh, dir);
        assert_eq!(zsh.get_env("ZDOTDIR"), Some("/run/x/zsh".into()));

        let mut fish = CommandSpec::new("fish").env("XDG_DATA_DIRS", "/a");
        inject(&mut fish, dir);
        assert_eq!(fish.get_env("XDG_DATA_DIRS"), Some("/run/x:/a".into()));

        assert_eq!(inject(&mut CommandSpec::new("nu"), dir), None);
    }
}
