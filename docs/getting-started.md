# Getting started

Proteus is a GPU-rendered terminal emulator for Wayland desktops on Linux. It
has built-in panes, tabs and floating panes, command blocks, inline images,
SSH helpers, and is configured with a Lua file. This page covers installing it,
starting it, the command line, and the environment it gives to programs.

## Requirements

- A Wayland session. Proteus is a native Wayland client; it does not run under X11.
- A GPU with working Vulkan or OpenGL drivers. Rendering uses wgpu, which tries
  Vulkan and then GL. Set the standard wgpu variable `WGPU_BACKEND` (for
  example `WGPU_BACKEND=gl`) to force a backend.
- fontconfig, xkbcommon and the Wayland client libraries at runtime.
- `tic` (from ncurses, `ncurses-bin` on Debian and Ubuntu) to install the
  `proteus` terminfo entry. Without it, programs get `TERM=xterm-256color`.
- Optional: `curl`, used by `proteus +icat` to fetch images from URLs.

## Install a release

Release tarballs and Debian packages are built for x86_64 and aarch64. See
[releases.md](releases.md) for how they are produced and installed. In short:

```sh
sha256sum -c proteus-VERSION-HOST.SHA256SUMS
sudo apt install ./proteus_VERSION_ARCH.deb
```

The tarball contains a `usr` tree that you copy into a prefix such as `/usr/local`.

## Build from source

You need the Rust toolchain named in the workspace (Rust 1.98 or newer) and the
native development packages. On Debian or Ubuntu:

```sh
sudo apt install pkg-config libwayland-dev libxkbcommon-dev libfontconfig1-dev ncurses-bin
cargo build --release -p proteus
./target/release/proteus
```

The Files plugin is a separate WASI component. To build it you also need the
`wasm32-wasip2` target (`rustup target add wasm32-wasip2`); see [files.md](files.md)
and [plugins.md](plugins.md). `cargo xtask dist` builds both and packages them
(see [releases.md](releases.md)).

## First launch

Run `proteus`. It opens one window with one pane running your shell: the
`shell.command` setting if you set one, otherwise `$SHELL`, otherwise the shell
listed for your user in `/etc/passwd`, otherwise `/bin/sh`.

No configuration file is needed. With the default key preset, these keys are
useful right away:

| Key | Action |
| --- | --- |
| `Ctrl+Shift+C` / `Ctrl+Shift+V` | Copy / paste |
| `Ctrl+Shift+P` | Open the command palette (every action is listed there) |
| `Ctrl+P` | Pane mode; then `r` splits right, `d` splits down, `x` closes, `Esc` returns |
| `Ctrl+T` | Tab mode |
| `Alt+N` / `Alt+T` | New pane / new tab |
| `Alt+H` `Alt+J` `Alt+K` `Alt+L` (or Alt+arrows) | Move focus between panes |
| `Ctrl+Shift+F` | Search |
| `Ctrl+=` / `Ctrl+-` / `Ctrl+0` | Bigger / smaller / default font size |
| `Ctrl+G` | Lock: pass all keys to the program until you press `Ctrl+G` again |

While a mode other than Normal is active, a hint bar at the bottom lists its
keys. The full key reference, the other presets (`leader`, `tmux`) and how to
rebind keys are in [keys.md](keys.md). Panes, tabs and layouts are covered in
[panes-and-layouts.md](panes-and-layouts.md).

When the last pane closes, Proteus exits with the exit status of that pane's process.

## Where configuration lives

Proteus reads `$XDG_CONFIG_HOME/proteus/init.lua`, which is
`~/.config/proteus/init.lua` when `XDG_CONFIG_HOME` is unset. A missing file is
not an error; the built-in defaults are used. Changes are applied while Proteus
runs. See [configuration.md](configuration.md).

Other locations Proteus uses:

| Path | Contents |
| --- | --- |
| `$XDG_CONFIG_HOME/proteus/layouts/` | Saved layouts, opened with `--layout NAME` |
| `$XDG_STATE_HOME/proteus/session.lua` (default `~/.local/state/proteus/`) | The saved session used for restore |
| `$XDG_DATA_HOME/proteus/terminfo/` (default `~/.local/share/proteus/`) | The compiled `proteus` terminfo entry |
| `$XDG_RUNTIME_DIR/proteus/shell-integration/` | Shell integration scripts, written at startup |

## Command line

```
proteus [OPTIONS] [-- COMMAND...]
proteus +TOOL ...
```

| Option | Effect |
| --- | --- |
| `-c`, `--config FILE` | Use this Lua file instead of the default `init.lua`. A relative path is taken from the current directory, and `require` and relative paths in the file resolve next to it. Unlike the default file, a missing `FILE` is an error. |
| `--no-config` | Ignore the config file and use built-in defaults. Cannot be combined with `--config`. |
| `--check-config` | Evaluate the config and exit. Prints `Config OK`, or the error with exit status 2. Opens no window and loads no WASM plugins. |
| `--list-keybindings` | Print the configured key bindings as JSON (`mode`, `key`, `action`). Default bindings added by running plugins are not included. |
| `--list-actions` | Print every action name that can be bound to a key, and exit. `tab.goto_N` stands for `tab.goto_1` and so on. |
| `--diagnostics` | Print support information (see below) and exit. |
| `--listen SOCKET` | Enable the control socket at this path (see [Control socket](#control-socket)). |
| `--font FAMILY` | Font family or fontconfig pattern. Overrides `font.family`, also after config reloads. |
| `--font-size PT` | Font size in points, limited to 4 to 96. Overrides `font.size`, also after reloads. |
| `-l`, `--layout NAME` | Open a layout: a name from `$XDG_CONFIG_HOME/proteus/layouts` (without `.lua`), or a path. A value containing `/` or ending in `.lua` is a path. Cannot be combined with `COMMAND`. |
| `--emit-lua-types DIR` | Write Lua type annotations for lua-language-server to `DIR/proteus.lua`, print the path, and exit. |
| `-h`, `--help` | Print help. |
| `-V`, `--version` | Print the version. |

Exit status 2 means a usage, config or startup error.

### Run a command instead of the shell

Everything after `--` is the program and its arguments. It runs in the first
pane; panes you open later start your shell.

```sh
proteus -- htop -d 5
proteus --font-size 14 -- nvim notes.md
```

To change the program for every new pane, set `shell.command` in the config
instead (see [configuration.md](configuration.md#shell)).

A command or `--layout` on the command line also skips session restore for that start.

### Tools

Tools are subcommands that run inside an existing terminal instead of opening a
window. They cannot be mixed with the window options above.

**`proteus +icat [OPTIONS] FILE|URL...`** shows images using the kitty graphics
protocol. It works in Proteus and in other terminals that support that protocol.

| Option | Effect |
| --- | --- |
| `FILE`, `URL` | PNG, JPEG, GIF or WebP files, or `http://`/`https://` URLs (downloaded with `curl`) |
| `--place COLSxROWS@XxY` | Draw inside a box of `COLS` by `ROWS` cells at column `X`, row `Y` (0-based), for example `40x20@0x0`. The cursor does not move. |
| `--scale` | Scale small images up to the terminal width (or the `--place` box) |
| `--clear` | Delete all images shown in the terminal, before showing any `FILE` |
| `--inline` | Always send the image data inline, even for local files |

```sh
proteus +icat screenshot.png
proteus +icat --place 40x20@0x0 --scale logo.png
proteus +icat --clear
```

Local PNG files are sent by path so the terminal reads them itself. Other
formats, and all images when `SSH_CONNECTION`, `SSH_CLIENT` or `SSH_TTY` is set,
are converted to PNG and sent inline. See [images.md](images.md).

**`proteus +msg [--socket SOCKET] REQUEST`** controls a running instance through
its control socket. See the next section.

## Control socket

The control socket lets scripts list panes, read their text, type into them and
run actions. It is off by default. Enable it with `--listen` or in the config:

```lua
require("proteus").config { ipc = { socket = "/run/user/1000/proteus.sock" } }
```

`--listen` takes precedence over `ipc.socket`. A relative path is resolved from
the directory Proteus was started in. At startup Proteus prints
`proteus: control socket PATH` to its standard error.

Security behaviour you may notice:

- The socket file is created with mode `0600`, and connections from other user
  IDs are ignored.
- Proteus does not replace an existing file, a symbolic link, or a socket that
  another instance is listening on; it starts without a control socket and
  prints the error. A stale socket left by a crash is replaced.
- The socket is removed when Proteus exits.
- Programs running in Proteus see the socket path in `PROTEUS_SOCKET`. When the
  socket is disabled, `PROTEUS_SOCKET` is removed from their environment, so a
  nested Proteus does not inherit an outer one's socket.

### Using `proteus +msg`

Inside a pane, `+msg` uses `PROTEUS_SOCKET`. Elsewhere pass `--socket PATH`.
Results are printed as JSON. Errors are printed to standard error with exit status 2.
Pane IDs come from `list`. Where `--pane` is optional, the focused pane is used.

| Request | Effect | Result |
| --- | --- | --- |
| `list` | List all panes | Array of `{id, title, name, cwd, remote, process, command, window, tab, focused, floating, cols, rows}` |
| `send-text [--pane ID] [--paste] TEXT` | Write `TEXT` to the pane. With `--paste`, send it as a paste (bracketed paste when the program enabled it; paste protection prompts are not shown). | `{"pane": ID}` |
| `read [--pane ID] [--scrollback N]` | Screen text, plus up to `N` lines of history above it (at most 100000; at most 2 MiB of text). Refused for private panes. | A JSON string |
| `focus ID` | Focus the pane, switching tab if needed | `{"pane": ID}` |
| `close ID` | Ask to close the pane. The normal close confirmation still applies. | `{"pane": ID}` |
| `action [--pane ID] ACTION` | Run a key action by name (see `--list-actions`). With `--pane`, that pane is focused first. | `null` |
| `split [--pane ID] [--axis x\|y]` | Split the pane. `x` places the new pane beside it, `y` below it; without `--axis` the longer side is split. | `{"pane": NEW_ID}` |
| `new-tab [--cwd DIR] [-- COMMAND...]` | Open a tab, optionally in `DIR` (must exist) and running `COMMAND` | `{"pane": NEW_ID}` |

Examples, from a shell running inside Proteus:

```sh
proteus +msg list
proteus +msg send-text --pane 3 $'make test\r'      # \r presses Enter
proteus +msg read --scrollback 200 | jq -r .
proteus +msg split --axis y
proteus +msg new-tab --cwd ~/src -- htop
proteus +msg action tab.next
```

`send-text` writes the text exactly; use a carriage return (`\r`) to press Enter.

### Protocol

Other clients can speak the protocol directly. Each connection carries one
request: a single line of JSON terminated by a newline, at most 1 MiB. The reply
is one JSON line, either `{"result": ...}` or `{"error": "message"}`. Requests
time out after about 3 seconds.

The request object has an `op` field with the request name (`list`,
`send-text`, `read`, `focus`, `close`, `action`, `split`, `new-tab`) and the
request's fields: `pane`, `text`, `paste`, `scrollback`, `id` (the action name
for `action`), `axis`, `cwd`, `command` (an array of strings). Unknown fields
are rejected.

```sh
printf '%s\n' '{"op":"send-text","pane":3,"text":"ls\r"}' | socat - UNIX-CONNECT:"$PROTEUS_SOCKET"
```

## Environment of programs in a pane

| Variable | Value |
| --- | --- |
| `TERM` | `proteus` when its terminfo entry is installed, otherwise `xterm-256color`. `shell.term` overrides it. |
| `TERMINFO_DIRS` | Prefixed with Proteus's terminfo directory when the entry is installed |
| `COLORTERM` | `truecolor` |
| `TERM_PROGRAM`, `TERM_PROGRAM_VERSION` | `proteus` and the Proteus version |
| `PROTEUS_SOCKET` | The control socket path, when enabled |

`LINES` and `COLUMNS` are removed. The terminfo entry is compiled with `tic` on
first use into `$XDG_DATA_HOME/proteus/terminfo/`. If you log in to other
machines over plain `ssh`, they will not have this entry; set
`shell = { term = "xterm-256color" }` or see [ssh.md](ssh.md).

### Shell integration

Proteus loads a small integration script into bash, zsh and fish automatically.
It reports where prompts and commands start and end (OSC 133) and the current
directory (OSC 7). This is what enables [command blocks](blocks.md), jumping
between prompts, opening new panes in the same directory, and notifications for
long commands. Your own startup files still run normally.

How it is injected:

- zsh: `ZDOTDIR` points to Proteus's directory for startup; the script restores your `ZDOTDIR`.
- fish: Proteus's directory is added to the front of `XDG_DATA_DIRS`.
- bash: bash is started with `--posix` and `ENV` set to the script, which
  switches POSIX mode off again and reads your normal startup files. This is
  only done when bash has no arguments other than `-i`; with other arguments
  (for example `shell.command = { "bash", "-l" }`) bash runs without integration.

Other shells and other programs get no integration. To turn it off:

```lua
require("proteus").config { shell = { integration = false } }
```

## Troubleshooting

Check the config without opening a window:

```sh
proteus --check-config
proteus -c ./test.lua --check-config
```

Print support information:

```sh
proteus --diagnostics
```

This prints the version, platform, which config file was loaded, the font,
the number of declared plugins, the `WAYLAND_DISPLAY`, `XDG_SESSION_TYPE`,
`XDG_CURRENT_DESKTOP`, `LANG`, `LC_ALL`, `TERM` and `COLORTERM` variables, the
`TERM` that panes will get, the Wayland protocols the compositor offers (or why
Wayland is unavailable), and key bindings that are declared more than once
(the last declaration wins), and the path of the log file. A config error is
printed after the report with exit status 2.

If the config has an error when Proteus starts, it starts with the defaults and
shows the error in the window.

### Logs

Proteus writes a log to `$XDG_STATE_HOME/proteus/proteus.log` (by default
`~/.local/state/proteus/proteus.log`). The file is private to your user, and
every running instance appends to it; each line names its thread, and each
instance starts with a `proteus started` line that has its process ID. When
the file is larger than 8 MiB at startup, it is renamed to `proteus.log.old`
and a new one is started. `proteus --diagnostics` prints the path.

By default the log file records Proteus's own messages at the `info` level
and above (windows and panes opening and closing, the GPU adapter, driver and
surface format, config reloads, errors with their cause and the file or
program involved) and warnings and errors from libraries such as wgpu.
Standard error shows only warnings and errors. Every error shown in the window
(a toast or notice) is also in the log; Lua errors are logged with the full
Lua traceback and the hook, timer, key binding or status segment that ran the
callback, while the window shows only the first line.

Two environment variables give more detail:

- `PROTEUS_DEBUG=1` records Proteus's messages at the `debug` level in the
  file: commands started in panes, SSH command lines, IPC connections,
  Wayland protocols found or missing, and why a pane counts as remote.
- `PROTEUS_LOG` sets the filter for both the file and standard error, in
  [`EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html)
  syntax. For example, `PROTEUS_LOG=debug` logs everything at `debug`,
  including libraries, and `PROTEUS_LOG=proteus_wasm=trace,info` traces the
  plugin runtime and keeps everything else at `info`. When it is set, standard
  error shows the same messages, with the module each came from. An invalid
  value prints a warning and the defaults are used.

Proteus does not write clipboard contents, typed or pasted text, or terminal
output to the log. It does record paths and, at `debug`, the commands panes
run, so check it before attaching it to a public report.
