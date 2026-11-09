# Scripting with Lua

Your config file, `$XDG_CONFIG_HOME/proteus/init.lua` (usually
`~/.config/proteus/init.lua`), is a Lua 5.4 program. Besides setting options
with `p.config` (see [configuration.md](configuration.md)), it can bind keys to
Lua functions, react to events, add status bar text and command palette
entries, run timers, and open small UI panels.

The Lua state stays alive after the file has run, so the functions you register
keep working until the config is reloaded.

```lua
local p = require("proteus")

p.keymap("pane", "g", function(ctx) ctx.pane:split { cmd = "lazygit" } end)
p.on("command_finished", function(ev)
  if ev.exit_code ~= nil and ev.exit_code ~= 0 then
    p.notify("failed: " .. (ev.command or "(unknown command)"), { level = "error" })
  end
end)
p.status.segment("right", function() return os.date("%H:%M") end)
p.palette.add("Git status", function(ctx) ctx.pane:send_text("git status\r") end)
```

## Editor completion

Write the type annotations for lua-language-server and point your editor at
them:

```sh
proteus --emit-lua-types ~/.config/proteus/types
```

This creates `~/.config/proteus/types/proteus.lua` and exits. Add that directory
to `workspace.library` in your lua-language-server settings.

## Check and reload the config

- `proteus --check-config` runs the config and exits without opening a window
  (WASM plugins are not loaded). It prints `Config OK`, or the error and a
  nonzero exit status.
- A running Proteus reloads the config when a `.lua` file in the config
  directory or one of its subdirectories changes (except `layouts/`). If the new
  config has an error, a message says so and the previous config stays in use.
- A reload stops all timers, closes all `p.ui` surfaces and then emits
  `config_changed`.

## Splitting the config into modules

`require` looks only inside the config directory: `require("extensions.clock")`
loads `extensions/clock.lua` or `extensions/clock/init.lua` next to `init.lua`.
Lua C modules cannot be loaded. Each module calls `require("proteus")` itself:

```lua
-- extensions/clock.lua
local p = require("proteus")
p.status.segment("right", function() return os.date("%H:%M:%S") end)
```

```lua
-- init.lua
require("extensions.clock")
```

The repository has a complete example in `examples/config/`.

## When each function can be called

| Function | While the config loads | In callbacks |
|---|---|---|
| `p.config`, `p.keymap`, `p.load_plugin`, `p.i18n.load` | yes | no (error) |
| `p.on`, `p.status.segment`, `p.palette.add`, `p.after`, `p.every` | yes | yes |
| `p.panes.*`, pane methods, `p.notify`, `p.run_action`, `p.ui.open` | no (error) | yes |

A callback is any function Proteus calls later: a key binding, an event hook, a
status segment, a palette entry, a timer or a surface's `on_event`.

## Bind a key to Lua

`p.keymap(mode, key, action)` binds `key` in `mode`. The action is an action
name (see `proteus --list-actions`), a Lua function, or `"none"`/`false` to
unbind. Modes are `normal`, `locked`, `menu`, `pane`, `tab`, `resize`, `move`,
`scroll` and `session`. Key syntax is described in [keys.md](keys.md).

```lua
p.keymap("normal", "alt+g", function(ctx)
  ctx.pane:split { dir = "right", cmd = { "lazygit" } }
end)
p.keymap("normal", "alt+x", "pane.close")
p.keymap("normal", "alt+t", false)
```

A function receives `ctx`, where `ctx.pane` is the focused pane.

## React to events

`p.on(event, f)` runs `f(ev)` each time the event happens. `ev.name` is the
event name. An unknown event name is an error.

| Event | Fields of `ev` | When |
|---|---|---|
| `pane_opened` | `pane` | A pane was opened |
| `pane_closed` | `pane` | A pane closed; only `pane.id` still works |
| `focus_changed` | `pane`, `previous` (may be nil) | Another pane got the keyboard |
| `cwd_changed` | `pane`, `cwd` | The shell reported a new directory (OSC 7) |
| `title_changed` | `pane`, `title` | The program set the title |
| `command_started` | `pane`, `command` (may be nil) | A command began (shell integration) |
| `command_finished` | `pane`, `command`, `exit_code`, `duration` | A command ended (shell integration) |
| `block_state_changed` | `pane`, `block`, `state` | A prompt, command input, run or result began |
| `bell` | `pane` | The program rang the bell |
| `config_changed` | none | The config was reloaded |
| `theme_changed` | none | The configured theme colors changed after a reload |

`exit_code` can be nil, for example when a nested shell ends a block without
reporting a status. Nil means unknown, not failure, so test
`ev.exit_code ~= nil and ev.exit_code ~= 0`. `duration` is in seconds.
`block` is a block id for `pane:block_text`, and `state` is one of `prompt`,
`input`, `running` or `finished`.

Several `title_changed`, `cwd_changed` or `bell` events for the same pane that
arrive before hooks run are merged into the latest one.

## Panes

| Function | Returns |
|---|---|
| `p.panes.list()` | All panes, in id order |
| `p.panes.focused()` | The focused pane of the keyboard window, or nil |
| `p.panes.get(id)` | The pane with that id, or nil |
| `p.panes.new_tab(opts)` | A new pane in a new tab |
| `p.panes.float(opts)` | A new floating pane |

`opts` may contain `cmd` (a shell command line string, or a list of program and
arguments) and `cwd` (`~` and `~/...` are expanded). Without `cmd` the shell
starts; without `cwd` the new pane uses the focused pane's directory.

A pane object has these read-only fields. Each read asks the terminal for the
current value; reading any field of a closed pane gives nil.

| Field | Meaning |
|---|---|
| `id` | Number |
| `title` | What the tab bar shows |
| `name` | Name set by the user, or nil |
| `cwd` | Working directory, or nil |
| `remote` | Host of the working directory when it is another machine |
| `process` | Foreground program (the shell when idle) |
| `command` | Command line of the running command (shell integration) |
| `window` | Window number |
| `tab` | Tab index in its window, starting at 1 |
| `focused` | Focused pane of the active tab of its window |
| `floating` | Whether it floats |
| `cols`, `rows` | Size in cells |

Methods use a colon: `pane:focus()`, not `pane.focus()`.

| Method | What it does |
|---|---|
| `pane:info()` | All fields as a plain table, or nil if the pane is gone |
| `pane:exists()` | Whether the pane is still open |
| `pane:split { dir = "right" or "down", cmd = ..., cwd = ... }` | Split it; returns the new pane, which gets the focus. Without `dir`, splits along the longer side |
| `pane:send_text(text)` | Type text as if typed; `"\r"` is Enter |
| `pane:insert_text(text)` | Paste text (bracketed paste if the program enabled it) |
| `pane:close()` | Close it, asking first if `panes.confirm_close` applies |
| `pane:focus()` | Show and focus it |
| `pane:set_name(name)` | Set its name; nil or `""` goes back to the automatic title |
| `pane:read_screen { scrollback = n }` | Screen text with up to `n` lines of scrollback above it |
| `pane:read_blocks()` | Command blocks, oldest first |
| `pane:block_text(id, part)` | Text of a block; `part` is `"command"`, `"output"` (default) or `"all"` |

Each block from `read_blocks` has `id`, `command`, `cwd`, `exit_code`,
`duration` and `state`. Blocks need shell integration; see
[blocks.md](blocks.md).

Pane objects compare equal when their ids are equal:
`p.panes.focused() == ev.pane`.

## Run an action

`p.run_action(name)` runs a built-in action on the focused pane, as a key bound
to it would. Plugin actions use the form `plugin:PLUGIN-ID:ACTION-ID`.

```lua
p.palette.add("Zoom pane", function() p.run_action("pane.zoom") end)
```

## Show a message

`p.notify(text, opts)` shows a message in the top right corner of the window.
`opts.level` is `"info"` (default) or `"error"`. `opts.timeout` is the time in
seconds it stays (at most 3600). Without a timeout, info messages stay
4 seconds and errors 8 seconds. Error messages are also written to the
[log file](getting-started.md#logs) and standard error.

## Status bar segments

`p.status.segment(side, f)` adds text to the tab bar. `side` is `"left"` (after
the tabs) or `"right"` (at the right end). `f(ctx)` returns the text; nil or
`""` hides the segment. Only the first line is shown. Segments run after events
and once a second, so keep them fast.

```lua
p.status.segment("left", function(ctx)
  return ctx.pane and ctx.pane.cwd
end)
```

## Command palette entries

`p.palette.add(name, f)` adds an entry to the command palette (Ctrl+Shift+P by
default). Adding the same name again replaces the entry. `f(ctx)` gets the
focused pane in `ctx.pane`.

## Timers

`p.after(secs, f)` runs `f` once after `secs` seconds. `p.every(secs, f)` runs
it every `secs` seconds. Both return a timer with a `:stop()` method. The
shortest interval is 0.01 seconds.

```lua
local ticks = 0
local timer
timer = p.every(60, function()
  ticks = ticks + 1
  if ticks == 10 then timer:stop() end
end)
```

## UI surfaces

`p.ui.open(opts)` opens a panel that your Lua code draws and that receives keys
and mouse events. It can only be called from a callback, and the surface closes
when the config reloads.

| Option | Meaning |
|---|---|
| `kind` | `"float"` (default), `"sidebar_left"`, `"sidebar_right"`, `"overlay"`, `"pane_right"`, `"pane_down"` or `"tab"` |
| `title` | Title text |
| `cols` | Width of sidebars, floats and overlays (default 40, at most 500) |
| `rows` | Height of floats and overlays (0 means 10) |
| `focus` | Take the keyboard (default `true`) |
| `pane` | A pane or pane id to split or anchor at instead of the focused pane |
| `on_event` | `function(ev, surface)` called for every event |

The surface has these methods:

| Method | What it does |
|---|---|
| `surface:draw(lines, opts)` | Show `lines`, rows from the top |
| `surface:close()` | Close it |
| `surface:focus()` | Give it the keyboard |
| `surface:size()` | Columns and rows, or nil once it is gone |

Draw on the first `resize` event; that is when the size is known.

A line is a string, a span, or a list of strings and spans. A span is a table
with the text as its first element (or `text`) and optional `fg`, `bg` (a
palette index 0 to 255 or `"#rrggbb"`), `link` (opened with Ctrl+click), and
the flags `bold`, `dim`, `italic`, `underline`, `reverse` and `strike`. The
`opts` of `draw` can set `cursor = { col, row }` (1-based) and `background`.

`ev.type` is one of these:

| Type | Fields |
|---|---|
| `key` | `key` (`a`, `enter`, `escape`, `up`, `page-down`, `f5`, ...), `text`, `mods`, `repeat` |
| `text` | `text` |
| `preedit` | `text` (nil when composition ends), `cursor` |
| `mouse` | `button` (`left`, `middle`, `right`, `wheel_up`, `wheel_down`), `action` (`press`, `release`, `drag`), `col`, `row` (1-based), `mods` |
| `resize` | `cols`, `rows` |
| `focus` | `focused` |
| `close` | none |
| `drop` | `paths` |
| `accessibility` | `node`, `action`, `value` |

`mods` is a table with the booleans `shift`, `ctrl`, `alt` and `logo`.

This example from `examples/config/extensions/panel.lua` opens a float with
Alt+i and closes it with Escape:

```lua
local p = require("proteus")
p.keymap("normal", "alt+i", function()
  p.ui.open {
    title = "Config panel", cols = 40, rows = 4,
    on_event = function(ev, surface)
      if ev.type == "resize" then
        surface:draw { "This UI is ordinary Lua config.", "Press Escape to close." }
      elseif ev.type == "key" and ev.key == "escape" then
        surface:close()
      end
    end,
  }
end)
```

A styled line:

```lua
surface:draw {
  { { "ok", fg = 2, bold = true }, " build finished" },
  { { "docs", link = "https://example.com", underline = true } },
}
```

## Translations

`p.i18n.locale()`, `p.i18n.load(locale, source)` and `p.i18n.t(id, args)` let
config code use Fluent messages. See [i18n.md](i18n.md).

## Other fields

`p.version` is the Proteus version string.

## Limits and errors

- An error in a callback is shown in the window as a one-line message; the
  terminal keeps running. The full error, with the Lua traceback, the
  function's `FILE:LINE` and what ran it (`event cwd_changed`, `timer 3`,
  `action`, `status segment`, `ui surface 2`), goes to standard error and the
  [log file](getting-started.md#logs). An error while the config loads (an
  unknown option, an unknown event name, a bad key) fails the whole config;
  its traceback is logged with the config path.
- A callback should take under 5 ms. A slower one gets a single warning,
  "slow Lua callback (FILE:LINE)", which is also logged with its duration.
  Time spent inside terminal calls, such as starting a shell for a new pane,
  is not counted.
- A callback that runs longer than 2 seconds is stopped with "took too long and
  was stopped". `init.lua` itself may run for 5 seconds.
- The time limit is checked between Lua instructions. A long call into a
  library function, such as `os.execute("sleep 60")` or a blocking
  `io.read`, is not interrupted and freezes the terminal until it returns.
- The Lua state may use 512 MiB of memory.
- If hooks keep causing events that run more hooks, Proteus stops after 1000
  calls in one pass and drops the remaining events.

## Trust

The config is trusted like your shell startup files. It runs with every Lua
standard library except `debug`, including `io`, `os` and `package`, and is not
sandboxed. Code from other people that you do not want to run with your full
access belongs in a WASM plugin, which only gets the permissions you approve;
see [plugins.md](plugins.md).
