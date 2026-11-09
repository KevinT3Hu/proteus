# Shell integration and command blocks

With shell integration, Proteus knows where each prompt, command and its output
start and end. It groups them into **command blocks**. Blocks let you jump
between prompts, fold long output, copy one command's output, run a command
again, see exit status and duration, and get a desktop notification when a long
command finishes in a pane you are not looking at.

Shell integration is on by default for bash, zsh and fish started by Proteus.
Other shells and remote hosts can send the same escape sequences themselves.

## How integration is set up

When a pane starts bash, zsh or fish (from `shell.command`, the command after
`--`, or `$SHELL`), Proteus writes its integration scripts to
`$XDG_RUNTIME_DIR/proteus/shell-integration/` (or `/tmp/proteus-UID/shell-integration/`
when `XDG_RUNTIME_DIR` is unset) and makes the shell load them. Your own
startup files still run as usual.

| Shell | How the script is loaded | Notes |
|-------|--------------------------|-------|
| bash  | Started as `bash --posix` with `ENV` pointing at the script, which turns POSIX mode off again and sources `/etc/bash.bashrc` and `~/.bashrc` (or the profile files for a login shell). | Needs bash 4.4 or later. Only when bash has no arguments other than `-i`; `bash -l` or `bash --norc` are started unchanged. Works alongside bash-preexec. |
| zsh   | `ZDOTDIR` points at Proteus's `.zshenv`, which restores your `ZDOTDIR`, runs your `.zshenv`, then loads the hooks. | Hooks are registered at the first prompt, after everything in `.zshrc`. |
| fish  | Proteus's directory is prepended to `XDG_DATA_DIRS`, so fish finds `vendor_conf.d/proteus.fish`. The script removes that entry again. | `fish_prompt` is wrapped at the first prompt. |

Inside an integrated pane, `PROTEUS_SHELL_INTEGRATION_DIR` holds the directory
the scripts were loaded from.

To turn automatic integration off:

```lua
require("proteus").config { shell = { integration = false } }
```

The setting applies to panes opened after the config is loaded.

## Install the scripts manually

Load a script yourself when Proteus does not start the shell directly, for
example:

- a shell on a remote host reached with plain `ssh`,
- bash started with options (`shell.command = { "bash", "-l" }`),
- shells inside tmux or screen,
- `shell.integration = false`, but you still want blocks in some shells.

The scripts are in the source tree under `crates/proteus-pty/shell-integration/`.
From an integrated local pane you can also copy them out of
`$PROTEUS_SHELL_INTEGRATION_DIR`:

```sh
scp "$PROTEUS_SHELL_INTEGRATION_DIR/bash/proteus.bash" myhost:.proteus.bash
```

| Shell | File | Add this |
|-------|------|----------|
| bash  | `bash/proteus.bash` | `source ~/.proteus.bash` at the end of `~/.bashrc` |
| zsh   | `zsh/proteus.zsh` | `source ~/.proteus.zsh` in `~/.zshrc` |
| fish  | `fish/vendor_conf.d/proteus.fish` | copy it to `~/.config/fish/conf.d/proteus.fish` |

Each script only acts in interactive shells and loads itself once, so it is
safe to source it in a shell that Proteus has already set up. The bash script
needs `shopt -s promptvars` (the bash default) to mark where output starts.

For remote bash there is also **SSH: Connect with Bash integration**, which
loads the hooks without installing anything on the server. See [ssh.md](ssh.md).

## Sequences for other shells

Any shell or prompt framework can produce blocks by printing these sequences
(`ESC` is `\e`, `BEL` is `\a`; `ESC \` also ends a sequence):

| Sequence | When to send it |
|----------|-----------------|
| `OSC 133 ; A BEL` | A prompt starts. |
| `OSC 133 ; B BEL` | The prompt ends and the command line starts. |
| `OSC 133 ; C BEL` | The command runs; output follows. May carry the command line as `;cmdline_url=PERCENT-ENCODED` or `;cmdline=TEXT`. |
| `OSC 133 ; D ; EXIT BEL` | The command finished with exit status `EXIT`. |
| `OSC 7 ; file://HOST/PATH BEL` | The working directory changed (`PATH` percent-encoded). `kitty-shell-cwd://` URLs are also accepted. |

The same marks with `633` instead of `133` (VS Code) are also understood,
including `OSC 633 ; E ; COMMANDLINE` for the command text.

If `C` carries no command line, Proteus reads the command back from the screen
between the `B` and `C` marks. A prompt that arrives without a `D` ends the
previous block with an unknown exit status. Marks sent while a full-screen
program uses the alternate screen are ignored.

The working directory from OSC 7 is used for new panes, for opening relative
paths with hints ([scrollback.md](scrollback.md#hints)), and to tell when a
pane shows another machine (see [images.md](images.md#local-files-and-remote-panes)).

## What a block shows

Each pane has a narrow gutter on its left (in the window padding or the gap
between panes). Next to every prompt and command, the gutter shows a mark:

| Mark | Meaning |
|------|---------|
| Dim | The command finished with exit status 0, or its status is unknown. |
| Red | The command failed (non-zero exit status). Its output rows get a thin red mark too. |
| Accent color, moving segment | The command is still running. |

Hover over a mark to see a summary such as `✗ exit 2 · 1.3s · make test`.
Durations are shown as `1.2s`, `42s`, `3m 05s` or `2h 01m`.

When a command's output fills the pane so that its prompt has scrolled out of
view, the command is pinned in the first row of the pane, with its status on
the right (`running · 4.2s`, `✗ exit 2 · 1.3s`). Click the pinned row to scroll
back to the prompt.

| Option | Default | Effect |
|--------|---------|--------|
| `ui.gutter` | `0.5` | Gutter width in cells, from 0 to 2. `0` hides the gutter. |
| `ui.sticky_command` | `true` | Pin the command of the output at the top of the pane. |

```lua
require("proteus").config { ui = { gutter = 1, sticky_command = false } }
```

Gutter marks and the pinned command are not drawn while a program uses the
alternate screen.

## Jump between prompts

| Keys | Where | Action |
|------|-------|--------|
| `Ctrl+Shift+Z` | Normal and Locked mode | `scroll.prev_prompt`: scroll the previous prompt to the top |
| `Ctrl+Shift+X` | Normal and Locked mode | `scroll.next_prompt`: scroll the next prompt to the top |
| `[` / `]` | Scroll mode | `copy.prev_prompt` / `copy.next_prompt`: move the cursor to the previous / next prompt |

Keys in this file are the defaults of the `zellij` preset. Scroll mode is
described in [scrollback.md](scrollback.md#scroll-mode). Run
`proteus --list-keybindings` to see the keys of your configuration, and
[keys.md](keys.md) to change them.

## Block actions

These actions work on the block under the Scroll mode cursor. Outside Scroll
mode they work on the most recent command of the focused pane.

| Scroll mode key | Action | What it does |
|-----------------|--------|--------------|
| `c` | `block.copy_command` | Copy the command line to the clipboard and the primary selection. |
| `o` | `block.copy_output` | Copy the output. |
| `a` | `block.copy_all` | Copy prompt, command and output. |
| `C` | `block.select_command` | Select the command; move to extend, `y` copies. |
| `O` | `block.select_output` | Select the output. |
| `A` | `block.select_all` | Select the whole block. |
| `z` | `block.fold` | Fold or unfold the output. |
| `Z` | `block.unfold_all` | Unfold every block in the pane. |
| `r` | `block.rerun` | Run the command again. |
| `i` | `block.info` | Show status, duration, directory and number of output lines. |

None of them has a key outside Scroll mode by default. Bind them, or run them
from the palette (`Ctrl+Shift+P`):

```lua
require("proteus").config {
  keys = {
    normal = {
      ["alt+r"] = "block.rerun",
      ["alt+o"] = "block.copy_output",
      ["alt+z"] = "block.fold",
    },
  },
}
```

## Fold output

`z` in Scroll mode, or a click on a block's gutter mark, folds a finished
command's output into one row that reads `▸ 1,234 lines`. Click that row or the
mark again, or press `z` on it, to unfold. `Z` unfolds everything in the pane.
Running commands and commands without output cannot be folded. A search that
finds a match inside folded output unfolds it.

## Run a command again

`block.rerun` clears whatever is typed at the prompt (it sends `Ctrl+U`),
pastes the block's command and presses Enter. It leaves Scroll mode and scrolls
to the bottom.

It does nothing when:

- the command contains control characters, which includes newlines (a
  multi-line command could run differently than it looks),
- the block has no recorded command,
- a program other than the shell is running in the pane. You see a notice
  naming that program.

## Exit status and failures

A command's exit status comes from the `D` mark. Failed commands get a red
mark and the hover summary `✗ exit N`. When a new prompt appears without a `D`
mark (for example a nested shell's prompt), the status is unknown and the block
is not counted as failed.

## Notifications for long commands

When a command that ran for at least `notify.min_duration` seconds finishes,
and you are not looking at its pane (its window is unfocused, or another tab or
pane has the focus), Proteus sends a desktop notification. The title is
`Command finished` or `Command failed (exit N)`; the body is the first line of
the command and `took 3m 05s`.

Click the notification to bring that pane to the front: Proteus switches to
its tab, focuses it and asks the compositor to raise its window.

Programs can also send their own notifications with OSC 9 or OSC 777
(`OSC 777 ; notify ; TITLE ; BODY`). At most 5 per pane are shown in any
10 seconds. Clicking them also focuses the pane.

| Option | Default | Effect |
|--------|---------|--------|
| `notify.min_duration` | `10` | Seconds a command must run before its end is notified. `false` turns these notifications off. |
| `notify.osc` | `true` | Show OSC 9 / OSC 777 notifications from programs. |

```lua
require("proteus").config { notify = { min_duration = 30 } }
```

Notifications need a notification daemon on the D-Bus session bus
(`org.freedesktop.Notifications`). If none is available, Proteus prints one
error on its standard error and continues.

## Use blocks from Lua

The config can react to commands and read blocks. See [scripting.md](scripting.md)
for the full API.

```lua
local p = require("proteus")

-- A message in the window when a command fails.
p.on("command_finished", function(ev)
  if ev.exit_code ~= nil and ev.exit_code ~= 0 then
    p.notify("failed: " .. (ev.command or "?"), { level = "error" })
  end
end)

-- Alt+C: show the last command of the focused pane.
p.keymap("normal", "alt+c", function(ctx)
  local blocks = ctx.pane:read_blocks()
  local last = blocks[#blocks]
  if last then
    p.notify(ctx.pane:block_text(last.id, "command") or "")
  end
end)
```

`command_finished` gives `command`, `exit_code` and `duration` (seconds);
`exit_code` is `nil` when unknown. `pane:read_blocks()` returns blocks oldest
first, and `pane:block_text(id, part)` returns `"command"`, `"output"` (the
default) or `"all"`.

## Troubleshooting

- **No gutter marks.** Check that the pane runs bash (4.4 or later), zsh or
  fish without extra bash arguments, or that your shell loads a script
  manually. `echo $PROTEUS_SHELL_INTEGRATION_DIR` is empty when Proteus did
  not set up the shell. Also check `ui.gutter` is not `0`.
- **Prompt themes.** The scripts re-add their marks at every prompt, so
  frameworks that rebuild `PS1` or `fish_prompt` keep working.
- **No notification.** It is only sent when the pane is not watched and the
  command ran at least `notify.min_duration` seconds. A command without a
  `C` mark has no duration.
