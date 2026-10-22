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
| Lifecycle | Hot reload on save | Install / enable / disable / update at runtime |
| Threading | Runs on the main thread; must be fast (budget per hook, warn if > 5 ms) | Each plugin instance in its own worker thread |

## Lua config

- Entry: `$XDG_CONFIG_HOME/proteus/init.lua`; `require` resolves within that dir.
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
  if ev.exit_code ~= 0 and ev.duration > 30 then p.notify("failed: " .. ev.command) end
end)

p.status.segment("right", function() return os.date("%H:%M") end)
```

### Lua API (v1)

- Only while `init.lua` runs: `p.config{...}`, `p.keymap(mode, key, action)` (action name, `"none"`/`false`, or a function; `keys.MODE` tables also accept functions).
- Any time: `p.on(event, f)`, `p.status.segment("left"|"right", f)`, `p.palette.add(name, f)`, `p.after(secs, f)` / `p.every(secs, f)` (return a timer with `:stop()`), `p.version`.
- In callbacks only (they need the terminal): `p.panes.list/focused/get/new_tab/float`, `p.run_action(name)`, `p.notify(text, { level, timeout })`, and pane objects: fields `id title name cwd process command window tab focused floating cols rows` (read live), methods `split{dir, cmd, cwd}`, `send_text`, `insert_text` (as a paste), `close` (asks if something runs), `focus`, `set_name`, `read_screen{scrollback}`, `read_blocks`, `block_text(id, part)`, `info`, `exists`.
- Events: `pane_opened`, `pane_closed`, `focus_changed` (`previous`), `cwd_changed`, `title_changed`, `command_started`, `command_finished` (`command`, `exit_code`, `duration`), `bell`, `config_changed`. `block_state_changed` (`block`, `state`). `theme_changed` comes with themes. Surfaces: `p.ui.open` (see Status of v1).
- Callbacks never run inside an event handler: events are queued and run before the next frame, so a hook can cause more events (at most 1000 calls per pass). Status segments run after events and once a second.
- Budget: time in the callback itself (not in host calls such as starting a shell) over 5 ms warns once per function; over 2 s the callback is stopped. `init.lua` itself may run 5 s.
- Errors in callbacks and messages show as toasts in the top right corner (the same error at most once per 30 s) and on stderr.
- Hot reload: the config directory and its subdirectories (not `layouts/`) are watched; a changed `.lua` file reloads after 150 ms. A failing reload shows the error and keeps the running config and script. Fonts, keys, bars and limits apply at once; terminal settings (scrollback, clipboard, image limits, shell) apply to new panes. Command-line `--font`/`--font-size` keep winning.

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

### Lifecycle

- Install: `proteus +plugin install <path|archive|git-url>` (also `update`, `list`, `info`, `enable`, `disable`, `revoke`, `remove`; tools take a `+` like `+icat`). Recorded in `$XDG_DATA_HOME/proteus/plugins.lock` with source, sha256 of `plugin.wasm` (checked before every load), enabled flag and granted permissions. Running terminals watch the lock file and start/stop plugins at once. In the terminal, the palette has `plugins: enable/disable/restart NAME`.
- Enable/disable/update/remove at runtime; state survives reload via plugin data dir.
- Crash or trap in a plugin: instance stopped, error shown, terminal unaffected; auto-restart up to 3 times.
- Resource limits per instance: memory (default 256 MiB), fuel/epoch-based CPU interruption for runaway loops.

### Permissions

| Permission | Grants |
|---|---|
| `ui` | Create surfaces, status segments, palette actions, keybinding suggestions |
| `read-panes` | Pane metadata (cwd, title, process, block states) |
| `read-screen` | Grid and scrollback text of panes |
| `write-pty` | `send_text` / `insert_text` into panes |
| `fs:<path>:<r\|rw>` | WASI preopened directories |
| `run-command:<bins>` | Spawn listed executables (no shell), with stdout/stderr capture |
| `network:<hosts>` | Outbound HTTP(S) to listed hosts only (host-provided client) |
| `clipboard-read` / `clipboard-write` | Clipboard |
| `secrets` | Read/write own entries in Secret Service keyring |
| `open-pane` | Spawn terminal panes with commands |

- Shown on install with plain-language descriptions; must be approved. Updates that add permissions require re-approval.
- Per-plugin revocation in the plugin manager (palette: "Plugins").
- Panes marked "private" (doc 06) are invisible to `read-screen` for every plugin.

### Status of v1 (M5)

Implemented: `wit/proteus.wit` in `crates/proteus-plugin` (the SDK), host in `crates/proteus-wasm` (wasmtime 50, WASI p2), GUI in `gui/plugins.rs`, example `examples/plugins/pane-list`.

- Each instance runs in its own thread; a call into it may run 5 s (epoch interruption, also used to stop it at once), memory 256 MiB. Traps and timeouts stop the instance and are shown; it is restarted after 1 s, at most 3 times a session. `init` failures are not retried.
- Permissions: all of the table above. `run-command` lists program names (looked up in `PATH`, run without a shell in their own process group; `run` with a timeout, `spawn-process` streaming stdout/stderr); `network` lists hosts (`*.example.com`, `host:port`; ureq with rustls and the system's certificates, no redirects followed); `secrets` uses the Secret Service (entries tagged with the plugin id). Every plugin has its own `/data` directory (`$XDG_DATA_HOME/proteus/plugin-data/ID`) and a key-value store next to it (`ID.storage.toml`, 1 MiB); `fs` grants are visible at their real paths, with `watch` (inotify), `trash` (freedesktop), `open` and `mime-type`. Time spent in host calls does not count against the 5 s limit.
- Revoking: `proteus +plugin revoke ID PERMISSION` or the palette (`plugins: revoke PERMISSION from NAME`); `run-command:git` / `network:HOST` take back one entry.
- Surfaces: `sidebar-left`, `sidebar-right` (take columns from the panes), `float` (centered), `overlay` (below the focused pane's cursor), and `pane-right`, `pane-down`, `tab` (a pane in the layout with no PTY). Layouts may hold plugin panes (`{ plugin = "ID", args = "..." }`); the plugin hears `surface-opened(id, args)` once it runs, and again after a restart (its panes stay while it is stopped). Frames are styled spans plus image placements (`load-image` RGBA, sent to the surface terminal with the kitty protocol); the host writes them into a terminal with control characters of the plugin's text removed, drawn by the pane renderer. Keys reach a focused surface (or a focused plugin pane) unless the mode binds them; `sidebar.focus` (Ctrl+Shift+B in every preset) cycles the keyboard through the window's sidebars and floats, then back to the panes; mouse events have press, release and drag; files dropped from other programs arrive as `drop(paths)`. Input-method text arrives as `text`; while composing, the host draws the preedit at the frame's cursor and sends `preedit(text, cursor)` (none when it ends), so a plugin need only place its cursor.
- `list-dir(path)`: a directory's entries with type, size and mtime in one call (WASI needs one call per entry).
- Compiled plugins are kept in `<data>/compiled/<sha256>-<engine>.cwasm` (mode 0600, used only if owned by the user and not group/other-writable), so only the first start compiles.
- `register-action(name, key)`: the key (`alt+e`, or `MODE:KEY` such as `pane:e` for another mode) is bound unless the user's keymap uses it. `pane-remote(id)` names the host of a pane whose cwd (OSC 7) is on another machine. `start-drag(surface, paths)` during a drag on a surface: released over a terminal pane, the paths are pasted there shell-quoted. `prompt` and `confirm` use the input line and the bottom prompt (`answered`); tasks show in the status bar and can be cancelled from the palette (`task-cancelled`). `block-state-changed` and `config-changed` events; `clipboard-read/write`.
- `pane.toggle_private` hides a pane from `read-screen` and block events.
- SDK helpers: styled text, `fit`/`pad`/`clip`, `List`, `TextInput`, `Tree`, `Table`, `Spinner`, `ScrollView`.
- Lua: `p.ui.open{ kind, title, cols, rows, focus, pane, on_event }` gives scripts the same surfaces (`s:draw(lines, { cursor, background })`, `s:close()`, `s:focus()`, `s:size()`; lines are strings or lists of span tables; rows and columns 1-based). They close when the config reloads.

## Host API (WIT, v1)

Grouped interfaces; Lua module `proteus` mirrors the same names.

- `events`: subscribe to `pane_opened`, `pane_closed`, `focus_changed`, `cwd_changed`, `command_started`, `command_finished`, `block_state_changed`, `key` (only for the plugin's own surfaces), `theme_changed`, `config_changed`, `timer`.
- `panes`: list, focused, metadata, `split`, `new_tab`, `float`, `close`, `send_text`, `insert_text`, `read_screen`, `read_blocks`.
- `ui`: surfaces (see below), `palette.register_action`, `status.register_segment`, `notify`, `prompt` (input dialog), `confirm`.
- `fs` (beyond WASI): `watch`, `trash`, `open`, `mime-type`.
- `image`: `load-image` (RGBA pixels the plugin decoded, e.g. with the `image` crate), `drop-image`; placed with `frame.images` on a surface.
- `process`: `run` (with a timeout) and `spawn-process` (streaming output, `process-write`, `process-kill`), allowlisted.
- `http`: `http` (whole response) and `http-stream` (`http-response`, `http-body`, `http-done` events, for SSE), `http-cancel`.
- `secrets`: get/set/delete own keys.
- `tasks`: background task handle with progress, cancel.
- `storage`: plugin data dir + small key-value store.

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
