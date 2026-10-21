//! Proteus: a Wayland terminal rendered with wgpu.
//!
//! Settings come from `$XDG_CONFIG_HOME/proteus/init.lua` (see [`config`]);
//! command-line options override them.

mod config;
mod gui;
mod session;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "proteus", version, about = "proteus — a Wayland terminal")]
#[command(after_help = "Without COMMAND, `shell.command` from the config or $SHELL is started.")]
struct Cli {
    /// Config file [default: $XDG_CONFIG_HOME/proteus/init.lua].
    #[arg(short, long, value_name = "FILE", conflicts_with = "no_config")]
    config: Option<PathBuf>,
    /// Ignore the config file and use built-in defaults.
    #[arg(long)]
    no_config: bool,
    /// Font family or fontconfig pattern (overrides `font.family`).
    #[arg(long, value_name = "FAMILY")]
    font: Option<String>,
    /// Font size in points (overrides `font.size`).
    #[arg(long, value_name = "PT")]
    font_size: Option<f32>,
    /// Program and arguments to run (overrides `shell.command`).
    #[arg(last = true, value_name = "COMMAND")]
    command: Vec<OsString>,
}

fn load_config(cli: &Cli) -> (config::Config, Option<String>) {
    if cli.no_config {
        return (config::Config::default(), None);
    }
    let (path, explicit) = match &cli.config {
        Some(p) => (Some(p.clone()), true),
        None => (config::default_path(), false),
    };
    let Some(path) = path else { return (config::Config::default(), None) };
    match config::load(&path, explicit) {
        Ok(c) => (c, None),
        Err(e) => (config::Config::default(), Some(e)),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let (mut cfg, error) = load_config(&cli);
    if let Some(e) = &error {
        eprintln!("proteus: config error, using defaults: {e}");
    }
    if let Some(f) = cli.font {
        cfg.font_family = f;
    }
    if let Some(s) = cli.font_size {
        cfg.font_size = s.clamp(4.0, 96.0);
    }
    if !cli.command.is_empty() {
        cfg.command = Some(cli.command);
    }
    match gui::run(cfg, error) {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("proteus: {e}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli() {
        Cli::command().debug_assert();
        let c = Cli::try_parse_from(["proteus", "--font-size", "14", "--", "htop", "-d", "5"]).unwrap();
        assert_eq!(c.font_size, Some(14.0));
        assert_eq!(c.command, ["htop", "-d", "5"]);
        assert!(Cli::try_parse_from(["proteus", "--no-config", "-c", "x.lua"]).is_err());
        // The old subcommands are gone.
        assert!(Cli::try_parse_from(["proteus", "run"]).is_err());
    }
}
