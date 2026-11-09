# Panes, tabs, windows and layouts

A Proteus window holds tabs, and each tab holds panes. Tiled panes split the
tab between them; floating panes sit on top. A layout file describes tabs and
their panes so you can open the same arrangement again, and Proteus saves your
session so it can bring it back after a crash.

The keys below are those of the default `zellij` preset. Other presets and
rebinding are covered in [keys.md](keys.md). Every action can also be run from
the palette (Ctrl+Shift+P).

## Split a pane

| Keys | Action | Result |
|------|--------|--------|
| Alt+n, or Ctrl+p `n` | `pane.new` | New pane along the longer side of the focused one |
| Ctrl+p `r` or `v` | `pane.split_right` | New pane to the right |
| Ctrl+p `d` or `s` | `pane.split_down` | New pane below |

The new pane gets the focus and starts your shell (`shell.command`, else
`$SHELL`) in the working directory of the focused pane. Proteus takes that
directory from the shell's OSC 7 report when it is on this machine, else from
the foreground program. Panes on another host (SSH) give no directory, so the
new pane starts in the default one.

"Longer side" counts a cell as about twice as tall as it is wide: a pane at
least twice as many columns as rows splits to the right, otherwise down. A
pane that is too small to split stays as it is.

## Move the focus

| Keys | Action |
|------|--------|
| Alt+h, Alt+j, Alt+k, Alt+l, or Alt+arrows | Focus the pane in that direction |
| Ctrl+p then `h` `j` `k` `l` or arrows | Same, from Pane mode |
| Click a pane | Focus it |

In Normal mode, Alt+h and Alt+l go on to the previous or next tab when there
is no pane further in that direction (`pane.focus_or_tab_left`,
`pane.focus_or_tab_right`). They do not wrap from the last tab to the first.

`pane.focus_next` cycles through the panes of the tab (tiled first, then
visible floating panes), and `pane.focus_next_global` /
`pane.focus_prev_global` cycle through all panes of all tabs in the window.
They have no default key in the zellij preset; `ctrl+b o` in the tmux preset
runs `pane.focus_next`.

When you close a pane, the focus goes back to the pane you used before it.

## Resize panes

Ctrl+n enters Resize mode. It stays active until Esc, so you can press keys
repeatedly.

| Key | Action |
|-----|--------|
| `h` `j` `k` `l`, arrows | Grow the focused pane on that side by 2 cells |
| `H` `J` `K` `L`, Shift+arrows | Shrink it on that side by 2 cells |
| `=` | `pane.equalize`: equal sizes for all panes |

With the mouse, drag the line between two panes. Programs in the panes are
told the new size when you release the button.

## Swap panes

Ctrl+h enters Move mode. `h` `j` `k` `l` or the arrows swap the focused pane
with its neighbor in that direction (`pane.move_left` and so on). `t` or `b`
moves the pane into a new tab of its own.

## Zoom and stack

**Zoom** (Ctrl+p `f` or `z`, `pane.zoom`) shows the focused tiled pane alone
in the tab. Press it again to go back. Focusing another tiled pane or
splitting also ends the zoom. A tab with a single tiled pane, or a focused
floating pane, cannot be zoomed. The tab bar shows a zoomed tab as
`title [3, zoom]`.

**Stack** (Ctrl+p `S`, `pane.toggle_stack`) puts the focused pane and its
sibling (everything on the other side of the split it belongs to) into one
area. The active pane is shown in full and the others shrink to one-row title
bars above and below it. Move between them with the normal focus keys, or
click a title bar. Press Ctrl+p `S` on a stacked pane to turn the stack back
into panes split top to bottom.

## Pane titles

Title rows above tiled panes are off by default. Ctrl+p `T`
(`ui.toggle_pane_titles`) shows or hides them for the session, and
`ui.pane_titles` sets the default: `"never"`, `"always"`, or `"auto"` (only
when the tab has more than one tiled pane). Stacked and floating panes always
have a title row. The focused pane's title shows the active mode, such as
`[RESIZE]`.

## Floating panes

Floating panes are drawn over the tiled ones. Each tab has its own.

| Keys | Action | Result |
|------|--------|--------|
| Alt+f, or Ctrl+p `w` | `pane.toggle_floating` | Show or hide the tab's floating panes. With none, it creates one. |
| Ctrl+p `e` | `pane.toggle_embed` | Turn the focused tiled pane into a floating one, or put the focused floating pane back into the tiled layout |
| (no default) | `pane.new_floating` | Another floating pane |

A new floating pane is centered and takes 70% of the tab; further ones are
offset a little. While a floating pane has the focus, `pane.new` (Alt+n) also
opens another floating pane, the focus keys move between floating panes, Resize
mode changes its size, and Move mode moves it.

With the mouse, drag a floating pane's title row or border to move it, and
drag its bottom-right corner to resize it. Hiding the floating panes returns
the focus to the tiled pane you used last.

The only tiled pane of a tab cannot be floated.

## Tabs

| Keys | Action |
|------|--------|
| Alt+t, or Ctrl+t `n` | `tab.new`: new tab after the current one, in the focused pane's directory |
| Alt+1 ... Alt+9, or Ctrl+t `1` ... `9` | Go to tab 1 to 9 |
| Alt+[ and Alt+], or Ctrl+t `h`/`l` (also `k`/`j` and arrows) | Previous or next tab, wrapping |
| Ctrl+t `r` | `tab.rename` |
| Ctrl+t `x` | `tab.close`: close the tab and all its panes |
| Ctrl+t `b` | `pane.to_new_tab`: move the focused pane into a new tab |
| (no default) | `tab.move_left`, `tab.move_right`: move the current tab one place, wrapping |

`tab.goto_N` works for any N up to 99 if you bind it; a number past the last
tab does nothing.

The tab bar appears when a window has more than one tab (`ui.tab_bar`:
`"auto"`, `"always"` or `"never"`). A tab shows its name, or else the title of
its focused pane, followed by the number of panes, such as `build [2]`. A dot
in front means a bell rang in one of its panes.

With the mouse:

- Click a tab to switch to it. Scroll the wheel over the tab bar to step
  through tabs.
- Middle-click a tab to close it.
- Drag a tab along the bar to reorder it.
- Drag a tab away from the bar (more than two rows below it, or out of the
  window) to move it to another window. Drop it on another Proteus window's
  tab bar to place it at that position, anywhere else in that window to add
  it at the end, or outside all Proteus windows to give it a new window.

## Name panes and tabs

Ctrl+p `c` (`pane.rename`) and Ctrl+t `r` (`tab.rename`) open a prompt with
the current name. Enter an empty name to go back to the automatic title (the
title the program sets). Names are kept in saved layouts and in the session.
From Lua, `pane:set_name(name)` does the same; see [scripting.md](scripting.md).

## Private panes

`pane.toggle_private` marks the focused pane as private, or makes it normal
again. It has no default key; bind one or use the palette:

```lua
require("proteus").keymap("normal", "alt+p", "pane.toggle_private")
```

For a private pane:

- WASM plugins receive no events about it, and when they ask for its details
  they only get its name, with no title, directory, program or command.
  Requests to read its screen or its command blocks fail with "pane is
  private". Plugins can still send text to it.
- `proteus +msg read` refuses to read it.
- Accessibility clients such as screen readers do not get its output.
- Its title row shows `[private]`.

Your own Lua config is not restricted. Privacy is not saved in layouts or the
session; a restored pane starts as normal.

## Windows

| Keys | Action |
|------|--------|
| Ctrl+Shift+N | `window.new`: a new window with a shell in the focused pane's directory |
| Ctrl+p `W` | `pane.to_new_window`: move the focused pane into a new window |
| Ctrl+t `W` | `tab.to_new_window`: move the current tab into a new window |
| Ctrl+t `w` | `tab.to_next_window`: move the current tab into the next window |

All windows belong to one Proteus process. The palette lists the panes of
every window, and picking one brings it into view. A window whose last tab
moves away closes.

To move a pane into an existing tab, there is no action; move the tab instead,
or rebuild the arrangement with a layout.

## Close panes, tabs and windows

A pane closes when its program exits, or with Ctrl+p `x` (`pane.close`).
Ctrl+t `x` or a middle click closes a tab. The `window.close` action (no
default key) and the window's close button close the window.

If a program other than the shell is running in a pane you are closing,
Proteus asks first, naming the programs: `Close this pane? Running: cargo`.
Press `y` or Enter to close, `n` or Esc to cancel.

| Option | Default | Effect |
|--------|---------|--------|
| `panes.confirm_close` | `true` | `false` closes without asking |
| `panes.confirm_close_ignore` | `{ "less", "more", "man", "top", "htop", "btop", "watch", "tail" }` | Programs that never need the question |
| `panes.keep_last_shell` | `false` | `true`: closing the last pane of a window starts a new shell instead of closing the window |

Closing the last pane of the last window ends Proteus. Closing the only window
(close button or `window.close`) asks about running programs in all its panes
and then quits. With several windows open, it closes just that window.

## Session restore

Proteus saves the windows, tabs and panes of the session every 2 seconds to
`$XDG_STATE_HOME/proteus/session.lua` (default
`~/.local/state/proteus/session.lua`), in the layout format described below.
Only the first running Proteus process saves; later ones run without a
session.

What is saved: windows, tabs and their names, splits and their sizes, stacks,
floating panes with their position and size, zoom, the focused tab and pane,
pane names, working directories, and programs from `session.restore_commands`
that were running in the foreground (with their arguments). Scrollback is not
saved, and neither are directories of panes on other hosts.

How the session ends decides what happens next time:

- You close the window: the session is kept and marked as a normal exit.
- The last pane exits (for example you type `exit` in the last shell): the
  session file is removed.
- Proteus crashes, is killed, or you log out: the session is left as it was.

`session.restore` controls startup:

| Value | Effect |
|-------|--------|
| `"ask"` (default) | After an exit that was not normal, ask: `Restore the previous session? (2 windows, 3 tabs, 5 panes; it did not exit normally)`. The question also lists the commands it would run. Press `y` or Enter to restore, `n` or Esc to start fresh. Keys pressed in the first half second are ignored, so typing does not answer it by accident. |
| `"always"` | Restore the last saved session at every start, without asking |
| `"never"` | Do not save or restore |

```lua
require("proteus").config {
  session = {
    restore = "ask",
    restore_commands = { "nvim", "vim", "hx", "less", "htop", "btop", "top", "man" },
  },
}
```

The list above is the default for `restore_commands`. Programs are matched by
process name, and a restored pane types the saved command line into a fresh
shell, so the pane stays open when the program ends.

Starting Proteus with a command (`proteus -- htop`) or a layout (`proteus -l
dev`) skips the restore.

If the session file cannot be parsed, Proteus keeps a copy next to it with
`.corrupt-` in its name and starts fresh.

## Saved layouts

A layout is a Lua file that returns tables of tabs and panes.

### Save the current layout

Ctrl+o `s` (`layout.save`) asks for a name and writes the current window to
`$XDG_CONFIG_HOME/proteus/layouts/NAME.lua` (default
`~/.config/proteus/layouts/NAME.lua`), replacing a file of the same name. The
directory is created if needed. A name that contains `/` or ends in `.lua` is
used as a path instead, relative to the directory Proteus was started in, with
`~` for your home directory.

The saved file contains the same things as the session (see above), for one
window. Paths under your home directory are written as `~/...`.

### Open a layout

- At startup: `proteus -l dev` (or `--layout dev`) opens
  `layouts/dev.lua` as the initial tabs. `proteus -l ./dev.lua` or an
  absolute path opens that file. If the file cannot be read or is invalid,
  Proteus prints the error and exits with status 2. A `COMMAND` argument is
  not used when a layout is given.
- In a running window: Ctrl+o `l` (`layout.load`) asks for a name and shows
  the saved names, or pick `layout: NAME` in the palette. The layout's tabs
  are added after the current tabs, and the layout's focused tab (or its first
  one) becomes active. Further windows in the layout open as new windows.

The layouts directory is always under `$XDG_CONFIG_HOME/proteus` (default
`~/.config/proteus`), also when you use `-c` to pick another config file.

### File format

```lua
return {
  tabs = {
    { name = "code", cwd = "~/src/app", focus = true,
      split = "vertical", ratio = 0.65,          -- side by side
      { cmd = "nvim" },
      { split = "horizontal",                    -- top and bottom
        { cmd = { "cargo", "watch", "-x", "test" } },
        { run = "git status" },                  -- typed into the shell
      },
    },
    { name = "logs", cmd = "journalctl -f",
      floating = { { run = "htop", x = 10, y = 3, width = 80, height = 20 } },
      show_floating = true,
    },
    { split = "horizontal", stacked = true, { run = "make" }, { run = "git log" } },
    { name = "files", plugin = "proteus.files", args = "~/src" },
  },
}
```

The file returns a table with `tabs`, a list of tab tables. For several
windows, use `windows` instead, a list of `{ tabs = { ... } }`. A file has
either `tabs` or `windows`, and each window needs at least one tab.

A tab table is also the tab's top node: either a single pane (pane keys) or a
split (split keys and child nodes in its list part).

**Tab keys**

| Key | Type | Meaning |
|-----|------|---------|
| `name` | string | Tab name. In a one-pane tab this is the tab's name, not the pane's. |
| `cwd` | string | Working directory for the tab's panes. `~` is your home; a relative path is relative to the directory Proteus runs in. |
| `focus` | boolean | Make this the active tab |
| `zoom` | boolean | Zoom the focused pane (only with more than one tiled pane) |
| `floating` | list | Floating panes, see below |
| `show_floating` | boolean | Show the floating panes. Default `false`: they exist but are hidden until Alt+f. |

**Pane keys**

| Key | Type | Meaning |
|-----|------|---------|
| `cmd` | string or list of strings | Run this instead of the shell. A string runs through `$SHELL -c` (`/bin/sh` if `$SHELL` is unset); a list is the program and its arguments. The pane closes when the program exits. If it cannot start, the pane gets a shell and a notice. |
| `run` | string | Type this line into the shell once it is ready, as if you typed it and pressed Enter. The pane stays open afterwards. Lines with control characters are refused with a notice. |
| `cwd` | string | Working directory. A relative path is relative to the tab's `cwd` (without one, to the directory Proteus runs in). Default: the tab's `cwd`. |
| `name` | string | Pane name |
| `focus` | boolean | Focus this pane in its tab |
| `plugin` | string | A plugin pane instead of a terminal: the plugin's ID, for example `"proteus.files"` |
| `args` | string | Text passed to the plugin pane; only with `plugin`. Its meaning belongs to the plugin (the file manager takes a directory; see [files.md](files.md)). |

`plugin` cannot be combined with `cmd` or `run`. If the plugin is not
running, the pane says so and the plugin takes it over when it starts. See
[plugins.md](plugins.md).

**Split keys**

| Key | Type | Meaning |
|-----|------|---------|
| `split` | string | `"vertical"` (also `"right"`, `"columns"`): children side by side. `"horizontal"` (also `"down"`, `"rows"`): children top to bottom. |
| `ratio` | number | Share of the first child, between 0 and 1 (exclusive). Default: equal shares. |
| `stacked` | boolean | Make the children a stack (see [Zoom and stack](#zoom-and-stack)). The pane with `focus = true` is expanded. |
| `[1]`, `[2]`, ... | tables | The children: two or more nodes. In a stack they must all be panes. |

A split cannot have pane keys. With more than two children, the first gets
`ratio` if given and the rest share the remaining space equally.

**Floating pane keys**

Each entry of `floating` is a pane table (pane keys, no split) with an
optional position in cells: `x`, `y` (column and row of its top-left corner in
the tab) and `width`, `height`. Give all four or none; without them the pane
gets the default centered placement. Floating panes inherit the tab's `cwd`.
To focus one, set `focus = true` on it and `show_floating = true` on the tab.

Unknown keys and wrong types are errors that name the place, such as
``/home/me/.config/proteus/layouts/dev.lua: tabs[1][2]: unknown key `comand` ``.

### What layout Lua can do

Layouts are data, so they run in a restricted Lua:

- Only Lua's basic functions and the `table`, `string` and `math` libraries
  are available. `io`, `os`, `require`, `debug`, `coroutine` and `utf8` are
  not.
- `string.find`, `string.match`, `string.gmatch` and `string.gsub` are
  removed (their pattern matching can run for a very long time).
- `string.rep` refuses results larger than 1 MiB.
- The file may be at most 1 MiB, may use at most 16 MiB of memory, and must
  finish within 100 ms.
- The returned value may be nested at most 32 levels deep and contain at most
  16384 values. Tables cannot have metatables, and the same table cannot
  appear twice. Build a fresh table for every pane instead of reusing one:

```lua
local function shell(dir) return { cwd = dir } end

return {
  tabs = {
    { name = "work", split = "vertical", shell("~/src/a"), shell("~/src/b") },
  },
}
```

Session restore reads the session file with the same rules.
