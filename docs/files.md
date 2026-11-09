# Files: the file manager plugin

Files (plugin ID `proteus.files`) is the first-party file manager. It is a WASM
plugin like any other (see [plugins.md](plugins.md)) and is not enabled until
you load it. It has three views:

- a **sidebar** with a directory tree that follows the focused pane's
  directory,
- a **pane** with Miller columns and a preview, and
- a **picker**, a floating list that inserts the chosen paths into the pane you
  came from.

## Enable it

Release packages ship a prebuilt copy in `share/proteus/plugins/files`
(see [releases.md](releases.md)). Load it from `init.lua`, adjusting the prefix
to your installation:

```lua
require("proteus").load_plugin { source = "/usr/share/proteus/plugins/files" }
```

From a source checkout, point at `plugins/files` instead. Proteus then builds
it with Cargo, which needs the `wasm32-wasip2` target
(`rustup target add wasm32-wasip2`):

```lua
require("proteus").load_plugin { source = "~/code/proteus/plugins/files" }
```

On first start Proteus asks you to approve the plugin's permissions. The prompt
carries all three warnings (programs, panes, and writing your home directory),
because Files needs those to work. See [Permissions](#permissions) for how to
grant less.

## Open it

| Key (default) | Action ID | Opens |
|---|---|---|
| Alt+e | `plugin:proteus.files:sidebar.toggle` | The sidebar, or focuses it if it is open |
| `E` in Pane mode | `plugin:proteus.files:pane.open` | A Files pane to the right of the focused pane |
| Alt+p | `plugin:proteus.files:picker.open` | The picker, or focuses it if it is open |

The command palette lists the same actions as "Files: sidebar", "Files: pane"
and "Files: pick a path". These default keys are used only if they are free;
your own bindings win. To move or remove them:

```lua
local p = require("proteus")
p.keymap("normal", "alt+f", "plugin:proteus.files:sidebar.toggle")
p.keymap("normal", "alt+e", false)
```

A Files pane can also be part of a layout. `args` is the starting directory;
without it the pane starts in your home directory:

```lua
return { tabs = { { split = "vertical", { plugin = "proteus.files", args = "~/src" }, { cmd = "nvim" } } } }
```

See [panes-and-layouts.md](panes-and-layouts.md) for layout files.

## The views

| View | Starts in | Layout | Notes |
|---|---|---|---|
| Sidebar | The focused pane's directory | Tree | Follows the focused pane as it changes directory or focus. `F` pins or unpins it |
| Pane | The focused pane's directory | Columns | Shows a preview of the file under the cursor when it is at least 30 columns wide |
| Picker | The focused pane's directory | List | Enter on a file inserts it into the pane and closes the picker |

The sidebar stops following when you jump elsewhere with `-`, `~`, `'` or `z`;
press `F` to follow again. When the focused pane's directory is on another
machine (an SSH session reporting its directory), the sidebar shows
"HOST: remote browsing is not available".

`w` cycles the layout of any view: columns, tree, list.

## Keys

Keys work in the focused Files view. Actions that take files use the marked
files, or the file under the cursor if none are marked.

### Moving around

| Key | Action |
|---|---|
| `j` / `k`, Down / Up | Next / previous entry |
| `gg`, Home | First entry |
| `G`, End | Last entry |
| PageDown / PageUp | One page |
| Enter, `l`, `o`, Right | Open: enter a directory, or open a file (see below). In a tree, opens or closes the directory |
| `h`, Left | Back to the parent directory. In a tree, closes the open directory or moves to its parent row |
| `-` | Parent directory |
| `~` | Home directory |
| `z` | Ask for a query and jump to the result of `zoxide query` |
| `m` then a key | Bookmark the current directory under that key |
| `'` then a key | Jump to the bookmarked directory |
| `.` | Show or hide dotfiles (hidden by default) |
| `/` | Filter by name (see below) |
| `,` then a key | Sort: `n` name, `m` modified time (newest first), `s` size (biggest first), `e` extension, `d` directories first on/off, `r` reverse |
| `w` | Next layout |
| `R` | Read the directory again and refresh Git status |
| `F` | Sidebar: follow the focused pane on/off |
| `p` | Sidebar: show or hide a floating preview |
| `q` | Close this view |
| Escape | Leave visual mode, else clear the filter, else clear the marks, else close the picker or return to the terminal pane |

Opening a file: in the picker it inserts the path. Elsewhere, a text file opens
in your editor in a split to the right; other files open with the desktop's
default application (`xdg-open`). Executable files and `.desktop` files are not
opened this way.

Bookmarks are kept in the plugin's own storage and survive restarts.

The filter matches names fuzzily: the typed characters must appear in order. It
ignores case unless you type an uppercase letter. In a tree, directories leading
to a match stay visible. Enter keeps the filter and returns to the list; Escape
clears it.

Names sort in natural order (`file2` before `file10`), directories first.

### Marking

| Key | Action |
|---|---|
| Space | Mark or unmark the entry and move down |
| `v` | Visual mode: mark every entry between here and where the cursor goes; `v` again ends it |
| `*` | Invert the marks of the shown entries |

### Working with the terminal

| Key | Action |
|---|---|
| `i` | Insert the paths, shell-quoted, into the terminal pane (as a paste) |
| `c` | `cd` the terminal pane to this directory. Only works when the pane is at a shell prompt, which needs shell integration ([blocks.md](blocks.md)) |
| `y` | Copy the full paths to the clipboard, one per line |
| `s` | Open the file in your editor in a split to the right |
| `t` | Open the file in your editor in a new tab |
| `f` | Open the file in your editor in a floating pane |

The terminal pane is the focused pane, or if a plugin pane has the focus, the
last terminal pane you used. The picker uses the pane that was focused when it
opened. The editor is `$VISUAL`, else `$EDITOR`, else `vi`. For a directory,
`s`, `t` and `f` open a shell in it instead.

### File operations

| Key | Action |
|---|---|
| `C` | Take the files for copying |
| `x` | Take the files for moving |
| `P` | Paste the taken files here |
| `L` | Create symbolic links here to the taken files |
| `r` | Rename the entry under the cursor |
| `b` | Bulk rename in your editor |
| `n` | New file; end the name with `/` for a directory |
| `M` | Change permissions with `chmod` (for example `644` or `u+x`) |
| `d` | Move to the trash |
| `D` | Delete for good, after you type `yes` |
| `u` | Undo the last rename, bulk rename, move or trash |

"Here" is the directory shown, or in a tree the open directory under the cursor.

**Copy and move.** Paste runs in the background and shows progress in the
status bar. Only one copy or move runs at a time. Cancel it from the command
palette ("plugins: cancel Files: copying" or "...moving"). Copying a file into
its own directory creates `name (1).ext`. When a destination exists, Files
asks: `o` overwrite, `s` skip, `r` rename to a free name; `O`, `S` or `R` apply
the choice to all remaining conflicts, and Escape stops. Overwriting a
directory with a directory merges them. A move cannot be undone if it merged,
replaced or skipped anything, or did not finish.

**New file.** The name may contain subdirectories (`docs/notes.md`); missing
ones are created. The new entry must stay inside the current directory.

**Bulk rename.** The files must be in one directory. Files writes their names,
one per line, to a temporary file in `~/.cache/proteus-files/` and opens your
editor on it in a floating pane. Edit the names, save, and quit. Files then asks
"Rename N files?". The edited file must keep the same number of lines; names
cannot be empty, `.`, `..`, or contain `/` or control characters, and must not
repeat. A new name may not exist already unless it is one of the files being
renamed, so swapping two names works.

**Trash.** Files moves entries to the freedesktop.org trash: the home trash if
the file is on the same file system, else that file system's own trash
directory. Undo restores from `~/.local/share/Trash`.

**Delete.** `D` removes the files and directories with their contents. It
cannot be undone.

## Mouse

The wheel scrolls. A click selects an entry, and a click on the selected entry
opens it. Dragging starts a drag of the marked paths (or the entry under the
cursor).

## Previews

The pane's column layout and the sidebar's floating preview (`p`) show the file
under the cursor: its name, size, modification date (UTC) and MIME type, and
then:

| File | Preview | Needs |
|---|---|---|
| Text | The start of the file with line numbers and syntax highlighting | |
| PNG, JPEG, GIF, WebP | The image | |
| SVG | The rendered image | |
| Video | A thumbnail | `ffmpegthumbnailer` |
| PDF | The first page | `pdftoppm` |
| Archive (zip, tar, gz, xz, 7z, ...) | The list of entries | `bsdtar`, or `unzip` for `.zip` |
| Other binary files | A description | `file` |

Images larger than 48 MiB are not previewed. Video and PDF thumbnails are
stored in the freedesktop.org thumbnail cache, `~/.cache/thumbnails/large/`,
and reused while they are newer than the file.

## Git status

Inside a Git repository, entries get a marker:

| Marker | Meaning |
|---|---|
| `M` (yellow) | Modified |
| `+` (green) | Staged |
| `?` (red) | Untracked |
| `!` (dim) | Ignored |

A directory shows the strongest marker of the files below it. The status is
refreshed after a command finishes in any pane, when files in the shown
directory change, and with `R`. If `git` cannot be run, Git status stays off
until the plugin restarts.

## Settings

Pass settings with `config` in `p.load_plugin`:

```lua
require("proteus").load_plugin {
  source = "/usr/share/proteus/plugins/files",
  config = { position = "right", width = 40 },
}
```

| Key | Values | Default | Meaning |
|---|---|---|---|
| `position` | `"left"`, `"right"` | `"left"` | Side of the window for the sidebar |
| `width` | 1 to 4096 | 32 | Sidebar width in columns |

Any other key, or a value of the wrong type, stops the plugin from starting and
shows an error.

## Permissions

The manifest, `plugins/files/proteus-plugin.toml`, requests:

| Permission | Used for |
|---|---|
| `ui` | Its views, prompts, status bar progress, palette entries, opening files with `xdg-open` |
| `read-panes` | Following the focused pane's directory; choosing the terminal pane |
| `write-pty` | `i` (insert paths) and `c` (`cd`) |
| `open-pane` | Opening your editor or a shell with `s`, `t`, `f`, text files and bulk rename |
| `clipboard-write` | `y` |
| `fs = ["~:rw", "/:r"]` | Reading everything, changing files in your home directory |
| `run-command` | `git`, `zoxide`, `chmod`, `ln`, `file`, `bsdtar`, `unzip`, `ffmpegthumbnailer`, `pdftoppm` |

To grant less, list the permissions yourself; there is then no prompt. Features
whose permission is missing report an error when used. For example, to let
Files change only `~/src` and run only `git`:

```lua
require("proteus").load_plugin {
  source = "/usr/share/proteus/plugins/files",
  permissions = {
    ui = true,
    ["read-panes"] = true,
    ["write-pty"] = true,
    ["clipboard-write"] = true,
    fs = { "/:r", "~/src:rw" },
    ["run-command"] = { "git" },
  },
}
```

Without `open-pane`, Files cannot start your editor. Without a writable grant
for `~/.cache`, thumbnails and bulk rename cannot write their files.

## External programs

Each program is optional; without it the feature that needs it shows an error
or no preview. Programs run with `run-command` are found only in
`/usr/local/bin`, `/usr/bin` and `/bin`, not in your `PATH`. A `zoxide` that
lives in `~/.cargo/bin` or `~/.local/bin` is therefore not found.

| Program | Used for |
|---|---|
| `git` | Status markers |
| `zoxide` | `z` |
| `chmod` | `M` |
| `ln` | `L` |
| `file` | Describing binary files |
| `bsdtar`, `unzip` | Archive listings |
| `ffmpegthumbnailer` | Video thumbnails |
| `pdftoppm` (poppler) | PDF thumbnails |
| `xdg-open` | Opening non-text files (run by Proteus, not the plugin) |
| `$VISUAL` / `$EDITOR` / `vi` | Editing, bulk rename (started in a normal pane through `sh`) |
