# Project workspaces and tasks

A workspace is a TOML file named `.proteus-workspace` at the root of a project.
It gives the project a name and a list of tasks: commands you start from the
palette instead of typing them. Git is not required; the directory containing
the file is the workspace root.

```toml
name = "Proteus"
presets = ["rust"]

[tasks.test]
name = "Test workspace"
command = ["cargo", "test", "--workspace"]

[tasks.format]
disabled = true

[tasks.plugin-tests]
command = ["cargo", "test"]
cwd = "plugins/files"
```

Workspace definitions live only in project files. Your personal Lua
configuration does not contain them.

## Run a task

1. Focus a pane whose current directory is inside the project.
2. Open the palette (Ctrl+Shift+P), type `task`, and press Tab to show the
   matching entries.
3. Select an entry and press Enter.

Entries read `Task: NAME · WORKSPACE · COMMAND`. The task opens in a new pane
in the current tab, named `Task: NAME`. The pane starts your normal
shell in the task's working directory and types the command into it once the
shell is ready, the same way layout `run` commands work.

After the command finishes, the pane stays open at a shell prompt with the
output intact. Use command blocks ([blocks.md](blocks.md)) to see the exit
status and to copy or search the output. Ctrl+C interrupts the task like any
foreground command. Selecting the task again opens another pane; Proteus does
not track or rerun earlier runs.

Commands are quoted for POSIX-style shells and fish. A custom pane command that
is not an interactive shell, or a shell with incompatible quoting, cannot run
tasks. Exit status in command blocks needs shell integration; without it the
output still appears, but no structured status is recorded.

## Edit the definition

When a workspace is found, the palette also lists `Workspace: NAME · Edit
definition`. It opens `.proteus-workspace` in a new pane using `$VISUAL`, else
`$EDITOR`, else `vi`.

## How the workspace is found

Each time you open the palette, Proteus looks for `.proteus-workspace` starting
at the focused pane's current directory and then in each parent directory. The
first trusted file wins. The pane's directory comes from shell integration when
it reports a local path, otherwise from the foreground process; if neither is
known, Proteus uses its own working directory. Changes to the file or to
`package.json` show up the next time you open the palette.

SSH panes never show local tasks. Use remote workspaces instead (see
[ssh.md](ssh.md#remote-workspaces-and-tasks)).

If the nearest trusted file is invalid, the palette shows a `Workspace error:`
entry with the reason. Proteus does not fall back to a workspace further up in
that case. Without any workspace, the palette works normally and has no task
entries.

### Why isn't my workspace picked up?

Tasks run commands with your privileges, so Proteus ignores definitions that
someone else could have written or replaced. A file is skipped silently, and
the search continues in the parent directory, when any of these is true:

- The file is owned by a user other than you or root.
- The file is world-writable.
- The file is group-writable, unless you own it and its group is your private
  group.
- The directory containing it can be changed by others: it is owned by someone
  other than you or root, or it is group- or world-writable without the sticky
  bit. A group-writable directory is fine if you own it and its group is your
  private group. A sticky directory owned by you or root (such as `/tmp`) is
  fine.

Your private group is a group named after you, with no other members, that is
your primary group and no other user's. Proteus reads this from `/etc/passwd`
and `/etc/group`. This lets setups with umask 002 keep group-writable files.

To fix a skipped file, check ownership and modes:

```sh
ls -ld . .proteus-workspace
chmod go-w .proteus-workspace
```

A `.proteus-workspace` that is a symlink is never followed. It produces a
`Workspace error:` entry instead of being skipped. Other errors opening the
file, such as permission denied, are reported the same way.

## File format

### Top-level fields

| Field     | Type            | Default                  | Meaning                                    |
|-----------|-----------------|--------------------------|--------------------------------------------|
| `name`    | string          | name of the root folder  | Shown in palette entries.                  |
| `presets` | list of strings | `[]`                     | Built-in task sets: `"rust"`, `"node"`.    |
| `tasks`   | table           | none                     | Tasks keyed by ID, written `[tasks.ID]`.   |

### Task fields

| Field      | Type            | Default            | Meaning                                                  |
|------------|-----------------|--------------------|----------------------------------------------------------|
| `command`  | list of strings | required           | Program and arguments. Not shell code.                   |
| `name`     | string          | the task ID        | Label in the palette and the pane name.                  |
| `cwd`      | string          | the workspace root | Working directory, relative to the root.                 |
| `disabled` | boolean         | `false`            | `true` removes the task, including a preset task.        |

`command` is an argument list, so shell syntax such as `&&`, pipes, or `$VAR`
is not interpreted. To use it, run a shell explicitly:

```toml
[tasks.check-all]
command = ["sh", "-c", "cargo fmt --check && cargo clippy"]
```

Tasks have no environment field. They inherit the environment of a new shell
pane. Unknown fields and unknown presets are errors, so `env = ...` makes the
whole file invalid.

`cwd` must be relative, must exist as a directory, and must stay inside the
workspace root after symlinks are resolved. An invalid `cwd` on any task makes
the whole workspace show an error.

### Limits

- The definition file and `package.json` are each at most 256 KiB and must be
  regular files.
- At most 256 tasks after presets are merged, counting disabled entries.
- A command has at most 128 arguments, each at most 16 KiB; the first must be
  nonempty.
- Names and task IDs are nonempty, at most 1024 bytes, and contain no control
  characters. Arguments contain no control characters.

## Presets

Presets add tasks with fixed IDs. They apply in the order listed; if two define
the same ID, the later one wins.

### `rust`

| Task ID  | Command        |
|----------|----------------|
| `build`  | `cargo build`  |
| `test`   | `cargo test`   |
| `check`  | `cargo check`  |
| `format` | `cargo fmt`    |
| `lint`   | `cargo clippy` |

### `node`

Reads `package.json` in the workspace root (it must exist) and adds one task per
entry in its `scripts` object. The task ID is the script name and the command is
`MANAGER run NAME`. The package manager is chosen from lockfiles in the root:

| Lockfile present           | Manager |
|----------------------------|---------|
| `pnpm-lock.yaml`           | `pnpm`  |
| `yarn.lock`                | `yarn`  |
| `bun.lock` or `bun.lockb`  | `bun`   |
| none of these              | `npm`   |

The first match in this order is used. Detection only checks that files exist;
nothing is installed. Script names starting with `-`, and scripts whose value is
not a string, make the workspace invalid.

### Overriding preset tasks

A `[tasks.ID]` entry replaces the preset task with that ID entirely. It does not
merge fields, so it needs its own `command`:

```toml
presets = ["node"]

[tasks.dev]
name = "Dev server on port 4000"
command = ["pnpm", "run", "dev", "--", "--port", "4000"]

[tasks.lint]
disabled = true
```

## What discovery does and does not run

Finding a workspace and building the task list only reads files. No command,
package lifecycle hook, or repository script runs until you select a task.
Selected tasks run with your normal user privileges, so review the task
commands and `package.json` scripts of an unfamiliar project before running
them.
