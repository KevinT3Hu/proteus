//! `proteus +plugin ...`: install and manage WASM plugins. Running
//! terminals pick up the changes (they watch `plugins.lock`).

use std::io::{BufRead, IsTerminal, Write};

use proteus_wasm::{Manifest, Permissions, Store};

#[derive(clap::Args, Debug)]
pub struct Plugin {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(clap::Subcommand, Debug)]
pub enum Cmd {
    /// Install a plugin from a directory, archive (.tar.gz, .zip, ...) or git URL.
    Install {
        source: String,
        /// Grant the permissions it asks for without asking.
        #[arg(long)]
        yes: bool,
    },
    /// Install a plugin again from where it came from (asks about new permissions).
    Update {
        id: String,
        #[arg(long)]
        yes: bool,
    },
    /// Show the installed plugins.
    List,
    /// Show a plugin's source, version and granted permissions.
    Info {
        id: String,
    },
    Enable {
        id: String,
    },
    Disable {
        id: String,
    },
    /// Take a permission away (`read-screen`, `fs:~/src`, ...).
    Revoke {
        id: String,
        permission: String,
    },
    /// Uninstall a plugin.
    Remove {
        id: String,
        /// Keep the plugin's data directory.
        #[arg(long)]
        keep_data: bool,
    },
}

fn store() -> Result<Store, String> {
    match std::env::var_os("PROTEUS_DATA_DIR") {
        Some(d) => Ok(Store::new(d.into())),
        None => Store::open_default().ok_or_else(|| "no data directory ($XDG_DATA_HOME or $HOME)".into()),
    }
}

/// Print what a plugin asks for and ask the user.
fn ask(m: &Manifest, asks: &Permissions, yes: bool) -> bool {
    println!("{} {} ({})", m.name, m.version, m.id);
    if !m.description.is_empty() {
        println!("  {}", m.description);
    }
    let lines = asks.describe();
    if lines.is_empty() {
        println!("It asks for no permissions.");
    } else {
        println!("It asks to:");
        for l in &lines {
            println!("  - {l}");
        }
    }
    if yes {
        return true;
    }
    if !std::io::stdin().is_terminal() {
        eprintln!("proteus: not a terminal: pass --yes to grant these permissions");
        return false;
    }
    print!("Allow? [y/N] ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    let _ = std::io::stdin().lock().read_line(&mut answer);
    matches!(answer.trim(), "y" | "Y" | "yes")
}

fn run_cmd(cmd: Cmd) -> Result<(), String> {
    let store = store()?;
    for e in crate::bundled::install(&store) {
        eprintln!("proteus: bundled plugin: {e}");
    }
    match cmd {
        Cmd::Install { source, yes } => {
            if !std::path::Path::new(&source).exists()
                && let Some(r) = crate::bundled::reinstall(&store, &source)
            {
                r?;
                println!("installed the bundled {source}");
                return Ok(());
            }
            let e = store.install(&source, &mut |m, p| ask(m, p, yes))?;
            println!("installed {} {} ({})", e.name, e.version, if e.enabled { "enabled" } else { "disabled" });
        }
        Cmd::Update { id, yes } => {
            let e = store.update(&id, &mut |m, p| ask(m, p, yes))?;
            println!("updated {} to {}", e.name, e.version);
        }
        Cmd::List => {
            let lock = store.read()?;
            if lock.plugins.is_empty() {
                println!("no plugins installed");
            }
            for e in lock.plugins {
                let state = if e.enabled { "enabled" } else { "disabled" };
                println!("{:<28} {:<10} {:<9} {}", e.id, e.version, state, e.name);
            }
        }
        Cmd::Info { id } => {
            let lock = store.read()?;
            let e = lock.get(&id).ok_or_else(|| format!("no plugin `{id}` is installed"))?;
            println!("{} {} ({})", e.name, e.version, e.id);
            println!("  source:  {}", e.source);
            println!("  {}", if e.enabled { "enabled" } else { "disabled" });
            println!("  data:    {}", store.data_dir(&e.id).display());
            let names = e.granted.names();
            println!("  granted: {}", if names.is_empty() { "nothing".into() } else { names.join(", ") });
        }
        Cmd::Enable { id } => store.set_enabled(&id, true)?,
        Cmd::Disable { id } => store.set_enabled(&id, false)?,
        Cmd::Revoke { id, permission } => store.revoke(&id, &permission)?,
        Cmd::Remove { id, keep_data } => store.remove(&id, keep_data)?,
    }
    Ok(())
}

/// Exit status.
pub fn run(p: Plugin) -> u8 {
    match run_cmd(p.cmd) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("proteus: {e}");
            1
        }
    }
}
