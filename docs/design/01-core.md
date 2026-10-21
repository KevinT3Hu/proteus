# 01 — Core Terminal & Shell Integration

## Goals

- Correct VT/xterm emulation; full-screen apps (nvim, htop, less, tig) render exactly.
- Low latency: keypress-to-photon competitive with Alacritty/foot.
- First-class CJK: IME, wide characters, correct glyph localization.
- Shell integration turns output into structured command blocks without changing how the terminal looks.

## Emulation

- Parser: `vte`. Grid, scrollback, modes in `proteus-core` (no GUI deps).
- Scrollback: default 10,000 lines per pane, configurable; stored compactly (ring buffer of rows, styles interned).
- Reflow on resize, including wide chars and wrapped lines.
- Supported: true color, 256 color, undercurl and colored underlines (`CSI 4:3 m`, `58`), OSC 8 hyperlinks, OSC 52 clipboard (write allowed, read asks the user), OSC 7 cwd, OSC 133 marks, OSC 9 / 777 notifications, synchronized output (mode 2026), bracketed paste, focus events, SGR mouse, grapheme clustering (mode 2027), DECSCUSR cursor shapes.
- Kitty keyboard protocol and graphics: see doc 04.

## Rendering & fonts

- Renderer: `wgpu` (D11), surface created from the SCTK `wl_surface` via `raw-window-handle`.
  - Backends: Vulkan primary; GL (EGL) fallback enabled in the build for drivers without usable Vulkan. Backend overridable with `render.backend = "auto" | "vulkan" | "gl"`.
  - Instanced quads: one pass for cell backgrounds, one for glyphs from a glyph atlas (separate RGBA atlas for color emoji), then images (doc 04) and chrome (doc 02). Shaders in WGSL.
  - Present mode `Fifo`, render only on Wayland frame callback and only when damaged; no continuous redraw loop.
  - Startup: create the window and request the adapter/device in parallel with font loading; cache compiled pipelines (`wgpu` pipeline cache where the backend supports it) in `$XDG_CACHE_HOME/proteus/`.
  - Low-power adapter preferred (`PowerPreference::LowPower`) so laptops don't wake the discrete GPU.
  - Device loss (driver reset, suspend/resume): recreate device, surface and atlases from CPU-side state without losing terminal state.
  - Budget targets: first frame < 150 ms after launch on a warm cache; idle GPU usage zero.
- Fractional scale: render at physical size; rasterize glyphs at physical pixel size.
- Fonts: fontconfig for discovery and fallback; `rustybuzz` shaping; `swash` rasterization.
- Ligatures: on by default for fonts that have them; disabled under the cursor position option.
- Box-drawing, block elements, Braille, Powerline glyphs drawn procedurally so they align perfectly.
- Config: `font.family`, `font.size`, `font.cjk_family`, `font.emoji_family`, `font.lang` (default from locale, e.g. `zh-CN`, `ja`, `ko`) used for fontconfig fallback to pick the correct Han variant.

## CJK & input

- `xkbcommon` with compose (dead keys).
- IME via `text-input-v3`:
  - Enable on focus in, disable on focus out; apply state only on `done`.
  - Preedit drawn inline at the cursor, underlined, never written to the grid; preedit cursor shown.
  - `set_cursor_rectangle` on every cursor move so the candidate window follows.
  - IME active inside plugin UIs and the palette too.
- Width: `unicode-width`; `text.ambiguous_width = 1 | 2` (default 1).
- Wide char at last column wraps and leaves a spacer cell; the cursor is drawn double-width on wide chars.

## Shell integration

- Integration scripts for bash, zsh, fish are injected automatically (kitty/Ghostty style: env var + `ZDOTDIR`/`ENV`/`XDG_DATA_DIRS` tricks). Opt out with `shell_integration.enabled = false`.
- Emits OSC 133 (A prompt start, B command start, C output start, D exit status), OSC 7 cwd, and the command line itself.
- Without integration: everything works except block features; cwd falls back to `/proc/<pid>/cwd` of the foreground process.

### Command blocks (D2: classic)

- Output stays contiguous, exactly like a normal terminal.
- A thin gutter left of column 0 (width configurable, can be 0) shows a mark per block:
  - neutral mark at each prompt,
  - red mark + exit code on hover for non-zero exit,
  - spinner for running commands.
- Blocks are not drawn in alt screen (full-screen apps).
- Actions (palette + default keys in Scroll mode):
  - jump to previous / next prompt,
  - select / copy the command, the output, or both,
  - fold long output (fold replaced by a one-line "▸ 1,234 lines" marker; unfold restores),
  - re-run command,
  - send block to AI (if AI enabled).
- Duration and exit code stored per block and shown on hover or in a block info popup.
- Default keys in Scroll mode: `c`/`o`/`a` copy command/output/all, `C`/`O`/`A` select them, `z` fold, `Z` unfold all, `r` re-run, `i` info, `[`/`]` previous/next prompt. Without a Scroll cursor the actions use the last command. Clicking a gutter mark folds its block; clicking a fold marker unfolds it.

## Search

- Opened in Search mode (doc 02). Incremental, case-smart, optional regex.
- Scope: current pane (default) or all panes in tab.
- Matches highlighted; `n`/`N` navigate; `Enter` exits to selection at match.
- Search restricted to command output only (via blocks) as a toggle.
- Toggles while typing: `Ctrl+R` regex, `Ctrl+T` all panes of the tab, `Ctrl+O` output only.

## Hint mode

- Key enters hint mode; on-screen matches get short labels (home-row letters).
- Built-in patterns: URL, file path (absolute, relative to block cwd, `path:line:col`), git hash, IPv4/IPv6, UUID, hex color.
- Actions by modifier: open (`xdg-open` or `$EDITOR` for `path:line`), copy, insert into prompt.
- Patterns and actions extensible from Lua. v1 config: `hints = { alphabet, patterns = { { name, regex, url } } }`; Lua functions as actions come with doc 05.
- Keys: `Ctrl+Shift+e` (Normal), `f` (Scroll). A label opens; with Shift on its last key it copies, with Ctrl it is inserted at the prompt.

## Command palette

- Fuzzy list of every action: built-in, Lua-defined, plugin-provided. Shows current keybinding.
- Also searches tabs, panes (by title/cwd/command), layouts, themes.
- Recently used actions ranked first.

## Notifications

- When a command running longer than `notify.min_duration` (default 10 s) finishes while the window or pane is not focused: desktop notification via `org.freedesktop.Notifications`, with exit status.
- Clicking it focuses the window (`xdg-activation-v1`) and the pane.
- OSC 9 / OSC 777 from apps pass through, rate-limited.
- Bell: visual flash by default; tab/pane gets a bell marker.

## Clipboard & selection

- CLIPBOARD and PRIMARY. Middle-click pastes PRIMARY.
- Selection: normal, word, line, rectangular (Alt-drag), semantic (block output).
- Paste protection: confirm multi-line paste or paste containing control characters when not in bracketed paste mode.
- Keyboard selection in Scroll mode (vi-like motions, `v`, `V`, `Ctrl+v`, `y`).

## Theming

- Built-in themes; light/dark switch follows `org.freedesktop.appearance.color-scheme` via the XDG settings portal.
- Hot reload of theme and config.
- Theme covers terminal palette and chrome (doc 02) together.

## Non-goals (v1)

- Detachable sessions, SSH domains, scrollback restore.

## Decisions (were open questions)

1. Ligatures: on by default (`font.ligatures`).
2. Gutter: 0.5 cell by default, pixel-rendered in the padding/gap left of each pane so no column is lost (`ui.gutter`, 0 hides it).
