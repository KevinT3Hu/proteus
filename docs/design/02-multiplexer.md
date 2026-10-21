# 02 — Multiplexer

## Goals

- Replace tmux/zellij for local work without an extra process.
- Discoverable keyboard model (D3: modes + hint bar) that never fights nvim/shell bindings.
- Chrome that looks like part of the terminal (D4), but behaves like native GUI.

## Model

```
Window
 └─ Tab (1..n)
     ├─ Tiled layout: binary split tree of Panes (horizontal / vertical, ratio)
     ├─ Stacks: a split node may be a stack — one expanded pane, others shown as 1-line title bars
     ├─ Floating panes: z-ordered, positioned in cells, toggled as a group
     └─ Zoom: one pane temporarily fills the tab
Pane = Terminal | PluginView
```

- Pane titles: OSC 0/2 title, else foreground process name + cwd.
- New panes inherit the focused pane's cwd (from OSC 7 or `/proc`).
- Closing a pane whose foreground process group (`tcgetpgrp`) is not the shell asks for confirmation; configurable allowlist (e.g. never ask for `less`).
- Multiple windows supported; panes can be moved between tabs and windows.

## Keyboard model (D3)

### Modes

| Mode | Purpose | Example keys inside the mode |
|---|---|---|
| Normal | Keys go to the app, except the mode-entry key | — |
| Locked | Everything goes to the app, including the mode-entry key; only the unlock key is intercepted | — |
| Pane | Focus, split, close, zoom, float, stack | `hjkl` focus, `s`/`v` split, `x` close, `z` zoom, `f` float, `w` toggle floats, `r` rename |
| Tab | Create, switch, rename, move | `n` new, `1-9` goto, `hl` prev/next, `r` rename, `x` close |
| Resize | Resize panes | `hjkl` grow, `HJKL` shrink, `=` equalize |
| Move | Move panes in the tree | `hjkl` swap, `t` to new tab |
| Scroll | Scrollback navigation and selection | vi motions, `[`/`]` prev/next prompt, `v` select, `y` copy, `/` search |
| Search | Incremental search (doc 01) | `n`/`N`, `Enter`, `Esc` |
| Session | Layouts and session restore | `l` load layout, `s` save layout |

### Default entry: "leader to mode menu" (non-colliding)

zellij's default chords (`Ctrl+p`, `Ctrl+t`, `Ctrl+s`, `Ctrl+n`...) collide with shell history, fzf and editors. Proteus defaults to a single leader:

- `Ctrl+g` (configurable) from Normal opens the mode menu; hint bar shows `p pane · t tab · r resize · m move · s scroll · / search · o session · l lock`.
- After an action, return to Normal (default) or stay in the mode (`modes.sticky = true`); `Esc` always returns to Normal.
- Locked mode: entered with `Ctrl+g l`; only `Ctrl+g` is intercepted to unlock.
- A few direct shortcuts work in Normal without the leader (configurable, all can be disabled): `Alt+h/j/k/l` focus, `Alt+n` new pane, `Alt+[`/`Alt+]` prev/next tab, `Ctrl+Shift+c/v` copy/paste, `Ctrl+Shift+p` palette.
- Presets: `default`, `zellij` (original chords), `tmux` (`Ctrl+b` prefix, tmux keys).
- Every binding is a palette action; Lua can rebind anything (doc 05).

### Hint bar

- One line at the bottom of the window, shown while a non-Normal mode is active (configurable: always / in-mode / never).
- Lists the keys of the current mode, generated from the active keymap so it is always accurate.

## Chrome (D4)

Tab bar, hint bar, status bar, pane borders and stack title bars.

- Rendered natively (our renderer, not the cell grid of any pane), so they support hover, click, drag, tooltips, smooth highlight, crisp separators at fractional scale.
- Visually aligned to the cell grid: same font, same cell height, theme palette colors, Powerline-style separators drawn procedurally. To the eye it looks like zellij; to the hand it behaves like a GUI.
- Pane borders: 1-cell-wide or thin pixel lines (configurable); focused pane highlighted; title and mode indicator embedded in the top border (optional).
- Mouse: click to focus/switch, drag tab to reorder or to another window, drag border to resize, middle-click tab to close, scroll on tab bar to switch tabs.
- Status bar segments: mode, tabs, plugin widgets (git branch, AI status, clock). Segments are provided by Lua functions or plugins (doc 05).
- Position: tab bar top or bottom, or hidden when only one tab; status merged into tab bar by default.

## Layouts

- Defined in Lua (D6), e.g. `layouts/dev.lua` returning a table:

```lua
return {
  tabs = {
    { name = "code", cwd = "~/src/app",
      split = "vertical", ratio = 0.65,
      { cmd = "nvim" },
      { split = "horizontal", { cmd = "cargo watch -x test" }, {} },
    },
    { name = "files", plugin = "proteus.files", mode = "pane" },
  },
}
```

- Launch: `proteus --layout dev`, palette, or Session mode.
- Save current tab/window as a layout file (structure, cwd, foreground command).

## Session restore

- `session.restore = "never" | "ask" | "always"` (default `"ask"` after an unclean exit, `"never"` otherwise).
- Restores windows, tabs, split tree, ratios, floats, pane cwd and titles.
- Commands are re-run only if in `session.restore_commands` allowlist (default: `nvim`, `hx`, `less`, `htop`, `btop`).
- Scrollback never restored in v1.
- Stored as Lua-readable data in `$XDG_STATE_HOME/proteus/session.lua`; written on change, debounced.

## Edge cases

- Minimum pane size: 2 columns × 1 row; resizing beyond is clamped; split refused if too small.
- PTY resize debounced during drag; final size sent on release.
- Window close with running processes: one confirmation listing them.
- Last pane closed closes the tab; last tab closes the window (configurable to keep an empty shell).

## Decisions

1. Default keys follow zellij (`keys.preset = "zellij"`): `Ctrl+p/t/n/h/s` enter Pane/Tab/Resize/Move/Scroll directly (pressing the entry key again leaves the mode), `Ctrl+g` locks/unlocks. `keys.preset = "leader"` keeps the single-leader menu for people who need those chords in the shell.
2. One-step quick keys in Normal mode, in both presets: `Alt+h/j/k/l` focus (`h`/`l` continue into the previous/next tab at the window edge), `Alt+n` new pane, `Alt+t` new tab, `Alt+[`/`Alt+]` prev/next tab, `Alt+1-9` go to tab.
3. Every operation is a named action (`proteus --list-actions`). Each mode is a table of key → action; the config overrides single keys per mode (`"none"` unbinds), so presets are only defaults.
4. Sticky modes default off (`keys.sticky = false`); Resize, Move and Scroll always stay active after an action.

```lua
keys = {
  preset = "zellij", leader = "ctrl+g", sticky = false,
  normal = { ["alt+x"] = "pane.close", ["ctrl+h"] = "none" },
  pane = { v = "pane.split_right" },
},
ui = { hint_bar = "in-mode", tab_bar = "auto" },
panes = { confirm_close = true, confirm_close_ignore = { "less", "man" } },
```


5. Scroll mode (`Ctrl+s`) has a keyboard cursor with vi motions (`hjkl`, `w/b/e`, `0/$`, `g/G`, `H/M/L`, `d/u`, `[`/`]` prompts). `v`/`V`/`Ctrl+v` select by character, line or block. `y`/`Enter` copies to the clipboard and the primary selection and returns to Normal; with no selection it copies the cursor line. `Esc` first clears the selection, then the search, then leaves the mode. Leaving Scroll mode scrolls back to the bottom.
6. Search: `/`, `?` or `s` in Scroll mode, or `Ctrl+Shift+f` from Normal. It is incremental and searches upward from the cursor. Case is smart; `Ctrl+r` toggles regex. `Enter` moves the cursor to the match, and `n`/`N` go to the next older/newer match. The input line takes over the hint bar.
7. Rename: `r` in Tab mode, `c` in Pane mode (zellij keys). An empty name goes back to the automatic title.
8. Session mode (`Ctrl+o`): `s` saves the window as `layouts/NAME.lua`, `l` opens a layout's tabs after the existing ones.
9. Layout node keys: `cmd` replaces the shell (a string runs through `$SHELL -c`; the pane closes when it exits). `run` is typed into the shell (the pane stays). `split = "vertical"` places children side by side and `"horizontal"` stacks them. More than two children get equal shares. Plugin panes are rejected until M5.
10. Saved layouts and the session record only foreground commands listed in `session.restore_commands`, stored as `run`. The two use the same file format and parser.
11. Session file: only the instance holding `$XDG_STATE_HOME/proteus/session.lock` saves and restores. It saves every 2 s when the file's contents would change, marked `clean = false`. Closing the window writes `clean = true`; the last pane ending deletes the file. `"ask"` (default) offers a restore only when the file is unclean, `"always"` restores whenever a file exists, and `"never"` neither saves nor restores. An explicit `--layout` or `-- COMMAND` skips restore.

12. Floating panes (zellij keys): `Alt+f` or Pane-mode `w` shows or hides them (it creates the first one if there are none), Pane-mode `e` floats or embeds the focused pane, and `n` opens another floating pane while a float is focused. Floats have a frame with a title row: drag the title to move a float and its corner to resize it. In Move mode `hjkl` moves the float; in Resize mode it grows or shrinks it. The focused float is raised to the top.
13. Stacks: Pane-mode `S` stacks the focused pane with its sibling subtree, or unstacks it. The expanded pane has a title row, and the other panes collapse to one-row titles. Focus moves across those titles, and clicking a title expands its pane. Layout files write a stack as `stacked = true` on a split.
14. Pane title rows: `ui.pane_titles = "never"` (default), `"auto"` (when a tab has splits) or `"always"`; Pane-mode `T` toggles them. The focused pane's title shows the current mode.
15. Windows: `Ctrl+Shift+n` opens a new window, Tab-mode `W` moves the tab to a new window and `w` to the next window, and Pane-mode `W` moves the pane to a new window. Drag a tab out of the tab bar to move it with Wayland drag and drop: dropped on another window it joins that window's tabs, dropped elsewhere it gets a new window. A window left without tabs closes. Each window has its own renderer, fonts and font size. The session file stores all windows (`windows = { { tabs = ... }, ... }`), and so can layout files.
16. `keys.preset = "tmux"`: the prefix (`Ctrl+b`, or `keys.leader`) followed by tmux keys: `c n p 1-9 % " o x & z ! { } , [ / : w s r m`, `Ctrl+arrows` to resize; pressing the prefix twice sends it to the application. No Alt quick keys.
17. Palette: `Ctrl+Shift+p` (tmux: prefix `:`). Type words of an action name, use `Tab`/arrows to choose, `Enter` to run.
18. `panes.keep_last_shell = true`: closing a window's last pane starts a new shell instead of closing the window.
19. Status-bar segments from Lua functions and plugins remain for M5 (plugin API).
