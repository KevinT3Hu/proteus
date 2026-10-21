//! Proteus: a Wayland terminal rendered with wgpu.
//!
//! Settings come from `$XDG_CONFIG_HOME/proteus/init.lua` (see [`config`]);
//! command-line options override them.

mod config;
mod gui;
mod icat;
mod layout;
mod modes;
mod script;
mod session;
mod spec;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "proteus", version, about = "proteus — a Wayland terminal")]
#[command(after_help = "Without COMMAND, `shell.command` from the config or $SHELL is started.")]
#[command(
    args_conflicts_with_subcommands = true,
    disable_help_subcommand = true,
    subcommand_value_name = "+TOOL",
    subcommand_help_heading = "Tools"
)]
struct Cli {
    #[command(subcommand)]
    tool: Option<Tool>,
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
    /// Write Lua type annotations (lua-language-server) to DIR/proteus.lua, and exit.
    #[arg(long, value_name = "DIR")]
    emit_lua_types: Option<PathBuf>,
    /// Program and arguments to run (overrides `shell.command`).
    #[arg(last = true, value_name = "COMMAND")]
    command: Vec<OsString>,
}

/// Helpers run inside a terminal instead of opening a window.
#[derive(clap::Subcommand, Debug)]
enum Tool {
    /// Show images in the terminal (kitty graphics protocol).
    #[command(name = "+icat")]
    Icat(icat::Icat),
}

/// The config, the file it came from (watched for changes) and its error.
fn load_config(cli: &Cli) -> (config::Loaded, Option<PathBuf>, Option<String>) {
    if cli.no_config {
        return (config::defaults(), None, None);
    }
    let (path, explicit) = match &cli.config {
        Some(p) => (Some(p.clone()), true),
        None => (config::default_path(), false),
    };
    let Some(path) = path else { return (config::defaults(), None, None) };
    match config::load(&path, explicit) {
        Ok(c) => (c, Some(path), None),
        Err(e) => (config::defaults(), Some(path), Some(e)),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Some(Tool::Icat(args)) = cli.tool {
        return ExitCode::from(icat::run(args));
    }
    if cli.list_actions {
        for a in modes::Action::all() {
            println!("{}", a.name());
        }
        println!("tab.goto_N");
        return ExitCode::SUCCESS;
    }
    if let Some(dir) = &cli.emit_lua_types {
        let path = dir.join("proteus.lua");
        return match std::fs::create_dir_all(dir).and_then(|_| std::fs::write(&path, script::TYPES)) {
            Ok(()) => {
                println!("{}", path.display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("proteus: {}: {e}", path.display());
                ExitCode::from(2)
            }
        };
    }
    let (mut loaded, config_path, error) = load_config(&cli);
    if let Some(e) = &error {
        eprintln!("proteus: config error, using defaults: {e}");
    }
    // Command-line font options win over the config (also after reloads).
    let overrides = gui::Overrides { font: cli.font, font_size: cli.font_size.map(|s| s.clamp(4.0, 96.0)) };
    overrides.apply(&mut loaded.config);
    // The command runs in the first pane; new panes start the shell.
    let command = (!cli.command.is_empty()).then_some(cli.command);
    let layout =
        match cli.layout.as_deref().map(|l| spec::resolve(l).ok_or(l).map_err(|l| format!("{l}: no config directory")))
        {
            None => None,
            Some(path) => match path.and_then(|p| spec::load(&p)) {
                Ok(s) => Some(s),
                Err(e) => {
                    eprintln!("proteus: layout: {e}");
                    return ExitCode::from(2);
                }
            },
        };
    match gui::run(loaded, config_path, overrides, error, command, layout) {
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
        // Tools are subcommands.
        let c = Cli::try_parse_from(["proteus", "+icat", "--place", "40x20@1x2", "--scale", "a.png"]).unwrap();
        let Some(Tool::Icat(a)) = c.tool else { panic!("not +icat") };
        assert_eq!(a.place, Some(icat::Place { cols: 40, rows: 20, x: 1, y: 2 }));
        assert!(a.scale && !a.clear && a.files == ["a.png"]);
        assert!(Cli::try_parse_from(["proteus", "+icat"]).is_err());
        assert!(Cli::try_parse_from(["proteus", "+icat", "--clear"]).is_ok());
        assert!(Cli::try_parse_from(["proteus", "+icat", "--place", "4x2", "a"]).is_err());
        // Window options do not mix with tools.
        assert!(Cli::try_parse_from(["proteus", "--no-config", "+icat", "a.png"]).is_err());
        assert!(Cli::try_parse_from(["proteus", "--", "htop"]).unwrap().tool.is_none());
    }
}
