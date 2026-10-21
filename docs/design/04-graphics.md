# 04 — Graphics & Keyboard Protocols

## Goals

- Images work correctly inside a multiplexer: per-pane clipping, scrollback, resize, zoom, floats. This is where Proteus beats tmux/zellij + kitty.
- Existing tools (yazi, ranger, chafa, timg, viu, mpv `--vo=kitty`, notcat, matplotlib backends) work unchanged.

## Protocols

| Protocol | Support |
|---|---|
| Kitty graphics | Full: transmit/display/delete, placements, z-index, Unicode placeholders (`U+10EEEE`), animation frames, all transmission modes |
| Sixel | Decode to RGBA, placed as image at cursor; DECSDM; color registers |
| iTerm2 inline (OSC 1337 `File=`) | `inline=1`, width/height in cells, px, %; `preserveAspectRatio` |
| Kitty keyboard protocol | Full progressive enhancement flags (disambiguate, report events, alternates, all keys, associated text) — in v1 |

### Kitty transmission modes

| Mode | Policy |
|---|---|
| `t=d` direct (base64, chunked) | Always |
| `t=f` file | Only if the pane is local (no remote OSC 7 host), path readable by the user, regular file, under size cap |
| `t=t` temp file | As `t=f`, plus file must be in a temp dir and is deleted after reading |
| `t=s` shared memory | Local panes only; POSIX shm object unlinked after reading |

## Placement model

- Each pane's `Terminal` owns an image store: image data (by id) and placements anchored to a (scrollback line, column) cell position, size in cells, z-index (below or above text).
- Images scroll with text; placements scrolled out of scrollback are dropped.
- On resize/reflow: anchor follows its cell; image keeps its cell size; clipped to pane bounds.
- Alt screen has its own placements, cleared on exit from alt screen.
- Renderer composites per pane into the pane's viewport rect with scissoring, so splits, floats and zoom need no special cases.
- Unicode placeholder mode works through tmux-like intermediaries and survives reflow because placements are text.
- Animation: frames advance on frame callbacks only when the pane is visible.

## Resource limits

| Limit | Default |
|---|---|
| Max single image (decoded) | 64 MiB |
| Image memory per pane | 256 MiB |
| Image memory per window | 1 GiB |
| GPU texture budget | Evict LRU textures, re-upload on demand |

- When over quota: evict least recently displayed images; their placements render a subtle "image evicted" placeholder.
- Decoding (PNG/JPEG/GIF/WebP for iTerm2 and kitty `f=100`) happens off the PTY thread.

## Built-in tooling

- `proteus +icat <file|url>...` — display images, like `kitty +kitten icat` (options: `--place`, `--scale`, `--clear`). Usable over ssh by falling back to direct transmission.
- Click an image (with modifier) to open it full-size in an overlay: zoom, pan, copy image, save.
- Plugin API `ui.image` uses the same store (file-manager previews, AI chat images).

## Security

- File and shm modes refused for remote panes and for paths outside what the user can read; no symlink following into `/proc`, `/sys`, `/dev`.
- Size caps enforced before allocation (declared dimensions checked).
- Query responses (`a=q`) never leak file contents or paths.

## Testing

- Conformance against kitty's graphics protocol test suite where available; snapshot tests of placement after scroll/resize/split.
- Manual matrix: yazi, chafa, timg, mpv, nvim image plugins (`image.nvim`, `snacks.nvim`).

## Open questions

1. Text-above-image vs image-above-text default z-index for iTerm2/Sixel? (Proposed: image below text, matches kitty.)
