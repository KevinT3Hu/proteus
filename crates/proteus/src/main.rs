//! Proteus: a Wayland terminal rendered with wgpu.
//!
//! Settings come from `$XDG_CONFIG_HOME/proteus/init.lua` (see [`config`]);
//! command-line options override them.

mod config;
mod gui;
mod layout;
mod modes;
mod session;
mod spec;

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
    /// Open a layout: a name in $XDG_CONFIG_HOME/proteus/layouts, or a path.
    #[arg(short, long, value_name = "NAME", conflicts_with = "command")]
    layout: Option<String>,
    /// Print the names of all actions that keys can be bound to, and exit.
    #[arg(long)]
    list_actions: bool,
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
    if cli.list_actions {
        for a in modes::Action::all() {
            println!("{}", a.name());
        }
        println!("tab.goto_N");
        return ExitCode::SUCCESS;
    }
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
    // The command runs in the first pane; new panes start the shell.
    let command = (!cli.command.is_empty()).then_some(cli.command);
    let layout = match cli.layout.as_deref().map(|l| spec::resolve(l).ok_or(l).map_err(|l| format!("{l}: no config directory"))) {
        None => None,
        Some(path) => match path.and_then(|p| spec::load(&p)) {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("proteus: layout: {e}");
                return ExitCode::from(2);
            }
        },
    };
    match gui::run(cfg, error, command, layout) {
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
        assert_eq!(Cli::try_parse_from(["proteus", "-l", "dev"]).unwrap().layout.as_deref(), Some("dev"));
        assert!(Cli::try_parse_from(["proteus", "-l", "dev", "--", "htop"]).is_err());
    }
}
