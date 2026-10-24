# Feature opportunities after persistence hardening

Assessment: 2026-10-24. These are feature proposals. The Settings UI was removed by request; Actions retains completion and a config editor action.

Proteus already has tabs, splits, floats/stacks, multiple windows, saved layouts,
session recovery, command blocks, search, a palette, inline images, Lua UI and
WASM plugins, and a substantial file manager. The best additions should connect
those capabilities into everyday workflows. Packaging, compatibility testing,
and performance validation remain release requirements independent of this list.

| Priority | Addition | First useful version | Why it fits |
|---|---|---|---|
| 2 | Project task runner | Explicitly run named build/test/dev tasks in dedicated panes; show running/success/failure and jump to output | Reuses panes, command blocks, layouts, and the plugin UI |
| 3 | Workspace launcher | Browse projects and named layouts; open a project's terminal/file-manager/task arrangement with one action | Extends existing saved layouts into a repeatable project workflow |
| 4 | Optional failed-command explanation | Explicitly send one failed block to a local or hosted model; preview/redact context, then insert a proposed fix for editing | Delivers one clear slice of the AI roadmap without first building a full chat product |
| 5 | Command favorites and history search | Explicitly save commands with project/cwd, search favorites across panes, insert into the prompt | Uses shell integration and the now-transactional storage layer |
| 6 | Plugin inspection UI | Show loaded/failed instances, effective capabilities, source/revision/checksum, and restart/config links | Makes plugin behavior understandable; current loading and permission configuration primarily live in Lua |
| Later | SSH integration assistant and SFTP browser | Explicit remote terminfo/shell-integration setup first; remote file browsing as a separate plugin milestone | Helps the Linux developer/SSH target audience, but adds authentication and remote-state complexity |

## Strongest workflow addition: project tasks

A small first-party WASM plugin could expose tasks such as “test”, “build”, and
“dev server”, with a pane for each run and a task list showing status and time.
Selecting a failed task focuses its pane and command block. Rerun and stop are
explicit actions. Use OSC 133 where available, and identify direct child process
exit independently when shell integration is unavailable.

Project task definitions should be declarative data. Opening an unfamiliar
repository must not execute its Lua, shell commands, dependency hooks, or tasks.
Explain that manually running a task grants normal shell privileges. Start with
explicit configuration and manual runs; defer file-watch reruns and task DAGs.

## Workspace launcher: extend what already exists

The current palette already lists saved layouts and panes; another generic
switcher would duplicate it. Add project identity, pinned/recent projects, a
layout preview, and project-specific task/file-manager defaults. Start by opening
project windows while keeping existing windows alive. Per-project autosave needs
separate state names and locking, rather than reusing the single global session
snapshot. Do not imply that restoring a layout restores live processes.

## AI: one bounded feature before a provider ecosystem

The AI/provider plugins described in `design/06-ai.md` are still a roadmap. A
failed-command explanation is a smaller end-to-end slice than full chat, natural
language command generation, context indexing, and multiple provider plugins at
once. Make it optional and user-triggered, show the exact outgoing context,
exclude private panes, support cancellation, and insert suggested commands for
editing rather than executing them. A local endpoint is a useful first backend;
the current networking and Secret Service capabilities provide groundwork.

## History must be an explicit choice

Start with manually saved favorites; persistent automatic history should be
opt-in. Apply retention/clear controls, exclude private panes and password input,
and avoid saving output by default. Redaction can reduce exposure but cannot
guarantee arbitrary shell commands contain no secrets. Keep the existing
ephemeral command blocks useful when history persistence is off.

## Features to defer

- Detachable sessions require a durable PTY/server ownership architecture; layout
  snapshots are not a substitute. This remains a separate v2 project.
- Cross-platform ports would broaden the compositor, input, rendering, and
  packaging matrix substantially. Validate the intended Wayland scope first.
- A plugin marketplace needs artifact trust, version/ABI compatibility, updates,
  rollback, and support policy. An inspection UI is a useful smaller precursor.
- AI autonomous execution, automatic repository scripts, and silent background
  context collection are unnecessary for the initial workflows above.

Suggested order: project tasks, workspace launcher,
then the optional AI explanation slice. Build these through public Lua/WASM APIs
where practical; extend the host only for capabilities the APIs genuinely lack.
