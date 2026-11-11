# Proteus documentation

Proteus is a GPU-rendered terminal emulator for Wayland on Linux, with built-in
panes, tabs and windows, command blocks, inline images, Lua configuration and
WASM plugins.

## Start here

- [Getting started](getting-started.md): requirements, installing, first launch,
  command-line options, the control socket and troubleshooting.
- [Configuration](configuration.md): the `init.lua` file and every option.
- [Keys and modes](keys.md): default key bindings, modes, rebinding keys and the
  list of actions.

## Using the terminal

- [Panes, tabs, windows and layouts](panes-and-layouts.md): splitting, floating
  panes, session restore and saved layouts.
- [Shell integration and command blocks](blocks.md): prompt marks, jumping
  between commands, folding, rerunning and notifications.
- [Scrollback, selection, search and hints](scrollback.md): scroll mode,
  copying, the clipboard, paste protection and hints.
- [Images](images.md): kitty, Sixel and iTerm2 images and `proteus +icat`.
- [SSH](ssh.md): connecting to hosts, file transfers and remote workspaces.
- [Project workspaces and tasks](workspaces.md): `.proteus-workspace` files and
  running tasks.

## Extending Proteus

- [Scripting with Lua](scripting.md): events, timers, status segments, palette
  entries and UI surfaces.
- [WASM plugins](plugins.md): loading plugins, permissions and approvals.
- [Files](files.md): the first-party file manager plugin.
- Writing plugins: [plugin SDK](../crates/proteus-plugin/README.md) and
  [examples](../examples/plugins/README.md).

## For contributors

- [Localization](i18n.md): translating Proteus and plugins.
- [Release packaging](releases.md): building release tarballs and Debian
  packages.


## License

Proteus is licensed under the GNU Affero General Public License, version 3 only
(`AGPL-3.0-only`). See [LICENSE](../LICENSE).
