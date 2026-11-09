# WASM plugins

A plugin is a WebAssembly component that runs inside Proteus in a sandbox. It
can only do what its permissions allow: show panels, read pane titles, run
listed programs, read listed directories, and so on. Use plugins for code you
did not write yourself. Your own extensions can be plain Lua in the config
instead; see [scripting.md](scripting.md).

Proteus has no plugin registry and no install step. You list each plugin in
your config with `p.load_plugin`, and removing the call stops the plugin. The
first-party file manager is described in [files.md](files.md).

## Enable a plugin

Call `p.load_plugin` in `init.lua` once per plugin:

```lua
local p = require("proteus")

-- A package directory (relative paths are relative to init.lua's directory)
p.load_plugin { source = "~/code/my-plugin" }

-- A Git repository, pinned to a commit, with the package in a subdirectory
p.load_plugin {
  source = "https://example.com/tools.git",
  rev = "0123456789abcdef0123456789abcdef01234567",
  subdir = "plugin",
}

-- A prebuilt WASM file, pinned by checksum
p.load_plugin {
  source = "https://example.com/tool.wasm",
  sha256 = "<64 hex digits>",
  id = "example.tool",
  permissions = { ui = true },
}
```

Plugins are loaded in the background after the window opens. Errors (a failed
download or build, a bad manifest, a checksum mismatch) appear as messages in
the window. `proteus --check-config` checks the `p.load_plugin` calls but does
not load any plugin.

### Options

| Key | Meaning |
|---|---|
| `source` | Where the plugin is (see below). `path` and `url` are aliases; give exactly one of the three |
| `rev` | Git revision to check out. Only for Git sources |
| `subdir` | Package directory inside the source. Must stay inside it |
| `sha256` | Expected SHA-256 of the final `.wasm` file (64 hex digits). Loading fails if it differs |
| `id` | Plugin ID for a bare `.wasm` file. If the source has a manifest, it must match the manifest's ID |
| `config` | A table of settings passed to the plugin as JSON. Its meaning is up to the plugin |
| `permissions` | Grant exactly these permissions instead of the manifest's request (see below) |

Each source (with its `rev` and `subdir`) may be listed only once. If two
sources declare the same plugin ID, neither is loaded.

### Source forms

| Source | What happens |
|---|---|
| A directory with `proteus-plugin.toml` and `Cargo.toml` | Built with Cargo for `wasm32-wasip2`, then loaded |
| A directory with `proteus-plugin.toml` only | Loads the `.wasm` file the manifest names (default `plugin.wasm`) |
| A local `.wasm` file | Loaded directly. If its directory has a `proteus-plugin.toml`, that manifest applies |
| An `https://` URL ending in `.wasm` | Downloaded once (at most 256 MiB) and cached |
| A Git URL (`https://`, `ssh://`, `git@...`, `file://`, `http://`, `git://`) | Cloned once and cached; the checked-out directory is then treated like a local directory |

Rules for remote sources:

- A `.wasm` URL must use `https://` unless `sha256` is set.
- `http://` and `git://` Git URLs need `rev` set to a full commit hash (40 or 64
  hex digits). Use `https://` or `ssh://` otherwise.
- A remote source is fetched once and then reused from the cache, also offline.
  A Git source without `rev` stays at the commit that was `HEAD` when it was
  first cloned. To update, change `rev`, or delete the cached copy (see
  [Where plugin files live](#where-plugin-files-live)).
- Git runs without password prompts. SSH uses `BatchMode=yes` unless you set
  `GIT_SSH_COMMAND` or `GIT_SSH`, so keys must be usable without a prompt (for
  example from an agent).
- `git clone` and `git checkout` are stopped after 10 minutes; a Cargo build
  after 30 minutes.

Building from source needs `cargo` on your `PATH` and the `wasm32-wasip2`
target (`rustup target add wasm32-wasip2`). Local source directories are
rebuilt incrementally each time the config is loaded or reloaded, so edits are
picked up.

### Building runs code before you approve anything

A Git or local source that contains `Cargo.toml` is built with `cargo build`
when the config loads. Cargo runs the repository's build scripts and procedural
macros as you, outside the sandbox, before the permission prompt appears and
whether or not you later answer yes. Only use source repositories you trust. For
anything else, use a prebuilt `.wasm` file pinned with `sha256`.

## Pass settings to a plugin

The `config` table is converted to JSON and handed to the plugin unchanged. It
may contain strings, numbers, booleans and nested tables, but no functions.
Which keys are valid is decided by the plugin; for the Files plugin see
[files.md](files.md#settings).

```lua
p.load_plugin {
  source = "/usr/share/proteus/plugins/files",
  config = { position = "right", width = 40 },
}
```

Changing `config` and reloading restarts the plugin with the new settings.

## Permissions

Everything is off unless granted. A plugin's manifest (`proteus-plugin.toml`)
lists what it asks for, in the `[permissions]` table:

| Permission | Form | Allows the plugin to |
|---|---|---|
| `ui` | `true` | Show panels, status bar text, prompts and command palette entries; register actions; open files and URLs with the desktop's default application |
| `read-panes` | `true` | See your panes: titles, directories, running programs; focus a pane |
| `read-screen` | `true` | Read the text in your panes, including scrollback and command output |
| `write-pty` | `true` | Type into your panes. It can run commands as you |
| `open-pane` | `true` | Open panes running any program. It can run any code as you |
| `fs` | list of `"PATH:r"` or `"PATH:rw"` | Read (`r`) or read and change (`rw`) files in these directories |
| `run-command` | list of program names or absolute paths | Run these programs as you |
| `network` | list of host names | Make HTTP and HTTPS requests to these hosts |
| `clipboard-read` | `true` | Read the clipboard |
| `clipboard-write` | `true` | Change the clipboard |
| `secrets` | `true` | Keep its own passwords and keys in your desktop keyring (Secret Service) |

Details:

- `run-command`: a program name is looked up only in
  `/usr/local/bin:/usr/bin:/bin`, not in your `PATH`, because directories such
  as `~/.local/bin` are often writable by the plugin. Programs start without a
  shell. They get only `HOME`, `USER`, `LOGNAME`, `LANG`, `LANGUAGE`, `TERM`,
  `TZ`, `TMPDIR` and `LC_*` from your environment, with `PATH` set to the list
  above. A listed program gets whatever arguments the plugin passes, so it can
  usually be made to run other code.
- `network`: `api.example.com` matches that host, `*.example.com` any
  subdomain, and `localhost:11434` only that port. An entry without a port
  allows only the default port (443 for HTTPS, 80 for HTTP). Redirects are not
  followed. Opening a web URL with the `ui` permission is limited to the same
  hosts.
- `fs`: see [Filesystem grants](#filesystem-grants).
- Every plugin also has its own data directory, which it sees as `/data`, and a
  small key-value store. These need no permission.
- Opening a file with the desktop's default application refuses executable
  files, `.desktop` launchers and files in the plugin's data directory.

### Grant permissions in the config

`permissions` in `p.load_plugin` replaces the manifest's request completely: the
plugin gets exactly what you list, which may be less or more than it asks for.
No approval prompt is shown when the config grants permissions. A plugin that
lacks a permission gets an error when it tries to use it; how it copes is up to
the plugin.

Names are the manifest names. Lists are Lua sequences of strings:

```lua
p.load_plugin {
  source = "~/code/git-ui",
  permissions = {
    ui = true,
    ["read-panes"] = true,
    fs = { "~/src:rw", "/etc:r" },
    ["run-command"] = { "git" },
    network = { "api.github.com" },
  },
}
```

`permissions = {}` runs the plugin with no permissions at all. A bare `.wasm`
file without a manifest requests nothing, so it runs with no permissions unless
the config grants some.

### The approval prompt

If the config does not set `permissions` and the plugin requests any, Proteus
asks before starting it:

> Plugin NAME (SOURCE @ REV (sha256 FIRST-16-HEX-DIGITS)) requests these
> permissions. Allow and remember?

followed by the list of permissions. Press `y` to allow, or `n` or Escape to
not start the plugin. The prompt ignores keys for its first 500 ms, so a key
you were already typing does not answer it.

The answer `y` is remembered. Proteus asks again when the plugin's WASM file
changes (a different SHA-256), when it requests different permissions, or when
the source, `rev` or `subdir` changes. Answering `n` keeps the plugin stopped
until Proteus restarts; to run it without approving its request, grant
`permissions` in the config.

The prompt adds a warning when the request includes:

- `run-command`: "Programs it may run get its arguments unchecked and can run
  any code as you."
- `open-pane`: "Panes it opens may run any program, so it can run any code as
  you."
- a writable `fs` grant that is or contains your home directory, lies inside a
  dot directory of it (such as `~/.ssh` or `~/.config`), or overlaps
  `$XDG_CONFIG_HOME`, `$XDG_STATE_HOME`, `$XDG_DATA_HOME` or
  `$PROTEUS_DATA_DIR`: "It may change your startup files and Proteus's
  configuration, so it can run any code as you."

Treat these the same as running the plugin's code unsandboxed.

### Filesystem grants

- A path must be absolute or start with `~/` (or be `~`). `..` is not allowed.
- `:r` is read-only, `:rw` read and write. The plugin sees the directory at the
  same path as you do.
- A grant must be a directory; single files cannot be granted. A directory that
  does not exist yet is skipped and becomes available on a later start.
- A grant inside a read-write grant is refused, read-only or not. For example
  `{ "/srv:rw", "/srv/secret:r" }` fails, because the plugin could change the
  inner directory through the outer grant. `{ "/srv:r", "/srv/notes:rw" }` is
  fine.
- A grant whose path goes through a symbolic link that sits inside a writable
  directory (including the plugin's data directory) is refused, because the
  plugin could have placed the link to widen its access. Symlinks the plugin
  cannot write, like a symlinked home directory, are followed.

## Private panes

Mark a pane private with the `pane.toggle_private` action (no default key; use
the command palette or bind a key). Its title shows `[private]`. For a private
pane, plugins

- see only its id, position, size and user-set name, not its title, directory,
  program or command,
- cannot read its screen or blocks, and
- get no events about its title, directory, commands or blocks.

## Plugin actions and keys

A plugin can register actions. They appear in the command palette and can
suggest a default key. Default keys only take keys that are free in your key
preset, and any binding in your config wins. Refer to an action as
`plugin:PLUGIN-ID:ACTION-ID`:

```lua
p.keymap("normal", "alt+f", "plugin:proteus.files:sidebar.toggle")
p.keymap("normal", "alt+e", false) -- remove the plugin's default key
```

`proteus --list-keybindings` does not include plugin default keys, because they
exist only while the plugin runs.

## When a plugin fails

A plugin that crashes, runs one call for more than 5 seconds, or runs out of
memory (each plugin has 256 MiB) is stopped. Proteus restarts it after one
second, up to 3 times. A plugin that fails while starting up is not restarted.
In both cases a message names the plugin and the error.

To start it again by hand, run "plugins: restart NAME" from the command
palette. Long-running plugin tasks shown in the status bar can be cancelled
with "plugins: cancel ...". Panes that belong to a stopped plugin stay open and
say "plugin ID is not running"; they come back when it starts again.

Plugin log messages, plugin standard error, and the details of a plugin crash
are written to the Proteus log file whether or not `PROTEUS_DEBUG` is set. See
[Logs](getting-started.md#logs) for where it is and how to get more detail.

## Where plugin files live

| Path | Contents |
|---|---|
| `$XDG_CACHE_HOME/proteus/plugins/` (or `$PROTEUS_CACHE_DIR`) | Downloads, Git checkouts (`sources/`), build output (`build/`), loaded `.wasm` files (`artifacts/`), compiled code (`compiled/`). Safe to delete |
| `$XDG_DATA_HOME/proteus/plugin-data/ID/` | The plugin's data directory (`/data` inside the plugin) |
| `$XDG_DATA_HOME/proteus/plugin-data/ID.storage.toml` | The plugin's key-value store |
| `$XDG_DATA_HOME/proteus/plugin-owners/ID` | Which source owns this plugin ID |
| `$XDG_STATE_HOME/proteus/plugin-approvals.json` | Remembered approvals |

`$PROTEUS_DATA_DIR` replaces `$XDG_DATA_HOME/proteus`. Without the XDG
variables, `~/.cache`, `~/.local/share` and `~/.local/state` are used.

A plugin ID's data, store and keyring entries belong to the first source that
used the ID. If another source declares the same ID, it does not start and the
message names the file to delete in `plugin-owners/` to hand the data over.

## Writing plugins

Plugins are written in Rust with the SDK in `crates/proteus-plugin`; start with
its `README.md`. `examples/plugins/pane-list` is a small complete plugin, and
`examples/plugins/README.md` shows how to load it.
