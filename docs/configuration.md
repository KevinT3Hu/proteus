# Configuration

Proteus is configured with a Lua 5.4 file. You need no file at all; every
setting has a default. This page explains how the file is found and loaded,
how reloading works, and lists every option of `p.config`.

Key bindings are described in [keys.md](keys.md), Lua scripting (hooks, status
segments, palette entries, `p.keymap`) in [scripting.md](scripting.md), and WASM
plugins in [plugins.md](plugins.md).

## The config file

Proteus loads `$XDG_CONFIG_HOME/proteus/init.lua`, or
`~/.config/proteus/init.lua` when `XDG_CONFIG_HOME` is unset. Use
`proteus -c FILE` for another file, or `--no-config` for the defaults. A missing
default file is not an error; a missing `-c` file is.

A minimal file:

```lua
local p = require("proteus")
p.config {
  font = { family = "JetBrains Mono", size = 12 },
  scrollback = 20000,
}
```

Rules that apply to the whole file:

- `p.config` can be called several times. Later calls override earlier ones,
  key by key, so `p.config { font = { size = 14 } }` keeps a `font.family`
  set earlier. Key bindings accumulate.
- `p.config`, `p.keymap` and `p.load_plugin` only work while the file is
  loading, not later from hooks or timers.
- Unknown keys and values of the wrong type are errors, so a typo such as
  `font = { famly = "x" }` is reported instead of ignored.
- The file is your own code and runs with the full Lua standard library
  (`io`, `os`, `package`), like a shell rc file. Code from other people belongs
  in a WASM plugin, which only gets the permissions you grant.

### Split the config into modules

`require` looks for modules in the config file's directory: `require("theme")`
loads `theme.lua` or `theme/init.lua` next to `init.lua`, and
`require("extensions.clock")` loads `extensions/clock.lua`. Paths in
`p.load_plugin { source = ... }` are also relative to that directory.

```lua
-- ~/.config/proteus/font.lua
return { family = "Fira Code", size = 13 }

-- ~/.config/proteus/init.lua
require("proteus").config { font = require("font") }
```

`examples/config/` in the source tree is a complete example with modules.

### Check the config and get editor completion

```sh
proteus --check-config              # prints "Config OK" or the error
proteus --emit-lua-types ~/.config/proteus/types
```

The second command writes `proteus.lua`, type annotations for
lua-language-server. Add that directory to your editor's Lua library path to get
completion and type checking for `require("proteus")`.

### Edit the config from Proteus

The `config.open` action ("Open config in editor" in the command palette,
`Ctrl+Shift+P`) opens the config file that Proteus loaded, including a `-c`
file, in a new split pane with `$VISUAL`, then `$EDITOR`, then `vi`. It is not
bound to a key by default.

## Reloading

Proteus watches the config file's directory and its subdirectories (except
`layouts/`). When a `.lua` file there is written, created, moved or deleted, the
config is loaded again and "config reloaded" is shown briefly. If the new config
has an error, the previous config stays in use and the error is shown.

After a successful reload:

- Theme, fonts, padding, key bindings, decorations, opacity, blur, bidi mode and
  hint patterns change immediately in all windows.
- Terminal settings (scrollback size, clipboard policy, image limits) apply to
  panes opened from then on.
- `window.width`, `height`, `columns` and `rows` only set the size of new
  windows. `graphics.total_memory`, `session.restore` and `ipc.socket` are read
  only at startup.
- `--font` and `--font-size` from the command line keep overriding the file.
- Lua state from the previous load is discarded, and the `config_changed`
  event runs (see [scripting.md](scripting.md)).

If the config has an error at startup, Proteus starts with the defaults and
shows the error.

## Option reference

In the tables, "false" means you can write `false` to reset an optional value
to its automatic default. Sizes in logical pixels are scaled by the output's
scale factor.

### language

| Option | Type | Default | Effect |
| --- | --- | --- | --- |
| `language` | string | from `LC_ALL`, `LC_MESSAGES`, `LANG` | UI language, for example `"zh-CN"`. See [i18n.md](i18n.md). |

### font

```lua
p.config { font = { family = "JetBrains Mono", size = 12, fallback_families = { "Noto Color Emoji" } } }
```

| Option | Type | Default | Effect |
| --- | --- | --- | --- |
| `family` | string | `"monospace"` | Font family or fontconfig pattern |
| `size` | number, 4 to 96 | `11` | Size in points. `Ctrl+=`, `Ctrl+-` and `Ctrl+0` change it at runtime. |
| `ligatures` | boolean | `true` | Use the font's programming ligatures |
| `fallback_families` | list of strings, at most 32 | `{}` | Families tried in order for characters the main font lacks |
| `bold_family` | string or false | unset | Family for bold text. Unset: the bold style of `family`, synthesized if the family has none. |
| `italic_family` | string or false | unset | Family for italic text, as above |
| `bold_italic_family` | string or false | unset | Family for bold italic text, as above |
| `cell_width` | number 1 to 512, or false | unset | Cell width in logical pixels instead of the font's advance |
| `cell_height` | number 1 to 512, or false | unset | Cell height in logical pixels instead of the font's line height |
| `letter_spacing` | number, -64 to 256 | `0` | Logical pixels added to the cell width |
| `line_spacing` | number, -64 to 256 | `0` | Logical pixels added to the cell height |
| `offset_x` | number, -256 to 256 | `0` | Shift glyphs horizontally inside the cell, logical pixels |
| `offset_y` | number, -256 to 256 | `0` | Shift glyphs vertically inside the cell, logical pixels |

### window

| Option | Type | Default | Effect |
| --- | --- | --- | --- |
| `columns` | integer 2 to 8192, or false | 80 | Initial width in cells |
| `rows` | integer 1 to 8192, or false | 24 | Initial height in cells |
| `width` | integer 100 to 8192, or false | unset | Initial width in logical pixels; overrides `columns` |
| `height` | integer 60 to 8192, or false | unset | Initial height in logical pixels; overrides `rows` |
| `opacity` | number, 0 to 1 | `1` | Opacity of the default background. Cells with their own background color and the bars stay opaque. |
| `background_blur` | boolean | `false` | Ask the compositor to blur what shows through (needs the `ext-background-effect-v1` protocol) |
| `decorations` | `"auto"`, `"client"`, `"none"` | `"auto"` | `auto`: ask the compositor for a title bar and border, and draw Proteus's own if the compositor leaves it to the client. `client`: always draw Proteus's title bar with minimize, maximize and close buttons. `none`: no decorations. |

The compositor has the final say over the window size.

### ui

| Option | Type | Default | Effect |
| --- | --- | --- | --- |
| `padding` | number 0 to 256, or `{ horizontal = n, vertical = n }` | `4` | Space around the window content, logical pixels |
| `pane_gap` | number 0 to 256, or false | one cell width | Space between tiled panes, logical pixels |
| `border_width` | number, 0 to 32 | `1` | Width of pane borders, logical pixels |
| `tab_bar` | `"auto"`, `"always"`, `"never"` | `"auto"` | `auto` shows the tab bar when there is more than one tab |
| `hint_bar` | `"in-mode"`, `"always"`, `"never"` | `"in-mode"` | `in-mode` shows the key hints over the bottom row while a mode other than Normal is active; `always` reserves a bottom row for it |
| `pane_titles` | `"auto"`, `"always"`, `"never"` | `"never"` | Title rows above tiled panes. `auto`: when the tab has more than one tiled pane. Stacked and floating panes always have a title. |
| `gutter` | number, clamped to 0 to 2 | `0.5` | Width of the command-block gutter left of each pane, in cells; `0` hides it. See [blocks.md](blocks.md). |
| `sticky_command` | boolean | `true` | Pin the command of the block whose output is at the top of the pane |

### theme

Colors are strings: `"#rgb"`, `"#rrggbb"`, `"#rrrgggbbb"`, `"#rrrrggggbbbb"`
or `"rgb:r/g/b"`.

| Option | Type | Default | Effect |
| --- | --- | --- | --- |
| `preset` | `"default"`, `"dark"`, `"light"` | `"default"` | Start from a built-in theme. `default` and `dark` are the same. A preset resets the whole theme first, then the other keys in the same table apply on top. |
| `foreground` | color | `#d8d8d8` | Default text color |
| `background` | color | `#181818` | Default background color |
| `cursor` | color or false | unset | Cursor color |
| `ansi` | list of exactly 16 colors | built-in | Palette colors 0 to 15 (black, red, green, yellow, blue, magenta, cyan, white, then the bright versions) |
| `indexed` | table of index (0 to 255) = color or false | `{}` | Override single palette entries. Applied after `ansi`. `false` removes an override. Colors 16 to 255 default to the xterm color cube and gray ramp. |
| `ui` | table | see below | Colors of Proteus's own interface |

`theme.ui` keys, all colors or `false`. Unset ones follow the focused pane's palette:

| Key | Used for | When unset |
| --- | --- | --- |
| `foreground`, `background` | Bars and interface text | The pane's foreground and background |
| `accent` | Active tab, mode badge, hint labels, copy-mode cursor | Palette color 4 (blue) |
| `bar` | Bar background | A 10% mix of foreground into background |
| `dim` | Inactive tabs and secondary text | A 35% mix of foreground into background |
| `selection_foreground`, `selection_background` | Selected text | Selected text is shown in reverse video |
| `search_foreground`, `search_background` | Search matches | Palette color 0 on palette color 3 |
| `search_current_background` | The current search match | Palette color 208 |
| `cursor_text` | Text under a block cursor | The background color |
| `bell` | Visual bell flash | The foreground color |

Programs can still change the palette with escape sequences; the theme is the
base they start from.

#### Define a theme

Put the theme in its own module and select it from `init.lua`:

```lua
-- ~/.config/proteus/mytheme.lua
return {
  foreground = "#cdd6f4", background = "#1e1e2e", cursor = "#f5e0dc",
  ansi = {
    "#45475a", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#bac2de",
    "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#a6adc8",
  },
  indexed = { [208] = "#fab387" },
  ui = { accent = "#89b4fa", bar = "#313244", dim = "#6c7086", selection_background = "#45475a" },
}

-- ~/.config/proteus/init.lua
require("proteus").config { theme = require("mytheme") }
```

To use the light preset with one change:

```lua
require("proteus").config { theme = { preset = "light", ui = { accent = "#2050a0" } } }
```

`examples/appearance.lua` in the source tree sets every theme, font, window
and ui option explicitly; copy it as a starting point.

### cursor

| Option | Type | Default | Effect |
| --- | --- | --- | --- |
| `shape` | `"app"`, `"block"`, `"bar"`, `"underline"` | `"app"` | `app` lets the program choose the shape; the others force one |
| `blink` | `true`, `false`, `"app"` | `"app"` | `app` blinks when the program asks for it; `true` always blinks; `false` never |

### text

| Option | Type | Default | Effect |
| --- | --- | --- | --- |
| `bidi` | `"auto"`, `"on"`, `"off"` | `"auto"` | Bidirectional text reordering. `auto` reorders the normal screen but leaves full-screen programs on the alternate screen alone. |
| `grapheme_clustering` | boolean | `false` | Turn on grapheme clustering (mode 2027) without the program asking for it |
| `ambiguous_width` | `1` or `2` | `1` | Width of East Asian ambiguous-width characters |

### scrollback

| Option | Type | Default | Effect |
| --- | --- | --- | --- |
| `scrollback` | number | `10000` | Lines of history kept per pane. Negative values count as 0. See [scrollback.md](scrollback.md). |

### clipboard

| Option | Type | Default | Effect |
| --- | --- | --- | --- |
| `osc52_write` | boolean | `true` | Let programs set the clipboard with OSC 52 (this works over SSH) |
| `osc52_read` | `"ask"`, `"allow"`, `"deny"` | `"ask"` | Programs reading the clipboard with OSC 52. `ask` shows a prompt: `y` allows once, `Shift+A` always for that session, `n` denies. |
| `paste_protection` | boolean | `true` | When the program has not enabled bracketed paste, ask before pasting text that contains newlines or other control characters (tab excepted), since each newline may run a command |

### graphics

Inline images from the kitty, Sixel and iTerm2 protocols. Sizes are in MiB,
at least 1. See [images.md](images.md).

| Option | Type | Default | Effect |
| --- | --- | --- | --- |
| `enabled` | boolean | `true` | Accept image protocols at all |
| `max_image` | number (MiB) | `64` | Largest single decoded image, all frames together |
| `pane_memory` | number (MiB) | `256` | Decoded image memory per pane |
| `total_memory` | number (MiB) | `1024` | Decoded image memory of all panes together. Read at startup. |
| `gpu_memory` | number (MiB) | `512` | GPU texture memory per window. Beyond it, the least recently drawn images are dropped and uploaded again when needed. |
| `local_files` | boolean | `true` | Let local programs send images as file paths or shared memory (kitty transmission types `f`, `t`, `s`) |

### notify

| Option | Type | Default | Effect |
| --- | --- | --- | --- |
| `min_duration` | seconds, or false | `10` | Send a desktop notification when a command that ran at least this long finishes in a pane you are not looking at. `false` turns it off. Needs [shell integration](getting-started.md#shell-integration). |
| `osc` | boolean | `true` | Pass OSC 9 and OSC 777 notifications from programs to the desktop |

A pane counts as watched when its window has keyboard focus, its tab is active
and it is the focused pane.

### bell

| Option | Type | Default | Effect |
| --- | --- | --- | --- |
| `visual` | boolean | `true` | Flash the pane on the bell. Independently of this, a bell in a pane you are not watching marks its tab and title with `•`. |

### hints

Hint mode labels URLs, paths and other patterns on screen so you can open,
copy or insert them by typing the label. See [keys.md](keys.md) for the
`hints.*` actions.

| Option | Type | Default | Effect |
| --- | --- | --- | --- |
| `alphabet` | string | `"asdfghjkl"` | Characters used for labels; at least 2 distinct lowercase letters or digits |
| `patterns` | list of `{ name, regex, url }` | `{}` | Extra patterns, tried before the built-in ones. `regex` is required (Rust regex syntax). `url`, if set, is opened with `{}` replaced by the matched text. `name` defaults to `patternN`. |

```lua
p.config { hints = { patterns = {
  { name = "jira", regex = "\\b[A-Z]+-\\d+\\b", url = "https://jira.example/browse/{}" },
} } }
```

### panes

| Option | Type | Default | Effect |
| --- | --- | --- | --- |
| `confirm_close` | boolean | `true` | Ask before closing a pane whose foreground process is not the shell |
| `confirm_close_ignore` | list of strings | `{ "less", "more", "man", "top", "htop", "btop", "watch", "tail" }` | Foreground programs that never need the confirmation |
| `keep_last_shell` | boolean | `false` | Closing the last pane of a window starts a new shell instead of closing the window |

### session

Proteus saves the open windows, tabs and panes to
`$XDG_STATE_HOME/proteus/session.lua` (default `~/.local/state/proteus/`).

| Option | Type | Default | Effect |
| --- | --- | --- | --- |
| `restore` | `"ask"`, `"always"`, `"never"` | `"ask"` | `ask` offers to restore after Proteus did not exit normally (crash, logout, killed). `always` restores the last session at every start. `never` neither restores nor saves. |
| `restore_commands` | list of strings | `{ "nvim", "vim", "hx", "less", "htop", "btop", "top", "man" }` | Foreground programs that are started again in restored panes. Other panes get a shell in the saved directory. |

A command or `--layout` on the command line skips the restore. See
[panes-and-layouts.md](panes-and-layouts.md) for saved layouts.

### shell

| Option | Type | Default | Effect |
| --- | --- | --- | --- |
| `command` | string, or list of strings | `$SHELL` | Program for new panes. A string is one program name with no arguments; use a list for arguments: `{ "fish", "-l" }`. |
| `integration` | boolean | `true` | Inject shell integration into bash, zsh and fish. See [getting-started.md](getting-started.md#shell-integration). |
| `term` | string | automatic | Value of `TERM` for programs. Automatic is `proteus` when its terminfo entry could be installed, otherwise `xterm-256color`. Letters, digits and `-+_.` only. |

```lua
p.config { shell = { command = { "zsh", "-l" }, term = "xterm-256color" } }
```

### ipc

| Option | Type | Default | Effect |
| --- | --- | --- | --- |
| `socket` | string | unset (off) | Path of the control socket, created at startup. `--listen` overrides it. See [getting-started.md](getting-started.md#control-socket). |

### keys

`keys` selects a preset (`"zellij"`, `"leader"`, `"tmux"`), the `leader` chord,
`sticky` modes, and per-mode bindings such as
`normal = { ["alt+x"] = "pane.close" }`. See [keys.md](keys.md).

```lua
p.config { keys = { preset = "tmux", normal = { ["alt+t"] = "none" } } }
```

### Plugins

WASM plugins are not part of `p.config`; writing `plugins = ...` is an error.
Load each one with `p.load_plugin`:

```lua
p.load_plugin { source = "/usr/share/proteus/plugins/files", config = { position = "left", width = 32 } }
```

See [plugins.md](plugins.md) for sources, pinning and permissions, and
[files.md](files.md) for the Files plugin's settings.

## A complete example

```lua
local p = require("proteus")

p.config {
  font = { family = "JetBrains Mono", size = 12, line_spacing = 2 },
  window = { columns = 120, rows = 36, opacity = 0.95 },
  theme = { preset = "light" },
  cursor = { shape = "bar", blink = false },
  scrollback = 50000,
  ui = { tab_bar = "always", pane_titles = "auto", padding = { horizontal = 10, vertical = 6 } },
  clipboard = { osc52_read = "deny" },
  notify = { min_duration = 30 },
  panes = { keep_last_shell = true },
  session = { restore = "always" },
  shell = { command = { "fish" } },
}

p.status.segment("right", function() return os.date("%H:%M") end)
```

Run `proteus --check-config` after editing to see errors before reloading.
