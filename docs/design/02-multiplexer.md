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

## Open questions

1. Leader key: `Ctrl+g` (zellij unlock-first convention) or `Ctrl+Space`? (Proposed: `Ctrl+g`.)
2. Sticky modes default on or off? (Proposed: off.)
