# 03 — File Manager (first-party plugin `proteus.files`)

## Goals

- yazi-class file manager integrated with panes, the prompt, images and AI.
- Implemented purely as a WASM plugin (D5). Any capability it needs must be a public plugin API; no private host hooks. It is the acceptance test for doc 05.

## Packaging

- Written in Rust, compiled to `wasm32-wasip2`, embedded in the `proteus` binary and installed as a bundled plugin.
- Bundled plugins have their permissions pre-granted but visible and revocable in the plugin manager like any other plugin.
- Can be disabled or replaced by a third-party file manager.

## Surfaces

One plugin, three surfaces (all from the plugin UI API):

| Surface | Open with | Behaviour |
|---|---|---|
| Sidebar | `Alt+e` / palette | Docked left or right, toggles, follows focused pane's cwd |
| Pane | `E` in Pane mode / layout | Full pane, like running yazi |
| Picker | `Alt+p` / palette | Floating popup; Enter inserts the selected path(s) into the focused pane's prompt and closes |

## Navigation & view

- Views: Miller columns (default in pane), tree (default in sidebar), list. Configurable per surface.
- Keys of this build: `w` cycles the view; `,` then `n`/`m`/`s`/`e` sorts by name/mtime/size/extension, `,d` toggles dirs first, `,r` reverses; `F` toggles follow (sidebar); `R` reloads; `q` closes; Esc clears the filter, then the marks, then returns to the pane.
- Vim keys: `hjkl`, `gg`/`G`, `/` fuzzy filter, `z` jump (zoxide integration if installed), `~` home, `-` parent, `.` toggle hidden.
- Bookmarks: `m<key>` set, `'<key>` jump; stored in plugin data dir.
- Sorting: name (natural), mtime, size, extension; dirs first toggle.
- Git status markers per file (modified, staged, untracked, ignored) via `git` command permission.
- Follow mode (sidebar): updates when the focused pane's cwd changes (`cwd_changed` event); can be pinned.

## Preview

Shown in the last column (pane) or a floating preview (sidebar, toggle `p`).

| Type | Method |
|---|---|
| Text / code | Syntax highlighting in-plugin (`syntect` compiled to WASM), first N KB |
| Images (png, jpeg, gif, webp, svg) | Decoded in-plugin (`image`, `resvg`), `load-image` + frame placement (doc 04 layer) |
| Video | First frame via `ffmpegthumbnailer` (if installed, `run-command` permission) or freedesktop thumbnail cache |
| PDF | `pdftoppm` first page, if installed |
| Archives | Listing via `bsdtar -tf` / `unzip -l`, if installed |
| Directories | Child listing |
| Binary | Size, mime type, `file` output |

- Previews are async and cancellable: a preview starts 60 ms after the cursor stops; moving on kills its helper program (`spawn-process`) and drops its image. Text is the first 64 KB (highlighted up to 400 lines); SVG is drawn with `resvg` (no `<text>` support) and handed to `load-image` as PNG; archives use `bsdtar -tf`, else `unzip -Z1` for zip files.
- Video and PDF thumbnails are written by the helper straight into `~/.cache/thumbnails/large/<md5(uri)>.png` and reused while newer than the file (the helpers do not add the spec's `Thumb::URI`/`Thumb::MTime` tags).

## Terminal integration

| Action | Default key | Effect |
|---|---|---|
| cd pane here | `c` | Sends `cd -- '<dir>'` to the focused pane if it is at a prompt (block state from OSC 133); otherwise refuses with a message |
| Insert path | `i` | Inserts shell-quoted path(s) at the prompt |
| Open | `Enter` / `o` | Directory: enter; text: `$EDITOR` in a new split; other: `xdg-open` |
| Open in split / tab / float | `s` / `t` / `f` | `$EDITOR` in the chosen place |
| Ask AI | `a` | Adds selected files to AI chat context (only if AI enabled) |
| Drag | mouse | Drag file onto a pane inserts its quoted path |
| Copy path | `y` | Path to clipboard |

## Operations

- Copy, move, rename, mkdir, touch, symlink, change permissions.
- Bulk rename: opens file list in `$EDITOR` in a float; applied after save and confirmation.
- Delete: to freedesktop Trash (host `fs.trash`); permanent delete (`D`) requires typing confirmation.
- Selection: `Space` toggle, `v` visual range, `*` invert.
- All operations run as background tasks with progress in the status bar; conflicts prompt (overwrite / skip / rename / apply to all).
- Undo: last operation (rename, move, trash) can be undone with `u`.
- Keys of this build: `C` yank to copy, `x` cut, `P` paste into the current directory, `r` rename, `n` new file (`name/`: directory), `L` link the yanked files here, `M` chmod, `d` trash, `D` delete (type `yes`), `b` bulk rename, `v` visual range. Copies run in 40 ms steps on a timer (one plugin call stays short) as a cancellable task; a conflict waits in the surface's footer (`o`/`s`/`r`, capitals for all). Moves across file systems copy, then remove. Symlinks inside copied directories are skipped with an error (WASI has no `symlink`); `L` and `M` use `ln` and `chmod`. Undo of a trash restores from the home trash only. Enter opens text files in `$EDITOR` in a split.

## Remote panes

- If the focused pane's OSC 7 host differs from the local host: sidebar shows "remote: not available in v1" and stops following.
- v2: SFTP backend through the user's ssh config.

## Required plugin APIs (must be in doc 05 v1)

- UI: sidebar, pane, floating surfaces; styled cell rendering; image placement; key/mouse input; drag-and-drop source (`start-drag`, onto panes).
- Events: `cwd_changed`, `focus_changed`, `block_state_changed`, `config_changed`.
- Panes: query focused pane (cwd, host via `pane-remote`, at-prompt via `read-blocks`), `send_text`, `insert_text`, open command in split/tab/float.
- Filesystem: read/write within granted paths, `fs.watch` (host-side inotify), `fs.trash`, metadata, xattr not required.
- Processes: `run-command` with allowlisted binaries (`git`, `ffmpegthumbnailer`, `pdftoppm`, `bsdtar`, `file`, `zoxide`).
- Host services: `load-image` (pixels decoded by the plugin), `open`, `clipboard.write`, `notify`, background tasks with progress reporting.

## Permissions requested

`ui`, `read-panes`, `write-pty`, `fs:~` (read-write), `fs:/` (read-only), `run-command:<allowlist>`, `clipboard-write`.

## Performance targets

- Directory of 10,000 entries lists in < 100 ms and scrolls at frame rate.
- Sidebar open/close < 1 frame after first load.

## Open questions

1. Is `fs:/` read-only by default acceptable, or restrict to `~` and require opt-in for the rest? (Proposed: `~` read-write, `/` read-only.)
2. Default sidebar side: left or right? (Proposed: left.)
