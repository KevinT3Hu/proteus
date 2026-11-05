# 05 — Scripting: Lua Config & WASM Plugins

## Goals

- Lua 5.4 (D6, D7) is the only config format; also used for personal automation.
- WASM is the distributable plugin format, sandboxed with explicit permissions.
- One host API, two bindings. The file manager (doc 03) and AI (doc 06) are built on it with no private hooks.

## Roles

| | Lua 5.4 (`mlua`, vendored) | WASM (`wasmtime`, component model, WIT) |
|---|---|---|
| Purpose | Config, keybinds, themes, hooks, small actions, layouts | Distributable plugins with UI and background work |
| Trust | Full (it is the user's own code) | Sandboxed; capability permissions |
| Lifecycle | Config reload replaces hooks, timers and UI | Explicit source declarations; resolve, build, cache and run |
| Threading | Runs on the main thread; must be fast (budget per hook, warn if > 5 ms) | Each plugin instance in its own worker thread |

## Actions and config editing

Open Actions with Ctrl+Shift+P. Tab opens matching completions, or all entries
when the query is empty. Tab/Shift+Tab or Up/Down navigate; Enter or a click
chooses an entry. Typing filters the open list. Escape dismisses the popup,
then closes the input bar.

Choose **Open config in editor** (`config.open`) to open the running instance's
selected Lua file, including a custom `-c` path, in a new pane. The editor is
`VISUAL`, then `EDITOR`, then `vi`. Missing parent directories are created; the
editor saves the file. Saving uses normal config hot reload. With `--no-config`,
the action reports that no file was selected. Bind it directly, for example:

```lua
p.keymap("normal", "ctrl+alt+s", "config.open")
```

## Lua config

- Entry: `$XDG_CONFIG_HOME/proteus/init.lua`; override with `proteus -c /path/to/config.lua` (or `--config`). `require` and relative WASM source paths resolve within the selected config directory. `--no-config` uses defaults and conflicts with `-c`.
- Typed API stubs shipped as LuaLS annotations (`proteus --emit-lua-types <dir>` writes `<dir>/proteus.lua`; add the dir to `workspace.library`) for completion and diagnostics in nvim/VS Code.
- No config required; `init.lua` overrides defaults.
- Errors: config error at startup or reload shows a non-modal banner, keeps the last good config.

```lua
local p = require("proteus")

p.config {
  font = { family = "JetBrains Mono", size = 11 },
  keys = { preset = "zellij", leader = "ctrl+g" },
}

p.keymap("normal", "alt+t", "tab.new")
p.keymap("pane", "g", function(ctx) ctx.pane:split { cmd = "lazygit" } end)

p.on("command_finished", function(ev)
  if ev.exit_code ~= nil and ev.exit_code ~= 0 and (ev.duration or 0) > 30 then p.notify("failed: " .. (ev.command or "(unknown command)")) end
end)

p.status.segment("right", function() return os.date("%H:%M") end)
```

### Inline code and ordinary modules

Lua is the config language, including extensions written by the user. There is
no Lua plugin type, registration step, manifest, plugin ID, install command or
separate Lua plugin lifecycle. Every `p.*` API below is directly available in
`init.lua`. Optional modularization is ordinary Lua:

```lua
-- init.lua
local p = require("proteus")
require("extensions.clock")
require("extensions.panel")
p.config { font = { size = 13 } }
p.on("command_finished", function(ev)
  if ev.exit_code ~= nil and ev.exit_code ~= 0 then p.notify("Command failed: " .. (ev.command or "(unknown command)")) end
end)
```

`extensions/clock.lua` can simply call
`require("proteus").status.segment("right", function() return os.date("%H:%M") end)`.
Modules execute in the same Lua state with the same config API as inline code;
normal Lua `require` caching applies within a load. A config reload creates a
fresh state, runs `init.lua` and its modules again, and replaces their registered
hooks, keymaps, timers and UI together. Later config calls override earlier
ones. Nothing loads a Lua directory automatically. Copyable examples, including
a custom panel built with `p.ui.open`, are in [`examples/config`](../../examples/config).

`p.ui.open{ kind, title, cols, rows, focus, pane, on_event }` opens config-owned
UI from a callback. Its surface supports `draw`, `close`, `focus`, and `size`;
frames can use styled spans. It requires no WASM runtime or plugin declaration.

### Lua API (v1)

- Only while `init.lua` runs: `p.config{...}`, `p.keymap(mode, key, action)` (action name, `"none"`/`false`, or a function; `keys.MODE` tables also accept functions).
- Any time: `p.on(event, f)`, `p.status.segment("left"|"right", f)`, `p.palette.add(name, f)`, `p.after(secs, f)` / `p.every(secs, f)` (return a timer with `:stop()`), `p.version`.
- In callbacks only (they need the terminal): `p.panes.list/focused/get/new_tab/float`, `p.run_action(name)`, `p.notify(text, { level, timeout })`, and pane objects: fields `id title name cwd process command window tab focused floating cols rows` (read live), methods `split{dir, cmd, cwd}`, `send_text`, `insert_text` (as a paste), `close` (asks if something runs), `focus`, `set_name`, `read_screen{scrollback}`, `read_blocks`, `block_text(id, part)`, `info`, `exists`.
- `command_finished.exit_code` is `nil` when the status is unknown (for example, a nested shell prompt closes the previous command block). Only a known nonzero code indicates failure; guard against `nil` in notification hooks.
- Events: `pane_opened`, `pane_closed`, `focus_changed` (`previous`), `cwd_changed`, `title_changed`, `command_started`, `command_finished` (`command`, `exit_code`, `duration`), `bell`, `config_changed`. `block_state_changed` (`block`, `state`). `theme_changed` fires after configured colors change. Surfaces: `p.ui.open` (see Status of v1).
- Callbacks never run inside an event handler: events are queued and run before the next frame, so a hook can cause more events (at most 1000 calls per pass). Status segments run after events and once a second.
- Budget: time in the callback itself (not in host calls such as starting a shell) over 5 ms warns once per function; over 2 s the callback is stopped. `init.lua` itself may run 5 s.
- Errors in callbacks and messages show as toasts in the top right corner (the same error at most once per 30 s) and on stderr.
- Hot reload: the config directory and its subdirectories (not `layouts/`) are watched; a changed `.lua` file reloads after 150 ms. A failing reload shows the error and keeps the running config and script. Fonts, themes, padding, cursor appearance, keys, bars and limits apply at once; terminal settings (scrollback, clipboard, image limits, shell) apply to new panes. Command-line `--font`/`--font-size` keep winning.

## Themes

Themes are configured in `init.lua` and hot reload across all windows, existing
terminal panes, and Lua/WASM surfaces. No restart is required. With no theme
settings, the original appearance is unchanged.

```lua
local p = require("proteus")
p.config {
  theme = {
    preset = "dark", -- "default" (same as dark), or "light"
    foreground = "#cdd6f4",
    background = "#1e1e2e",
    cursor = "#f5e0dc",
    ansi = {
      "#45475a", "#f38ba8", "#a6e3a1", "#f9e2af",
      "#89b4fa", "#f5c2e7", "#94e2d5", "#bac2de",
      "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af",
      "#89b4fa", "#f5c2e7", "#94e2d5", "#a6adc8",
    },
    indexed = { [208] = "#fab387" },
    ui = {
      foreground = "#cdd6f4", background = "#1e1e2e",
      accent = "#89b4fa", bar = "#313244", dim = "#6c7086",
      selection_foreground = "#cdd6f4", selection_background = "#45475a",
      search_foreground = "#1e1e2e", search_background = "#f9e2af",
      search_current_background = "#fab387", cursor_text = "#1e1e2e",
      bell = "#f5e0dc",
    },
  },
  font = { family = "JetBrains Mono", size = 13, ligatures = true },
  cursor = { blink = false, shape = "block" },
  ui = { padding = 8, tab_bar = "always", pane_titles = "auto" },
}
```

`foreground` and `background` are terminal defaults; `cursor` is the cursor
color. `ansi` is exactly 16 colors, in ANSI order (black, red, green, yellow,
blue, magenta, cyan, white, then their bright variants). `indexed` accepts
integer keys 0–255, and takes precedence over `ansi` in the same call. Entries
16–255 otherwise use the xterm cube and gray ramp. Colors accept `#rgb`,
`#rrggbb`, longer X11 hex forms, and `rgb:r/g/b`; names and alpha channels are
not supported. Invalid colors, indices, presets, and unknown fields reject the
reload and leave the previous configuration running.

`theme.ui` styles chrome and renderer decorations. `accent` colors active
tabs, mode badges, hint labels and focused borders; `bar` colors inactive tabs
and bars; `dim` colors inactive text and pane dividers. The remaining fields
style selection, search matches, block cursor text and the visual bell.
Omitted UI colors preserve the original automatic behavior: chrome follows the
focused pane's foreground/background and ANSI blue; selection inverts cell
colors; search uses ANSI yellow and palette entry 208. Explicit UI colors stay
fixed when applications change their terminal palette. Status segments use the
chrome colors; custom surfaces can still supply their own span colors.

Multiple `p.config` calls merge individual theme fields, including nested UI
fields and indexed entries. A `preset` resets the entire theme first, then
applies the rest of that call, independent of table ordering. `false` clears
an optional UI/cursor override or an indexed entry back to its built-in default.
Required foreground/background colors cannot be `false`. To keep themes in
separate files, return a theme table from `themes/mytheme.lua` and use
`p.config { theme = require("themes.mytheme") }`.

Application OSC 4/10/11/12 overrides survive config reloads. OSC resets and
terminal reset return to the latest configured defaults. Explicit true-color
text from an application retains its requested colors. Plugins receive updated
`ThemeChanged` colors, and Lua can subscribe with
`p.on("theme_changed", function(ev) ... end)` after a theme reload.

`cursor.shape` accepts `app` (default), `block`, `bar`, or `underline`; an explicit
shape overrides application requests while configured.

### Typography and geometry

All colors can be supplied directly; a preset is optional. A complete standalone
example with custom colors, fonts and geometry is in
[`examples/appearance.lua`](../../examples/appearance.lua).

```lua
p.config {
  font = {
    family = "JetBrains Mono", size = 13, ligatures = true,
    bold_family = "JetBrains Mono", italic_family = "JetBrains Mono",
    bold_italic_family = "JetBrains Mono",
    cell_width = 12, cell_height = 24,
    letter_spacing = 0, line_spacing = 2,
    offset_x = 0, offset_y = 0,
  },
  window = { width = 1200, height = 800 },
  -- Alternatively: window = { columns = 120, rows = 36 },
  ui = {
    padding = { horizontal = 12, vertical = 8 },
    pane_gap = 8, border_width = 1,
    tab_bar = "always", pane_titles = "auto", hint_bar = "in-mode",
  },
}
```

Font size is in points (4–96); all other pixel dimensions are **logical pixels**,
scaled with the window's display scale. The regular font determines natural cell
metrics. Optional `bold_family`, `italic_family` and `bold_italic_family` choose
separate primary font families for the respective styles; omitted styles use
variants of the regular family or synthesized styles. Fontconfig resolves font
names and provides missing-character fallback. All terminal and UI text shares
the same cell grid and point size.

`cell_width` and `cell_height` override the natural cell dimensions (1–512).
`false` restores automatic metrics. `letter_spacing` and `line_spacing` add to
those dimensions, whether explicit or automatic (-64–256); the resulting
physical cell size is rounded and clamped to at least one pixel. Changing cell
size changes the grid without stretching the glyphs. Added vertical space is
split around the baseline. `offset_x` and `offset_y` move glyphs relative to
cells (-256–256, positive right/down) without changing layout. Very small cells
or large offsets can overlap neighboring glyphs. Font style overrides accept
`false` to restore automatic selection.

`window.opacity` (0–1, default 1) makes the default background translucent;
cells with their own background color, the tab bar and the bottom bar stay
opaque. It needs a surface that supports premultiplied alpha. With
`window.background_blur = true`, Proteus asks the compositor to blur what shows
through, using `ext-background-effect-v1`; compositors without that protocol
(such as GNOME) show the translucent window unblurred. `window.decorations =
"none"` tells the compositor that the window decorates itself and draws no
title bar or borders; `"auto"` (the default) asks for server-side decorations
and draws Proteus's own (as with `"client"`) when the compositor provides none,
as GNOME does.
`"client"` draws Proteus's own decorations in the UI theme: a title bar with
the window title and minimize, maximize and close buttons (drag to move,
double-click to maximize, right-click for the compositor's window menu), a
one-pixel border, and resize grips along the edges. The border and grips are
left out while the window is maximized or tiled, and the title bar while it
is fullscreen. If the compositor insists on drawing decorations itself,
Proteus draws none.
All three apply to open windows when the config is reloaded.

`window.width` and `window.height` choose the **initial entire client window**
size (100–8192 and 60–8192 respectively), excluding compositor decorations.
`window.columns` and `window.rows` choose an initial single-pane grid (2–8192 and
1–8192, defaults 80×24); calculated window size includes padding, the gutter and
explicitly reserved bars/title rows. Pixel dimensions take precedence over grid
dimensions independently on each axis. Use `false` to clear a dimension.
Window dimensions apply to newly created windows; an unrelated config reload
does not resize an existing window. With no window settings, additional windows
inherit the current window's size. A Wayland compositor's configured size takes
precedence, so a tiling compositor may control the final dimensions. Dynamic
status segments and restored multi-pane layouts can change available grid space.

`ui.padding` accepts a number for uniform padding (default 4), or
`{ horizontal = ..., vertical = ... }` to control each axis (0–256). `pane_gap`
sets the spacing between tiled panes (0–256); omitted or `false` uses one cell
width. `border_width` controls drawn pane borders and divider lines (0–32,
default 1; 0 hides them). Floating pane frame/title allocation remains based on
the cell grid. Fonts, cell geometry, offsets, padding, pane gaps and border
thickness hot reload across all existing windows and update terminal PTY sizes.

Transparency, background images, independently sized per-widget fonts and
arbitrary per-widget layouts are not currently supported.

## WASM plugins

### Package

```
my-plugin/
  proteus-plugin.toml   # manifest
  plugin.wasm           # component, wasm32-wasip2
  assets/               # optional
```

```toml
[plugin]
id = "example.git-ui"
name = "Git UI"
version = "0.3.1"
api = "1"              # host API major version

[permissions]
ui = true
read-panes = true
write-pty = false
fs = ["~/src:rw"]
run-command = ["git"]
network = []
```

- Manifest is TOML because it is data for a sandboxed artifact, not user config (does not conflict with D6).
- SDK: Rust crate `proteus-plugin` (generated from WIT) first; other languages via `wit-bindgen` (Go, C, JS via componentize) as community-supported.

### Loading sources, builds and caching

Load WASM plugins with `p.load_plugin { ... }` while init.lua or its modules run.
Each call loads one plugin; calls accumulate in execution order. No calls means
no WASM plugins. Calls in callbacks after config loading are rejected.
No directories, old installed-plugin stores, or bundled defaults are discovered.
There are no install/enable/disable/update/remove commands or `plugins.lock`.

```lua
p.load_plugin { source = "~/code/proteus/plugins/files" } -- built automatically
p.load_plugin { source = "https://example.com/my-plugin.git", rev = "COMMIT", subdir = "plugin" }
p.load_plugin { path = "~/plugins/tool.wasm", id = "my.tool", permissions = { ui = true } }
p.load_plugin { url = "https://example.com/tool.wasm", id = "remote.tool" }
```

A source is a local WASM file, a package/source directory, a Git clone URL, or
an HTTP(S) URL ending in `.wasm` (query strings are allowed). The argument is a config table
with exactly one of `source`, `path`, or `url`.
Local paths resolve relative to the config directory; `~/` expands normally.
`rev` selects a Git revision; `subdir` selects a package within a repository.

Packages have `proteus-plugin.toml`. If a package has `Cargo.toml`, Proteus runs
`cargo build --release --target wasm32-wasip2`, with an isolated target directory
in the cache. This requires Cargo and the `wasm32-wasip2` target. Cargo handles
incremental compilation and dependency changes; a package must produce exactly
one WASM artifact. Source builds take precedence over any stale sibling WASM.
Prebuilt packages without Cargo load the manifest's `wasm` path. Bare WASM files
may specify `id` and `permissions`; otherwise their stable ID is derived from
the source and they receive no capabilities. A sibling manifest, when present,
supplies metadata and requested permissions; an explicit ID must match it.

Permissions given in `p.load_plugin { permissions = ... }` are granted as is.
Without them, a plugin requesting any permission in its manifest does not start
until the user approves the request at a prompt. Approvals are stored in
`$XDG_STATE_HOME/proteus/plugin-approvals.json` per plugin ID, source and exact
permission set, so a plugin that later requests different permissions asks again.
Declining keeps the plugin stopped for the rest of the session.

`p.load_plugin` validates its config and records a load request; it does not
block Lua on network access or compilation. It may be called conditionally, in
loops, or from ordinary required Lua modules. Duplicate sources are errors.
The old `p.config { plugins = ... }` option is rejected.

Fetch/build work runs off the GUI thread. Successful config reloads resolve
sources again and rebuild local Rust sources incrementally. Removing its `p.load_plugin` call
stops it immediately; stale background build results cannot start an undeclared
plugin. Changed artifacts or permissions restart the affected instance; unchanged
ones keep running. Two sources with the same manifest ID are rejected together.
Layouts can reference a declared plugin's ID but cannot load an undeclared one.
The palette exposes restart for declared/resolved plugins, including failed ones.

Downloaded Git snapshots and raw WASM are reused from
`$XDG_CACHE_HOME/proteus/plugins` (default `~/.cache/proteus/plugins`); cache
contents never cause loading by themselves. Git snapshots are keyed by URL and
`rev`; use a new revision or remove the relevant cached source to fetch an update.
Cargo artifacts are cached on disk; Wasmtime's native compilation is cached in memory only (see below). Set
`PROTEUS_CACHE_DIR` to override the cache root. Plugin data remains in
`$XDG_DATA_HOME/proteus/plugin-data/ID` and survives removal from the config.
The first source to run with an ID owns that ID's data, storage and secrets
(`$XDG_DATA_HOME/proteus/plugin-owners/ID`); a different source declaring the
same ID does not start until that file is deleted.

Crash/trap isolation, bounded restart attempts, and runtime resource limits
remain unchanged. Lua config and its modules are not WASM sources and must use
ordinary `require`, rather than calls to `p.load_plugin`.

### Stable action IDs and user keymaps

A plugin registers a stable local ID, a display label and an optional default key:

```rust
host::register_action("sidebar.toggle", "Files: sidebar", Some("alt+e"))?;
host::register_action("pane.open", "Files: pane", Some("pane:E"))?;
```

The label appears in the command palette. The plugin receives
`Event::Action("sidebar.toggle")` from both the palette and a keybinding.
The ID is independent of the label and uses 1–100 ASCII letters, digits, dots,
underscores or hyphens (case-sensitive). Re-registering an ID replaces its
label and default, without duplicating the palette entry. `None` removes its
default key but preserves explicit user bindings.

Users bind plugin actions through the same config paths as built-in actions:

```lua
local p = require("proteus")
p.config {
  keys = {
    normal = {
      ["alt+e"] = false, -- disable the plugin's default
      ["ctrl+alt+e"] = "plugin:proteus.files:sidebar.toggle",
    },
    pane = {
      E = false,
      v = "plugin:proteus.files:pane.open",
    },
  },
}
-- p.keymap also accepts the same stable action reference:
p.keymap("normal", "alt+f", "plugin:proteus.files:picker.open")
```

Action references have the form `plugin:PLUGIN-ID:ACTION-ID`, and may be used
before the plugin has started. The plugin must still be explicitly declared
with `p.load_plugin`; a keybinding does not load a source. As with built-in actions,
binding another chord adds a binding; set the old chord to `false`/`"none"`
to remove it.

Plugin defaults fill keys unoccupied by the preset. Every explicit user entry
wins, including unbindings and Lua callbacks. Defaults with `MODE:KEY` apply
only in that mode. If plugin defaults conflict, source declaration order then
stable action ID determines priority. Keymaps rebuild when registrations,
config, or running plugins change, so stopped/replaced defaults cannot linger
or override a user's unbinding.

The Rust SDK's `host::register_action(id, label, default_key)` uses the additive
WIT `register-action-id` function. The older name-based WIT import remains
available for prebuilt v1 components; its name acts as its legacy action ID.

### Permissions

| Permission | Grants |
|---|---|
| `ui` | Create surfaces, status segments, palette actions, keybinding suggestions |
| `read-panes` | Pane metadata (cwd, title, process, block states) |
| `read-screen` | Grid and scrollback text of panes |
| `write-pty` | `send_text` / `insert_text` into panes |
| `fs:<path>:<r\|rw>` | WASI preopened directories; existing single-file paths are rejected |
| `run-command:<bins>` | Spawn listed executables (no shell), with stdout/stderr capture |
| `network:<hosts>` | Outbound HTTP(S) to listed hosts only (host-provided client) |
| `clipboard-read` / `clipboard-write` | Clipboard |
| `secrets` | Read/write own entries in Secret Service keyring |
| `open-pane` | Spawn terminal panes with commands |

- A source declaration is trusted config. WASM capabilities default to the package manifest; an explicit `permissions` table replaces them completely (including an empty table to deny all optional capabilities). Change that table to restrict capabilities. Source builds execute Cargo/build scripts with the user's normal build privileges; the WASM runtime sandbox applies to the resulting component.
- Panes marked "private" (doc 06) are invisible to `read-screen` for every plugin.

### Status of v1 (M5)

Implemented: `wit/proteus.wit` in `crates/proteus-plugin` (the SDK), host in `crates/proteus-wasm` (wasmtime 50, WASI p2), GUI in `gui/plugins.rs`, example `examples/plugins/pane-list`.

- Host traffic is acknowledged after GUI consumption, including frames and logs: each plugin has one host call in flight. Messages are limited to 4 MiB (image loads use the separate 64 MiB image quota); more than 2000 host calls per second stops the instance. Log/status text is truncated; prompts/actions are capped at 16/256. The incoming event queue holds 256 events; overflow stops the instance instead of blocking the GUI or dropping terminal input silently.
- Custom filesystem helpers retain grant-root and target/parent descriptors using Linux `openat2` containment. Linux 5.6 or newer and mounted `/proc` are required; unsupported containment fails closed. Symlink escapes are rejected. Source builds and allowlisted native processes still have normal user privileges, as described above.
- Each instance runs in its own thread; a call into it may run 5 s (epoch interruption, also used to stop it at once), memory 256 MiB. Traps and timeouts stop the instance and are shown; it is restarted after 1 s, at most 3 times a session. `init` failures are not retried.
- Permissions: all of the table above. `run-command` lists program names (looked up in `PATH`, run without a shell in their own process group; `run` with a timeout, `spawn-process` streaming stdout/stderr); children get only `PATH`, `HOME`, `USER`, `LOGNAME`, `LANG`, `LANGUAGE`, `LC_*`, `TERM`, `TZ` and `TMPDIR` from the terminal's environment, and plugins may set only locale, color and `PROTEUS_*` variables. `network` lists hosts (`*.example.com`, `host:port`; ureq with rustls and the system's certificates, no redirects followed; names that resolve to loopback or private addresses are refused unless the URL names `localhost` or an IP address). `open` needs `ui`; URLs must be to a `network` host (or `mailto:`), paths must be under an `fs` grant. A read-only `fs` grant inside a read-write one is rejected. `secrets` uses the Secret Service (entries tagged with the plugin id). Every plugin has its own `/data` directory (`$XDG_DATA_HOME/proteus/plugin-data/ID`) and a key-value store next to it (`ID.storage.toml`, 1 MiB); `fs` grants are visible at their real paths, with `watch` (inotify), `trash` (freedesktop), `open` and `mime-type`. Existing single-file grants fail validation rather than silently disappearing. `file-mode`, `set-file-mode`, `create-private`, and `publish-file` let plugins preserve permissions and stage new files privately without widening their filesystem grants. Time spent in host calls does not count against the 5 s limit, for up to 200 calls a second; sleeping or polling in WASI does count, and is interrupted at the limit or on stop. A plugin holds at most 256 WASI resources (open files, streams).
- Surfaces: `sidebar-left`, `sidebar-right` (take columns from the panes), `float` (centered), `overlay` (below the focused pane's cursor), and `pane-right`, `pane-down`, `tab` (a pane in the layout with no PTY). Layouts may hold plugin panes (`{ plugin = "ID", args = "..." }`); the plugin hears `surface-opened(id, args)` once it runs, and again after a restart (its panes stay while it is stopped). Frames are styled spans plus image placements (`load-image` RGBA, sent to the surface terminal with the kitty protocol); the host writes them into a terminal with control characters of the plugin's text removed, drawn by the pane renderer. Keys reach a focused surface (or a focused plugin pane) unless the mode binds them; plugins register their own actions and default bindings for opening/focusing their surfaces and returning focus with `focus-pane`; mouse events have press, release and drag; files dropped from other programs arrive as `drop(paths)`. Input-method text arrives as `text`; while composing, the host draws the preedit at the frame's cursor and sends `preedit(text, cursor)` (none when it ends), so a plugin need only place its cursor.
- `list-dir(path)`: a directory's entries with type, size and mtime in one call (WASI needs one call per entry).
- Plugins compile from WASM bytes; up to 16 compiled components are cached in memory. Native `.cwasm` files on disk are not deserialized. The old `Spec.compiled` field is ignored for compatibility.
- `register-action-id(id, name, default-key)` (SDK: `host::register_action`): stable IDs and user-overridable defaults as described above. `pane-remote(id)` names the host of a pane whose cwd (OSC 7) is on another machine. `start-drag(surface, paths)` during a drag on a surface: released over a terminal pane, the paths are pasted there shell-quoted. `prompt` and `confirm` use the input line and the bottom prompt (`answered`); tasks show in the status bar and can be cancelled from the palette (`task-cancelled`). `block-state-changed` and `config-changed` events; `clipboard-read/write`.
- `pane.toggle_private` hides a pane from `read-screen` and block events.
- SDK helpers: styled text, grapheme-aware `fit`/`pad`/`clip`, `List`, `TextInput`, `Tree`, `Table`, `Spinner`, `ScrollView`. The `ui` module provides a declarative `Component` trait: `view()` returns an `Element` tree of pages, rows, columns, lists, text, inputs and images. `Component::render` owns layout, clipping, inherited styles, cursors and image placement, and returns a frame plus named regions for hit testing. Custom components nest through `Element::component`; responsive views receive their allocated size. `Canvas` and `Rect` remain the low-level backend. See the [SDK UI guide](../../crates/proteus-plugin/README.md).

## Host API (WIT, v1)

The WIT interfaces below belong to WASM. The Lua config API above uses terminal UI facilities directly.

- `events`: subscribe to `pane_opened`, `pane_closed`, `focus_changed`, `cwd_changed`, `command_started`, `command_finished`, `block_state_changed`, `key` (only for the plugin's own surfaces), `theme_changed`, `config_changed`, `timer`.
- `panes`: list, focused, metadata, `split`, `new_tab`, `float`, `close`, `send_text`, `insert_text`, `read_screen`, `read_blocks`.
- `ui`: surfaces (see below), `palette.register_action`, `status.register_segment`, `notify`, `prompt` (input dialog), `confirm`.
- `fs` (beyond WASI): `watch`, `trash`, `open`, `mime-type`.
- `image`: `load-image` (RGBA pixels the plugin decoded, e.g. with the `image` crate), `drop-image`; placed with `frame.images` on a surface.
- `process`: `run` (with a timeout covering descendants holding pipes) and `spawn-process` (streaming output, `process-write`, `process-kill`), allowlisted. Stdin is queued in order with a 16 MiB bound; a full queue rejects the whole write. Closing stdin drains queued bytes before EOF; stopping cancels pending input.
- `http`: `http` (whole response) and `http-stream` (`http-response`, `http-body`, `http-done` events, for SSE), `http-cancel`.
- `secrets`: get/set/delete own keys.
- `tasks`: background task handle with progress, cancel.
- `storage`: plugin data dir + small key-value store.

The key-value store uses a stable private sidecar lock around read/modify/write,
reloads the latest disk state on each operation, and synchronizes both the new
file and containing directory before a successful save. Separate terminal
processes therefore merge individual key updates instead of overwriting a stale
cached map. Storage files/locks are 0600 and their application-owned parent
directory is 0700 (existing user-owned state directories are migrated).
Missing files mean an empty store; malformed, invalid UTF-8, unreadable, or
oversized files produce errors and are not silently reset or overwritten.
The 1 MiB quota counts logical keys/values; serialized TOML reads are bounded at
8 MiB to accommodate escaping. A failed update has no speculative memory cache;
if rename succeeded before a directory-sync error, reads see the published value
and a repeated set still retries synchronization. The existing WIT API is
preserved: `storage-set` returns its error; legacy get/keys/delete calls also
surface I/O/corruption failures as host error notifications (get/keys return
none/empty when they cannot read). Repair/move a corrupt store explicitly before
retrying. Multi-call get-then-set sequences are not atomic transactions.

## Plugin UI model

- Surfaces: `pane` (occupies a pane in the layout), `sidebar` (left/right dock), `float` (popup), `overlay` (anchored to a pane, e.g. inline AI suggestion), `status` (segment).
- Rendering: retained cell grid. Plugin submits frames of styled cells (text, fg/bg, attributes, hyperlinks) + image placements; host diffs and renders with the terminal renderer, so plugin UIs match terminal look and CJK/IME work for free.
- Input: host delivers key events (kitty-keyboard-level detail), IME commits/preedit for focused text inputs, mouse events in cell coordinates, drag-and-drop.
- Helper widget library in the SDK (list, tree, text input, table, scroll view, spinner) — runs inside the plugin, not in the host.
- Size changes delivered as events; plugin re-renders.

## Versioning

- Host API semver by WIT package version; host supports current major and previous major for one release cycle.
- Lua API follows the same version; deprecated calls warn once.

## Open questions

1. ~~Rendering model~~: retained cell frames (decided in M5).
2. Should Lua be allowed to create UI surfaces too, or only WASM? (Proposed: yes, same `ui` API — useful for small personal widgets.)

Streaming HTTP checks cancellation during socket I/O, TLS handshakes, and output-backlog waits. Cancelled workers keep consuming a stream slot until they exit. `timeout-ms` bounds connection/header work and idle socket waits, so an active long-lived response can continue while a stalled peer cannot hold a worker indefinitely. WASI integration CI should set `PROTEUS_REQUIRE_WASM_TESTS=1` to fail if the target is unavailable.

### Localization

`p.config { language = "zh-CN" }` selects the UI language; omission follows the
locale environment. `p.i18n.load(locale, fluent_source)`, `p.i18n.t(id, args)`, and
`p.i18n.locale()` are available to config code and modules. WASM plugins use
`proteus_plugin::i18n` and `host::locale()`. See [Localization](../i18n.md) for
Fluent resources, typed arguments, locale negotiation, and reload handling.

### Plugin-owned settings

`load_plugin.config` is data for the plugin, separate from host options such as
`source`, `rev`, `id`, and `permissions`:

```lua
p.load_plugin {
  source = "../../plugins/files",
  config = { position = "right", width = 40 },
}
```

The file manager accepts `position = "left"` (default) or `"right"`, and `width`
in terminal columns (default 32, range 1–4096). These control the pane opened by
its Alt+e action. The plugin validates these settings.

Other plugins define their own schemas. Nested tables, arrays, strings, numbers,
and booleans cross the WASM boundary as JSON. Functions and recursive tables are
rejected. Changing `config` on reload restarts that plugin; settings do not change
the source build cache key. An omitted config uses the SDK settings type's defaults.
