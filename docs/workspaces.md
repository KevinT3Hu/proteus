# Project workspaces and tasks

Place a TOML file named `.proteus-workspace` at the project root:

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

Open Actions (Ctrl+Shift+P), type `task`, and press Tab. Tasks include their
workspace name and command. Enter runs the selected task in a new named terminal
pane. Output remains in that shell after completion. Use the normal terminal
command blocks to inspect exit status and copy/search output; Ctrl+C interrupts
the foreground task. Selecting a task again explicitly opens another run.

Discovery starts at the focused pane's current directory and walks upward to the
nearest definition. The file's directory is the root; Git is not required. Each
opening of Actions refreshes the definition and presets. A malformed nearest
file produces an error entry instead of silently falling back to an ancestor.
Actions also offers `Workspace: … · Edit definition`, using the configured editor.
Without a definition, Actions works normally and has no project task entries.

## Built-in presets

- `rust`: `build`, `test`, `check`, `format`, and `lint`, invoking `cargo build`,
  `cargo test`, `cargo check`, `cargo fmt`, and `cargo clippy` respectively.
- `node`: one task per existing `package.json` script. Runs `pnpm run NAME` when
  `pnpm-lock.yaml` exists, otherwise Yarn for `yarn.lock`, Bun for `bun.lock` or
  `bun.lockb`, and npm otherwise. Lockfile detection never installs dependencies.

Presets apply in listed order; later presets win ID collisions. Project tasks
replace entire preset entries by ID. `disabled = true` removes a task. Otherwise
`command` is a required nonempty argv array, `name` defaults to the task ID, and
`cwd` defaults to the workspace root. Working directories must exist and remain
inside the root, including after symlink resolution. Environment is inherited
from the terminal shell; per-task environment overrides are not implemented.

Commands are quoted and submitted to a fresh interactive shell, using the same
mechanism as layout `run` commands. This supports conventional POSIX-style shells
and fish; custom non-shell pane commands and shells with incompatible quoting
are not supported for task execution. Shell integration supplies command status;
without it, output still works but structured completion status is unavailable.

Discovery reads declarative data only. Commands, package lifecycle hooks, and
repository scripts run with normal user privileges only after selecting a task.
Review task commands and package scripts before running an unfamiliar project.
Definition/package files are bounded to 256 KiB, must be regular non-symlink files,
and workspaces are limited to 256 tasks. Unknown fields and presets are errors.

This first version provides task discovery and execution. Workspace creation,
recent-project launching, workspace-specific layout restoration, task dependency
graphs, automatic reruns, and a dedicated run-history/status UI remain future work.
Personal Lua configuration does not contain workspace definitions.
