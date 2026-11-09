# Scrollback, selection, search and hints

Each pane keeps the lines that scrolled off its top. You can scroll through
them with the mouse or keys, move a keyboard cursor over them in Scroll mode,
select and copy text, search, and pick URLs or paths with hints.

Keys below are the defaults of the `zellij` preset unless noted. Run
`proteus --list-keybindings` to see the keys of your configuration, and see
[keys.md](keys.md) to change them.

## Scrollback size

```lua
require("proteus").config { scrollback = 50000 }
```

`scrollback` is the number of lines kept per pane (default `10000`). Changes
apply to panes opened after the config is loaded. Full-screen programs that
use the alternate screen (editors, `less`, `htop`) do not add to the
scrollback.

## Scroll the view

| Keys | Action |
|------|--------|
| Mouse wheel | Scroll 3 lines per notch |
| `Shift+PgUp` / `Shift+PgDn` | `scroll.page_up` / `scroll.page_down` |
| `Shift+Home` / `Shift+End` | `scroll.top` / `scroll.bottom` |
| `Ctrl+Shift+Z` / `Ctrl+Shift+X` | `scroll.prev_prompt` / `scroll.next_prompt` (needs [shell integration](blocks.md)) |

These keys work in Normal and Locked mode. While a full-screen program runs,
they go to the program instead. `scroll.line_up`, `scroll.line_down`,
`scroll.half_page_up` and `scroll.half_page_down` exist as actions without
default keys.

When a full-screen program has not turned on mouse reporting, the wheel sends
it arrow keys (if it asked for that). Typing anything scrolls the pane back to
the bottom.

## Scroll mode

Scroll mode puts a cursor in the pane so you can move through the scrollback,
select and copy without the mouse. Enter it with:

| Preset | Keys |
|--------|------|
| zellij (default) | `Ctrl+S` |
| leader | `Ctrl+G`, then `s` |
| tmux | `Ctrl+B`, then `[` |

The cursor starts at the terminal cursor, or at the bottom of the view if you
had scrolled back. While the mode is active, the hint bar at the bottom shows
its keys (see `ui.hint_bar` in [configuration.md](configuration.md)).

Leave with `q`, or `Esc` when nothing is selected and no search is shown
(`Ctrl+S` also leaves in the zellij preset). Leaving scrolls back to the
bottom.

### Move

| Keys | Action | Moves to |
|------|--------|----------|
| `h` `j` `k` `l`, arrows | `copy.left` / `copy.down` / `copy.up` / `copy.right` | One cell or line |
| `w` / `b` / `e` | `copy.word_next` / `copy.word_prev` / `copy.word_end` | Next word, previous word, end of word |
| `0`, `^`, `Home` | `copy.line_start` | Start of the line |
| `$`, `End` | `copy.line_end` | Last non-blank character |
| `g` / `G` | `copy.top` / `copy.bottom` | Oldest line / last line |
| `u`, `Ctrl+U` / `d`, `Ctrl+D` | `copy.half_page_up` / `copy.half_page_down` | Half a page |
| `Ctrl+B`, `PgUp` / `Ctrl+F`, `PgDn` | `copy.page_up` / `copy.page_down` | A page |
| `H` / `M` / `L` | `copy.screen_top` / `copy.screen_middle` / `copy.screen_bottom` | Top, middle, bottom of the view |
| `[` / `]` | `copy.prev_prompt` / `copy.next_prompt` | Previous / next prompt |
| `Ctrl+Y` / `Ctrl+E` | `scroll.line_up` / `scroll.line_down` | Scrolls the view, not the cursor |

Paging moves the view with the cursor. Folded command output counts as one row
(see [blocks.md](blocks.md#fold-output)).

### Select and copy

| Keys | Action | What it does |
|------|--------|--------------|
| `v` | `copy.select` | Start or stop a character selection at the cursor |
| `V` | `copy.select_line` | Select whole lines |
| `Ctrl+V` | `copy.select_block` | Select a rectangle |
| `y`, `Enter` | `copy.yank` | Copy the selection, or the cursor's line if nothing is selected; then leave Scroll mode |
| `Esc` | `copy.escape` | Clear the selection; again: hide the search; again: leave Scroll mode |

Pressing a different selection key switches the kind and keeps the start
point. Copied text goes to both the clipboard and the primary selection.

Scroll mode also has keys for command blocks (`c`, `o`, `a`, `C`, `O`, `A`,
`z`, `Z`, `r`, `i`), described in [blocks.md](blocks.md#block-actions), and
`f` for [hints](#hints).

## Search

| Keys | Action |
|------|--------|
| `Ctrl+Shift+F` (zellij and leader presets), `Ctrl+B /` (tmux) | `search.start` from Normal mode; enters Scroll mode |
| `/`, `?`, `s` in Scroll mode | `search.start` |
| `n` | `search.up`: next match towards older lines |
| `N` | `search.down`: next match towards newer lines |

Type the query in the bar at the bottom. Matches are highlighted as you type,
and the nearest match at or above the cursor becomes current. The bar shows
`3/17`, the number of matches, or `no matches`, after flags such as `[regex]`,
`[tab]` and `[output]` for the options that are on.

Keys in the search bar:

| Keys | Effect |
|------|--------|
| `Enter` | Close the bar and select the current match; `y` then copies it |
| `Esc`, `Ctrl+C`, `Ctrl+G` | Cancel the search |
| `Ctrl+R` | Toggle regular expressions |
| `Ctrl+T` | Toggle searching every visible pane of the tab |
| `Ctrl+O` | Toggle searching only command output (needs [shell integration](blocks.md)) |
| `Ctrl+A` / `Ctrl+E`, `Home` / `End` | Start / end of the query |
| `Ctrl+B` / `Ctrl+F`, `Left` / `Right` | Move one character |
| `Ctrl+W` / `Ctrl+U` | Delete a word / everything before the caret |

Case is smart:
the search ignores case unless the query contains an uppercase letter.
Regular expressions use Rust `regex` syntax; an invalid one shows
`invalid regex`. Matches can span wrapped lines.

When the search covers all panes of the tab, `n` and `N` move from pane to
pane and focus the pane of the match. While matches are still highlighted,
starting a search again reopens the bar with the same query and options.
Cancelling the bar or leaving Scroll mode forgets the query; there is no
search history.

Search colors are set with `theme.ui.search_foreground`,
`search_background` and `search_current_background`
([configuration.md](configuration.md)).

## Select with the mouse

| Mouse | Selects |
|-------|---------|
| Drag | Characters |
| `Alt` + drag | A rectangle |
| Double-click | A word |
| Triple-click | A line |

Releasing the button copies the selection to the primary selection. Press
`Ctrl+Shift+C` (action `copy`) to copy it to the clipboard as well.

When a program has turned on mouse reporting, clicks go to the program. Hold
`Shift` to select text anyway.

## Clipboard and primary selection

| Keys | Action |
|------|--------|
| `Ctrl+Shift+C` | `copy`: copy the selection to the clipboard |
| `Ctrl+Shift+V` | `paste`: paste the clipboard |
| `Shift+Insert`, middle click | `paste_primary`: paste the primary selection |

Pasting scrolls the pane to the bottom. Clipboard reads that take longer than
3 seconds or are larger than 16 MiB are dropped whole, so a paste is never cut
short in the middle.

### Paste protection

When the program has not enabled bracketed paste (a plain shell prompt
without it, `cat`, a REPL), a newline in pasted text runs a command. Proteus
then asks first if the text contains a newline or another control character
(tabs are fine):

- `Paste and run this line?` for one line ending in a newline,
- `Paste N lines? Each newline may run a command.`,
- a warning about control characters if there are any.

The first non-empty line is shown. Press `Enter` or `y` to paste, `Esc` or `n`
to cancel.

With bracketed paste on (most modern shells and editors), Proteus does not
ask, and removes control characters from the pasted text so it cannot end the
paste early.

```lua
require("proteus").config { clipboard = { paste_protection = false } }
```

### OSC 52: programs using the clipboard

Programs can set the clipboard with OSC 52, which also works over SSH (for
example from tmux or Neovim on a server). Writes are allowed by default, up to
8 MiB of encoded data.

Reading the clipboard lets a program see whatever you copied, so by default
Proteus asks: `The application in "TITLE" wants to read the clipboard`.

| Answer | Effect |
|--------|--------|
| `y` | Allow this one read |
| `Shift+A` | Allow, and allow every later read from any pane until Proteus exits |
| `n`, `Esc`, `Enter` | Deny; the program gets an empty answer |

`Shift+A` needs Shift on purpose, so typing a lowercase `a` cannot grant it by
accident. Keys pressed in the first half second after the question appears
are ignored. Only one question is shown at a time; requests that arrive while
it is open are dropped.

| Option | Values | Default |
|--------|--------|---------|
| `clipboard.osc52_write` | `true`, `false` | `true` |
| `clipboard.osc52_read` | `"ask"`, `"allow"`, `"deny"` | `"ask"` |
| `clipboard.paste_protection` | `true`, `false` | `true` |

```lua
require("proteus").config {
  clipboard = { osc52_read = "deny", osc52_write = true },
}
```

## Hints

Hints label things on screen so you can pick one with a few keys. Press
`Ctrl+Shift+E` (`hints.open`, all presets) or `f` in Scroll mode. Every visible
pane of the tab gets labels.

Type a label to pick its item. What happens depends on the modifier you hold
on the last key of the label:

| Last key | Effect |
|----------|--------|
| Plain | The action that started hint mode (Open for `hints.open`) |
| With `Shift` | Copy the text to the clipboard and primary selection |
| With `Ctrl` | Insert the text at the prompt of the focused pane |

`Backspace` removes a typed character; `Esc`, `Ctrl+C` or `Ctrl+G` leave hint
mode. Because `Ctrl+G` and `Ctrl+C` always cancel, use `hints.insert` for
labels that end in `g` or `c`.

`hints.copy` and `hints.insert` start hint mode with Copy or Insert as the
plain-key action. They have no default keys:

```lua
require("proteus").config {
  keys = { normal = { ["ctrl+shift+y"] = "hints.copy", ["ctrl+shift+i"] = "hints.insert" } },
}
```

### What gets a hint

Custom patterns are tried first, then the built-in ones, in this order. When
matches overlap, the earlier pattern wins.

| Kind | Example | Open does |
|------|---------|-----------|
| URL with a scheme (`http`, `https`, `ftp`, `file`, `ssh`, `git`) | `https://example.com/a` | `xdg-open URL` |
| URL without a scheme | `www.rust-lang.org/learn`, `github.com/a/b`, `localhost:8080`, `10.0.0.1:3000` | Opens with `https://` (`http://` for localhost and IP addresses) |
| UUID | `123e4567-e89b-12d3-a456-426614174000` | Copies |
| IPv4 address | `192.168.1.10` | Copies |
| IPv6 address | `fe80::1` | Copies |
| Hex color | `#ff8800` | Copies |
| Path | `src/main.rs:12:5`, `./build`, `~/notes/todo.md` | See below |
| Git hash | `3f2a9c1` (7 to 40 hex digits, with a digit and a letter) | Copies |

Bare host names are only treated as URLs for common top-level domains, and
not for ones that look like file extensions, so `build.sh` stays a path.
Trailing punctuation and unbalanced closing brackets are left out of URLs.

Opening a **path**:

- Relative paths are resolved against the directory the command ran in (from
  OSC 7, see [blocks.md](blocks.md)), else the pane's directory. `~/` means
  your home directory.
- `file:LINE` or `file:LINE:COL` opens `$VISUAL` (or `$EDITOR`, or `vi`) at
  that line in a new pane. Helix, VS Code (`code`, `codium`, `code-oss`) and
  nano get the column too; other editors get `+LINE`.
- Other paths are opened with `xdg-open`.
- A path that does not exist shows `no such file`.

**Insert** puts the text into the focused pane as a paste. Paths, URLs and
custom matches are quoted for the shell when needed (`'my file'`).

### Custom hint patterns

```lua
require("proteus").config {
  hints = {
    alphabet = "asdfjkl",
    patterns = {
      { name = "jira", regex = "\\b[A-Z][A-Z0-9]+-\\d+\\b", url = "https://jira.example.com/browse/{}" },
      { name = "pr", regex = "PR #(\\d+)", url = "https://github.com/me/repo/pull/{}" },
      { name = "sha256", regex = "\\b[0-9a-f]{64}\\b" },
    },
  },
}
```

| Field | Meaning |
|-------|---------|
| `regex` | Required. Rust `regex` syntax. If it has a capture group, group 1 is the hint text. |
| `name` | Optional name (default `patternN`). |
| `url` | Optional. Open replaces `{}` with the URL-encoded text and opens the result. Without `url`, Open copies. |

`hints.alphabet` sets the label characters: at least 2 different lowercase
letters or digits (default `asdfghjkl`). All labels on screen have the same
length.

## Export the scrollback

Run **Export scrollback** (`scrollback.export`) from the palette
(`Ctrl+Shift+P`). Enter a destination path; the focused pane's scrollback and
screen are written there as plain text, with wrapped lines joined. A
full-screen program's alternate screen is not included. An existing file is
never overwritten. `~` is not expanded, so give a full path.

## Images in the scrollback

Images scroll with the text. Ctrl+click one to open it in the image viewer;
see [images.md](images.md#view-an-image).
