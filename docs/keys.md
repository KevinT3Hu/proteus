# Keys and modes

Proteus has built-in panes, tabs and windows, so some keys control Proteus
instead of the program in the pane. To keep those keys few, most commands live
in *modes*: you press one chord to enter a mode, then single letters. Every
command is a named *action*, and every key binding maps a key to an action in
one mode. You can rebind any key in your Lua config.

While a mode other than Normal is active, a hint bar at the bottom of the
window lists the keys of that mode, and the focused pane's title row shows the
mode name (for example `[PANE]`). The hint bar is set with `ui.hint_bar`
(`"in-mode"`, the default, `"always"` or `"never"`); see
[configuration.md](configuration.md).

## Modes

| Mode | What keys do | Ends |
|------|--------------|------|
| `normal` | Keys go to the program, except the keys bound in Normal mode. | |
| `locked` | Keys go to the program, except the unlock key and the basic terminal keys (copy, paste, font size, Shift+PageUp...). Use it for programs that need chords such as Ctrl+p or Ctrl+s. | With the unlock key |
| `menu` | A one-key menu opened by the leader key (`leader` and `tmux` presets). | After one key |
| `pane` | Split, close, zoom, float and rename panes. | After one action |
| `tab` | Create, switch, rename, close and move tabs. | After one action |
| `resize` | Grow and shrink the focused pane. | With Esc |
| `move` | Swap the focused pane with a neighbor, or move it to a new tab. | With Esc |
| `scroll` | Move a cursor through the scrollback, select, copy, search, act on command blocks. See [scrollback.md](scrollback.md). | With `q` or Esc |
| `session` | Save the current layout, load a saved one. See [panes-and-layouts.md](panes-and-layouts.md). | After one action |

In every mode except Normal and Locked, keys that are not bound in the mode
are ignored; they do not reach the program. Resize, move and scroll
modes stay active after each action so you can repeat it. To make the other
modes stay active too, set `keys.sticky = true`.

Holding a key down repeats movement actions (focus, resize, move, scroll,
font size, search steps), but not actions that create or close things.

## Presets and the leader key

`keys.preset` picks the default bindings. `keys.leader` replaces the preset's
leader chord.

| Preset | Leader | How modes are entered |
|--------|--------|-----------------------|
| `zellij` (default) | `ctrl+g` locks and unlocks | Ctrl+p, Ctrl+t, Ctrl+n, Ctrl+h, Ctrl+s, Ctrl+o enter the modes directly. Alt quick keys work in Normal mode. |
| `leader` | `ctrl+g` opens the mode menu | Leader, then `p`, `t`, `r`, `m`, `s`, `o` or `l`. Same Alt quick keys. No other chords are taken from the shell. |
| `tmux` | `ctrl+b` is the prefix | Prefix, then one tmux key (`c`, `%`, `"`, `n`, `p`, `o`, `x`, `z`, `[`...). No Alt quick keys. |

```lua
local p = require("proteus")
p.config {
  keys = { preset = "leader", leader = "ctrl+space" },
}
```

In the `zellij` preset, these chords are taken from the program in Normal mode:
Ctrl+p, Ctrl+t, Ctrl+n, Ctrl+h, Ctrl+s, Ctrl+o and Ctrl+g. If a program needs
one of them, lock first (Ctrl+g), or switch to the `leader` preset.

In the `leader` and `tmux` presets, pressing the leader twice sends the leader
chord itself to the program.

## Default bindings (zellij preset)

These tables match `proteus --no-config --list-keybindings`. Key names are
written the way the config spells them.

### Normal mode

| Key | Action | Does |
|-----|--------|------|
| `ctrl+g` | `mode.locked` | Lock |
| `ctrl+p` | `mode.pane` | Pane mode |
| `ctrl+t` | `mode.tab` | Tab mode |
| `ctrl+n` | `mode.resize` | Resize mode |
| `ctrl+h` | `mode.move` | Move mode |
| `ctrl+s` | `mode.scroll` | Scroll mode |
| `ctrl+o` | `mode.session` | Session mode |
| `alt+h`, `alt+left` | `pane.focus_or_tab_left` | Focus the pane to the left; at the window edge, the previous tab |
| `alt+j`, `alt+down` | `pane.focus_down` | Focus the pane below |
| `alt+k`, `alt+up` | `pane.focus_up` | Focus the pane above |
| `alt+l`, `alt+right` | `pane.focus_or_tab_right` | Focus the pane to the right; at the window edge, the next tab |
| `alt+n` | `pane.new` | New pane, split along the longer side |
| `alt+t` | `tab.new` | New tab |
| `alt+[`, `alt+]` | `tab.prev`, `tab.next` | Previous or next tab |
| `alt+1` ... `alt+9` | `tab.goto_1` ... `tab.goto_9` | Go to tab 1 to 9 |
| `alt+f` | `pane.toggle_floating` | Show or hide floating panes |
| `ctrl+shift+f` | `search.start` | Search the scrollback |
| `ctrl+shift+p` | `palette` | Action palette |
| `ctrl+shift+n` | `window.new` | New window |
| `ctrl+shift+e` | `hints.open` | Label URLs, paths and hashes on screen |
| `ctrl+shift+c` | `copy` | Copy the selection |
| `ctrl+shift+v` | `paste` | Paste the clipboard |
| `shift+insert` | `paste_primary` | Paste the primary selection |
| `ctrl+=`, `ctrl+shift+=`, `ctrl++` | `font.increase` | Larger font |
| `ctrl+-`, `ctrl+shift+-` | `font.decrease` | Smaller font |
| `ctrl+0`, `ctrl+shift+0` | `font.reset` | Configured font size |
| `ctrl+shift+z`, `ctrl+shift+x` | `scroll.prev_prompt`, `scroll.next_prompt` | Scroll to the previous or next prompt |
| `shift+pageup`, `shift+pagedown` | `scroll.page_up`, `scroll.page_down` | Scroll one page |
| `shift+home`, `shift+end` | `scroll.top`, `scroll.bottom` | Scroll to the top or bottom |

The scroll keys only scroll when the program shows its normal screen. In
full-screen programs (the alternate screen, as used by editors and pagers) the
key goes to the program.

### Locked mode

`ctrl+g` (`mode.normal`) unlocks. The copy, paste, font size and scroll keys
from the end of the Normal table also work here. Everything else goes to the
program.

### Pane mode

| Key | Action | Does |
|-----|--------|------|
| `h` `j` `k` `l`, arrows | `pane.focus_left` ... `pane.focus_right` | Focus a neighbor |
| `n` | `pane.new` | New pane along the longer side |
| `d`, `s` | `pane.split_down` | New pane below |
| `r`, `v` | `pane.split_right` | New pane to the right |
| `x` | `pane.close` | Close the pane |
| `f`, `z` | `pane.zoom` | Zoom the pane, or unzoom |
| `w` | `pane.toggle_floating` | Show or hide floating panes |
| `e` | `pane.toggle_embed` | Float the pane, or embed a floating one |
| `S` | `pane.toggle_stack` | Stack with the sibling, or unstack |
| `T` | `ui.toggle_pane_titles` | Show or hide pane title rows |
| `W` | `pane.to_new_window` | Move the pane to a new window |
| `c` | `pane.rename` | Name the pane |

### Tab mode

| Key | Action | Does |
|-----|--------|------|
| `n` | `tab.new` | New tab |
| `1` ... `9` | `tab.goto_1` ... `tab.goto_9` | Go to a tab |
| `h`, `k`, `left`, `up` | `tab.prev` | Previous tab |
| `l`, `j`, `right`, `down` | `tab.next` | Next tab |
| `b` | `pane.to_new_tab` | Move the focused pane to a new tab |
| `r` | `tab.rename` | Name the tab |
| `x` | `tab.close` | Close the tab |
| `w` | `tab.to_next_window` | Move the tab to the next window |
| `W` | `tab.to_new_window` | Move the tab to a new window |

### Resize mode

| Key | Action | Does |
|-----|--------|------|
| `h` `j` `k` `l`, arrows | `pane.grow_left` ... `pane.grow_right` | Grow the pane on that side by 2 cells |
| `H` `J` `K` `L`, Shift+arrows | `pane.shrink_left` ... `pane.shrink_right` | Shrink it on that side |
| `=` | `pane.equalize` | Give all panes equal sizes |

### Move mode

| Key | Action | Does |
|-----|--------|------|
| `h` `j` `k` `l`, arrows | `pane.move_left` ... `pane.move_right` | Swap with the neighbor (a floating pane moves instead) |
| `t`, `b` | `pane.to_new_tab` | Move the pane to a new tab |

### Scroll mode

| Key | Action | Does |
|-----|--------|------|
| `h` `j` `k` `l`, arrows | `copy.left` ... `copy.right` | Move the cursor |
| `w`, `b`, `e` | `copy.word_next`, `copy.word_prev`, `copy.word_end` | Word motions |
| `0`, `^`, `home` | `copy.line_start` | Start of line |
| `$`, `end` | `copy.line_end` | End of line |
| `g`, `G` | `copy.top`, `copy.bottom` | Oldest line, last line of the screen |
| `u`, `ctrl+u` / `d`, `ctrl+d` | `copy.half_page_up` / `copy.half_page_down` | Half a page |
| `ctrl+b`, `pageup` / `ctrl+f`, `pagedown` | `copy.page_up` / `copy.page_down` | A page |
| `H`, `M`, `L` | `copy.screen_top`, `copy.screen_middle`, `copy.screen_bottom` | Top, middle, bottom of the view |
| `[`, `]` | `copy.prev_prompt`, `copy.next_prompt` | Previous or next prompt |
| `ctrl+y`, `ctrl+e` | `scroll.line_up`, `scroll.line_down` | Scroll the view one line |
| `v`, `V`, `ctrl+v` | `copy.select`, `copy.select_line`, `copy.select_block` | Start a character, line or block selection |
| `y`, `enter` | `copy.yank` | Copy the selection (or the cursor line) and leave |
| `/`, `?`, `s` | `search.start` | Search |
| `n`, `N` | `search.up`, `search.down` | Next match towards older or newer lines |
| `c`, `o`, `a` | `block.copy_command`, `block.copy_output`, `block.copy_all` | Copy part of the command block at the cursor |
| `C`, `O`, `A` | `block.select_command`, `block.select_output`, `block.select_all` | Select part of the block |
| `z`, `Z` | `block.fold`, `block.unfold_all` | Fold the block's output, unfold all |
| `r` | `block.rerun` | Run the block's command again (leaves Scroll mode) |
| `i` | `block.info` | Command, exit status, duration and directory |
| `f` | `hints.open` | Hints |
| `esc` | `copy.escape` | Clear the selection, else the search, else leave |
| `q` | `mode.normal` | Leave |

Command blocks need shell integration; see [blocks.md](blocks.md).

### Session mode

| Key | Action | Does |
|-----|--------|------|
| `s` | `layout.save` | Save the current window as a layout |
| `l` | `layout.load` | Open a saved layout |

### Moving between modes

In every mode except Normal and Locked:

- `esc` returns to Normal (in Scroll mode, `esc` first clears the selection
  and the search).
- The chord that entered the mode returns to Normal (Ctrl+p in Pane mode).
- The other entry chords switch directly to their mode (Ctrl+t in Pane mode
  goes to Tab mode).
- `ctrl+g` locks.

## Leader preset

Normal mode has the same Alt quick keys, window keys and terminal keys as the
zellij preset, but no Ctrl+p/t/n/h/s/o. The leader (`ctrl+g`) opens the menu:

| Menu key | Enters |
|----------|--------|
| `p` | Pane mode |
| `t` | Tab mode |
| `r` | Resize mode |
| `m` | Move mode |
| `s` | Scroll mode |
| `o` | Session mode |
| `l` | Locked mode |
| leader | Sends the leader chord to the program |
| `esc` | Back to Normal |

Inside a mode, `esc` or the leader returns to Normal. The mode tables are the
same as in the zellij preset. In Locked mode, the leader unlocks.

## tmux preset

Normal mode has only the window keys (`ctrl+shift+p`, `ctrl+shift+n`,
`ctrl+shift+e`) and the terminal keys (copy, paste, font size, scroll). The
prefix `ctrl+b` opens the menu, which takes one key and returns to Normal:

| Key after `ctrl+b` | Action |
|--------------------|--------|
| `c` | `tab.new` |
| `n`, `p` | `tab.next`, `tab.prev` |
| `1` ... `9` | `tab.goto_1` ... `tab.goto_9` |
| `%` | `pane.split_right` |
| `"` | `pane.split_down` |
| `h` `j` `k` `l`, arrows | `pane.focus_left` ... `pane.focus_right` |
| `o` | `pane.focus_next` |
| `x` | `pane.close` |
| `&` | `tab.close` |
| `z` | `pane.zoom` |
| `!` | `pane.to_new_tab` |
| `{`, `}` | `pane.move_left`, `pane.move_right` |
| `,` | `tab.rename` |
| `.` | `pane.rename` |
| `[` | `mode.scroll` |
| `/` | `search.start` |
| `:` | `palette` |
| `w` | `pane.toggle_floating` |
| `s` | `mode.session` |
| `r` | `mode.resize` |
| `m` | `mode.move` |
| `ctrl+left`, `ctrl+right`, `ctrl+up`, `ctrl+down` | `pane.grow_left` ... `pane.grow_down` |
| `ctrl+b` | Send Ctrl+b to the program |
| `esc` | Back to Normal |

Inside the resize, move, scroll and session modes, `esc` or `ctrl+b` returns
to Normal (in Scroll mode, `esc` first clears the selection and the search,
and `q` also leaves).

## Change key bindings

Bindings go in your config file, `$XDG_CONFIG_HOME/proteus/init.lua`. Your
entries are applied in order on top of the preset; a later entry for the same
mode and key wins. Saving the file reloads the config, including the keys.

### In `p.config`

```lua
local p = require("proteus")
p.config {
  keys = {
    preset = "zellij",
    sticky = false,
    normal = {
      ["alt+x"] = "pane.close",
      ["alt+t"] = "none",            -- unbind: Alt+t goes to the program
    },
    pane = {
      ["|"] = "pane.split_right",
      ["-"] = "pane.split_down",
    },
  },
}
```

Inside `keys`, each mode name (`normal`, `locked`, `menu`, `pane`, `tab`,
`resize`, `move`, `scroll`, `session`) holds a table of key = action name. The
value `"none"` or `false` removes the binding.

### With `p.keymap`

`p.keymap(mode, key, action)` binds one key. `action` is an action name, a Lua
function, or `"none"`/`false` to unbind. It only works while the config file
runs, not later from a callback.

```lua
local p = require("proteus")

p.keymap("normal", "alt+p", "pane.toggle_private")
p.keymap("normal", "f5", "tab.goto_12")
p.keymap("pane", "g", function(ctx)
  ctx.pane:split { cmd = "lazygit" }
end)
p.keymap("normal", "alt+f", false)    -- Alt+f goes to the program again
```

A Lua function gets a context table whose `pane` field is the focused pane.
See [scripting.md](scripting.md) for what a pane object can do, and
`p.run_action(name)` to run any action from Lua.

`tab.goto_N` accepts any N from 1 to 99. The other actions are listed in
[Actions](#actions).

Plugins add actions named `plugin:PLUGIN-ID:ACTION-ID`, which you bind like
any other action. A plugin can also suggest default keys; those only fill keys
the preset leaves free, and anything you set for that key (including
`"none"`) wins. See [plugins.md](plugins.md).

The `menu` mode is only reachable in the zellij preset if you bind
`mode.menu` to a key yourself.

### Key names

A key is optional modifiers joined with `+`, then one key name.

| Part | Accepted names |
|------|----------------|
| Modifiers | `ctrl` (`control`), `alt` (`meta`, `opt`), `shift`, `super` (`logo`, `win`, `cmd`) |
| Named keys | `space`, `enter` (`return`), `tab`, `esc` (`escape`), `backspace`, `up`, `down`, `left`, `right`, `home`, `end`, `pageup` (`pgup`), `pagedown` (`pgdn`), `insert`, `delete` (`del`), `f1` to `f35` |
| Other keys | One character: `a`, `1`, `[`, `$`, `?`, `+` |

Rules:

- An uppercase letter means Shift: `H` is `shift+h`, `ctrl+B` is `ctrl+shift+b`.
- Shifted symbols can be bound by the character they type: `$` matches
  Shift+4 on a US layout, `?` matches Shift+/.
- `ctrl++` is Ctrl and the `+` key.
- Modifier names are case-insensitive. Num Lock and Caps Lock are ignored.
- Keypad keys match the bindings of their main-keyboard counterparts.
- An unknown modifier, key or action is a config error that names the entry.

### Check your bindings

```sh
proteus --check-config          # evaluate the config and report errors
proteus --list-keybindings      # every binding as JSON: mode, key, action
proteus --diagnostics           # also lists keys you declared more than once
proteus --list-actions          # every bindable action name
```

`--list-keybindings` does not include keys that plugins add at run time.

## Actions

Run `proteus --list-actions` for the current list. Actions marked "no default"
have no key in the zellij preset; bind one or run them from the palette.

### Modes and keys

| Action | Does |
|--------|------|
| `mode.normal` | Leave the current mode |
| `mode.locked` | Lock: pass keys to the program |
| `mode.menu` | Open the mode menu |
| `mode.pane`, `mode.tab`, `mode.resize`, `mode.move`, `mode.scroll`, `mode.session` | Enter that mode |
| `send_key` | Send the pressed key to the program |
| `palette` | Open the action palette |

### Panes

| Action | Does |
|--------|------|
| `pane.focus_left`, `pane.focus_down`, `pane.focus_up`, `pane.focus_right` | Focus the neighbor in that direction |
| `pane.focus_or_tab_left`, `pane.focus_or_tab_right` | Same, but at the window edge go to the previous or next tab |
| `pane.focus_next` | Next pane of the tab, tiled then floating, wrapping (no default) |
| `pane.focus_next_global`, `pane.focus_prev_global` | Next or previous pane across all tabs of the window, wrapping (no default) |
| `pane.split_right`, `pane.split_down` | Split the focused pane |
| `pane.new` | Split along the longer side; with a floating pane focused, a new floating pane |
| `pane.close` | Close the focused pane |
| `pane.zoom` | Zoom the focused pane, or unzoom |
| `pane.grow_left` ... `pane.grow_right` | Grow on that side by 2 cells |
| `pane.shrink_left` ... `pane.shrink_right` | Shrink on that side by 2 cells |
| `pane.equalize` | Equal sizes |
| `pane.move_left` ... `pane.move_right` | Swap with the neighbor; a floating pane moves |
| `pane.to_new_tab` | Move the focused pane to a new tab |
| `pane.to_new_window` | Move the focused pane to a new window |
| `pane.toggle_floating` | Show or hide floating panes (creates one if there is none) |
| `pane.new_floating` | New floating pane (no default) |
| `pane.toggle_embed` | Float the focused pane, or embed the focused floating pane |
| `pane.toggle_stack` | Stack the pane with its sibling, or unstack |
| `pane.rename` | Name the pane (empty: automatic title) |
| `pane.toggle_private` | Hide the pane from plugins and other readers, or show it again (no default) |
| `ui.toggle_pane_titles` | Show or hide title rows above tiled panes |

### Tabs and windows

| Action | Does |
|--------|------|
| `tab.new` | New tab after the current one |
| `tab.goto_1` ... `tab.goto_9`, `tab.goto_N` | Go to tab N |
| `tab.prev`, `tab.next` | Previous or next tab, wrapping |
| `tab.move_left`, `tab.move_right` | Move the tab one place, wrapping (no default) |
| `tab.rename` | Name the tab (empty: automatic title) |
| `tab.close` | Close the tab and its panes |
| `tab.to_new_window` | Move the tab to a new window |
| `tab.to_next_window` | Move the tab to the next window |
| `window.new` | New window |
| `window.close` | Close the window (no default) |

### Scrolling, selection and search

| Action | Does |
|--------|------|
| `scroll.line_up`, `scroll.line_down` | Scroll one line |
| `scroll.half_page_up`, `scroll.half_page_down` | Scroll half a page (no default) |
| `scroll.page_up`, `scroll.page_down` | Scroll a page |
| `scroll.top`, `scroll.bottom` | Scroll to the top or bottom |
| `scroll.prev_prompt`, `scroll.next_prompt` | Scroll to the previous or next prompt |
| `copy.left`, `copy.right`, `copy.up`, `copy.down` | Scroll-mode cursor motions |
| `copy.word_next`, `copy.word_prev`, `copy.word_end` | Word motions |
| `copy.line_start`, `copy.line_end` | Start or end of line |
| `copy.top`, `copy.bottom` | Oldest line, last screen line |
| `copy.half_page_up`, `copy.half_page_down`, `copy.page_up`, `copy.page_down` | Page motions |
| `copy.screen_top`, `copy.screen_middle`, `copy.screen_bottom` | Top, middle or bottom of the view |
| `copy.prev_prompt`, `copy.next_prompt` | Previous or next prompt |
| `copy.select`, `copy.select_line`, `copy.select_block` | Start, switch or stop a selection at the cursor |
| `copy.yank` | Copy the selection or cursor line, leave Scroll mode |
| `copy.escape` | Clear the selection, else the search, else leave Scroll mode |
| `search.start` | Type a search query (enters Scroll mode) |
| `search.up`, `search.down` | Next match towards older or newer lines |
| `scrollback.export` | Save the pane's scrollback to a file; asks for the path and does not overwrite existing files (no default) |

### Clipboard, font and display

| Action | Does |
|--------|------|
| `copy` | Copy the selection to the clipboard |
| `paste`, `paste_primary` | Paste the clipboard or the primary selection |
| `font.increase`, `font.decrease` | Font size up or down by one point |
| `font.reset` | Back to the configured size |
| `renderer.reset` | Reset the window's renderer (no default) |
| `image.view` | Open the newest image on screen in the image viewer (no default); see [images.md](images.md) |

### Command blocks and hints

| Action | Does |
|--------|------|
| `block.copy_command`, `block.copy_output`, `block.copy_all` | Copy part of the block at the Scroll cursor, else of the last block |
| `block.select_command`, `block.select_output`, `block.select_all` | Select part of the block |
| `block.fold`, `block.unfold_all` | Fold or unfold the block's output, unfold all |
| `block.rerun` | Run the block's command again |
| `block.info` | Show command, exit status, duration and directory |
| `hints.open` | Label on-screen URLs, paths, hashes; picking one opens it (URLs and files with `xdg-open`, `path:line` in `$EDITOR`), other kinds are copied |
| `hints.copy` | Same, but copy the picked text (no default) |
| `hints.insert` | Same, but type the picked text into the focused pane (no default) |

See [blocks.md](blocks.md) and [scrollback.md](scrollback.md).

### Layouts, config and SSH

| Action | Does |
|--------|------|
| `layout.save` | Save the current window as a layout |
| `layout.load` | Open a saved layout in new tabs |
| `config.open` | Open your config file in an editor pane (no default) |
| `ssh.connect`, `ssh.connect_bash`, `ssh.reconnect`, `ssh.workspace`, `ssh.sftp`, `ssh.upload`, `ssh.download`, `ssh.forward_local`, `ssh.forward_remote`, `ssh.stop` | SSH actions (no defaults); see [ssh.md](ssh.md) |

## The action palette

Ctrl+Shift+P (`palette`; `ctrl+b :` in the tmux preset) opens a prompt at the
bottom of the window that runs anything by name. It lists:

- every action, by name;
- SSH destinations and actions, and workspace tasks
  ([ssh.md](ssh.md), [workspaces.md](workspaces.md));
- entries your config adds with `p.palette.add(name, fn)`, and plugin
  entries;
- the tabs of the window (`tab 2: ...`) and the panes of all windows
  (`pane 7: ...`); picking a pane shows it;
- saved layouts (`layout: dev`).

Type words to filter; every word must appear in the entry. Press Tab or Down
to show the list of matches with their key bindings, Up or Shift+Tab to go
back, and Enter to run the highlighted entry. Esc closes the list, or the
palette when no list is shown. Entries you used recently come first. The last 30 are
remembered in `$XDG_STATE_HOME/proteus/palette_recent` (default
`~/.local/state/proteus/palette_recent`).

The prompt edits like a shell line: Ctrl+a, Ctrl+e, Ctrl+b, Ctrl+f, Ctrl+d,
Ctrl+u and Ctrl+w work, and Ctrl+c or Ctrl+g cancel.

## Keyboard protocol

Proteus implements the kitty keyboard protocol, including its query, push, pop
and set sequences and all five enhancement flags (disambiguate escape codes,
report event types, report alternate keys, report all keys as escape codes,
report associated text). Programs that enable it can tell apart keys that are
otherwise identical in a terminal, for example Shift+Enter and Enter.

Things you may notice:

- A key Proteus binds in the current mode never reaches the program, and
  neither does its release.
- Presses of a modifier key alone (Shift, Ctrl...) are only sent when the
  program asked for all keys as escape codes, and only while no Proteus mode,
  prompt, palette or hint labels are active.
- While a plugin pane or plugin panel has the focus, keys not bound in the
  current mode go to the plugin.
