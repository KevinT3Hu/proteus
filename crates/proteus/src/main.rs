//! Proteus: a Wayland terminal rendered with wgpu.
//!
//! Settings come from `$XDG_CONFIG_HOME/proteus/init.lua` (see [`config`]);
//! command-line options override them.

mod appearance;
mod config;
mod gui;
mod i18n;
mod icat;
mod ipc;
mod layout;
mod logging;
mod modes;
mod project;
mod script;
mod session;
mod spec;
mod ssh;
mod theme;

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
    /// Lua config file [default: $XDG_CONFIG_HOME/proteus/init.lua].
    #[arg(short = 'c', long, value_name = "FILE", conflicts_with = "no_config")]
    config: Option<PathBuf>,
    /// Ignore the config file and use built-in defaults.
    #[arg(long)]
    no_config: bool,
    /// Evaluate the config and exit without opening a window or loading WASM plugins.
    #[arg(long)]
    check_config: bool,
    /// Print configured keybindings as JSON (runtime plugin defaults require a running plugin).
    #[arg(long)]
    list_keybindings: bool,
    /// Print support diagnostics without opening a window.
    #[arg(long)]
    diagnostics: bool,
    /// Enable same-user local control on this Unix socket.
    #[arg(long, value_name = "SOCKET")]
    listen: Option<PathBuf>,
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
    /// Control an existing terminal instance.
    #[command(name = "+msg")]
    Msg(ipc::Client),
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
    // Keep require(), relative plugin sources, and reload watches anchored
    // to the selected file even when -c names a relative path.
    let path = if path.is_absolute() {
        path
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(e) => return (config::defaults(), None, Some(format!("config directory: {e}"))),
        }
    };
    match config::load(&path, explicit) {
        Ok(c) => (c, Some(path), None),
        Err(e) => (config::defaults(), Some(path), Some(e)),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let cli_tool = cli.tool.is_some()
        || cli.check_config
        || cli.diagnostics
        || cli.list_keybindings
        || cli.list_actions
        || cli.emit_lua_types.is_some();
    let log_file = if cli_tool {
        logging::init_stderr();
        None
    } else {
        logging::init()
    };
    match cli.tool {
        Some(Tool::Icat(args)) => return ExitCode::from(icat::run(args)),
        Some(Tool::Msg(args)) => {
            return match args.run() {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("proteus: {e}");
                    ExitCode::from(2)
                }
            };
        }
        None => {}
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
    if cli.check_config || cli.diagnostics || cli.list_keybindings {
        if cli.diagnostics {
            println!("proteus {}", env!("CARGO_PKG_VERSION"));
            println!("platform: {} / {}", std::env::consts::OS, std::env::consts::ARCH);
            println!(
                "config: {}",
                config_path.as_ref().map_or_else(|| "built-in defaults".into(), |p| p.display().to_string())
            );
            println!("font: {} ({} pt)", loaded.config.font_family, loaded.config.font_size);
            println!("plugins declared: {}", loaded.config.plugins.len());
            for key in
                ["WAYLAND_DISPLAY", "XDG_SESSION_TYPE", "XDG_CURRENT_DESKTOP", "LANG", "LC_ALL", "TERM", "COLORTERM"]
            {
                if let Some(value) = std::env::var_os(key) {
                    println!("{key}: {}", value.to_string_lossy().escape_debug());
                }
            }
            println!(
                "child TERM: {}",
                loaded.config.term.as_deref().unwrap_or("auto (proteus with terminfo, otherwise xterm-256color)")
            );
            match gui::diagnostic_protocols() {
                Ok(protocols) => println!("Wayland protocols:\n  {}", protocols.join("\n  ")),
                Err(e) => println!("Wayland unavailable: {e}"),
            }
            let mut seen = Vec::new();
            for (mode, chord, _) in &loaded.config.keys.binds {
                if seen.contains(&(*mode, *chord)) {
                    println!("keybinding override: {} / {} (last declaration wins)", mode.key(), chord.label());
                }
                seen.push((*mode, *chord));
            }
            println!(
                "log file: {}",
                logging::log_path().map_or_else(|| "none (HOME is unset)".into(), |p| p.display().to_string())
            );
            println!("The selected GPU adapter is written to the log file at startup.");
        }
        if error.is_none() && cli.list_keybindings {
            let keymap = loaded.config.keymap();
            let bindings: Vec<_> = modes::MODES
                .into_iter()
                .flat_map(|mode| {
                    keymap.bindings(mode).iter().map(move |(chord, action)| {
                    serde_json::json!({"mode": mode.key(), "key": chord.label(), "action": action.name()})
                })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&bindings).expect("JSON values serialize"));
        }
        return match error {
            Some(e) => {
                eprintln!("proteus: {e}");
                ExitCode::from(2)
            }
            None => {
                if cli.check_config {
                    println!("Config OK");
                }
                ExitCode::SUCCESS
            }
        };
    }
    if let Some(e) = &error {
        tracing::error!("config error, using defaults: {e}");
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
                    tracing::error!("layout: {e}");
                    return ExitCode::from(2);
                }
            },
        };
    match gui::run(loaded, config_path, overrides, error, command, layout, cli.listen, log_file) {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            tracing::error!("{e}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn relative_custom_config_loads_its_modules() {
        let cli = Cli::try_parse_from(["proteus", "-c", "../../examples/config/init.lua"]).unwrap();
        let (loaded, path, error) = load_config(&cli);
        assert!(error.is_none(), "{error:?}");
        assert!(path.unwrap().is_absolute());
        assert_eq!(loaded.config.font_size, 13.0);
        assert!(loaded.script.has_segments());
        assert!(loaded.script.has_hooks("command_finished"));
    }

    #[test]
    fn cli() {
        Cli::command().debug_assert();
        let c = Cli::try_parse_from(["proteus", "--font-size", "14", "--", "htop", "-d", "5"]).unwrap();
        assert_eq!(c.font_size, Some(14.0));
        assert_eq!(c.command, ["htop", "-d", "5"]);
        assert!(Cli::try_parse_from(["proteus", "--no-config", "-c", "x.lua"]).is_err());
        for flag in ["-c", "--config"] {
            let c = Cli::try_parse_from(["proteus", flag, "custom.lua"]).unwrap();
            assert_eq!(c.config, Some(PathBuf::from("custom.lua")));
        }
        assert!(Cli::try_parse_from(["proteus", "-c"]).is_err());
        assert!(matches!(
            Cli::try_parse_from(["proteus", "+msg", "--socket", "/tmp/test.sock", "list"]).unwrap().tool,
            Some(Tool::Msg(_))
        ));
        assert!(Cli::try_parse_from(["proteus", "+msg", "split", "--axis", "invalid"]).is_err());
        assert_eq!(
            Cli::try_parse_from(["proteus", "--listen", "/tmp/test.sock"]).unwrap().listen,
            Some(PathBuf::from("/tmp/test.sock"))
        );
        assert!(Cli::try_parse_from(["proteus", "--check-config"]).unwrap().check_config);
        assert!(Cli::try_parse_from(["proteus", "--diagnostics"]).unwrap().diagnostics);
        assert!(Cli::try_parse_from(["proteus", "--list-keybindings"]).unwrap().list_keybindings);
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
        assert!(Cli::try_parse_from(["proteus", "+plugin", "install", "./x"]).is_err());
    }
}
