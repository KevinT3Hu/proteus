# SSH

Proteus uses the installed OpenSSH `ssh` and `sftp` executables. Keys, agents,
jump hosts, proxies, host-key checks, and multiplexing follow your OpenSSH
configuration. Authentication and host-key questions appear in the session's
terminal pane. Proteus does not store passwords or private keys.

## SSH actions

All SSH features are actions. Run them from the palette (Ctrl+Shift+P; type
`ssh` and press Tab to list them) or bind them to keys. Every action except the
two connect actions needs a focused SSH pane.

| Palette entry                           | Action name          |
|-----------------------------------------|----------------------|
| SSH: Connect to destination…            | `ssh.connect`        |
| SSH: Connect with Bash integration…     | `ssh.connect_bash`   |
| SSH: Reconnect / run again              | `ssh.reconnect`      |
| SSH: Load / refresh remote workspace    | `ssh.workspace`      |
| SSH: Open SFTP                          | `ssh.sftp`           |
| SSH: Upload file…                       | `ssh.upload`         |
| SSH: Download file…                     | `ssh.download`       |
| SSH: Add local port forward…            | `ssh.forward_local`  |
| SSH: Add remote port forward…           | `ssh.forward_remote` |
| SSH: Stop session                       | `ssh.stop`           |

None of them has a default key. To bind one (see [keys.md](keys.md)):

```lua
local p = require("proteus")
p.config { keys = { normal = { ["ctrl+alt+s"] = "ssh.connect" } } }
```

## Connect

Open the palette and select a discovered `SSH: Connect to HOST` entry, or
**SSH: Connect to destination…** and enter an alias or `user@host`. The session
opens in a new pane in the current tab. Configure ports and other connection
settings in `~/.ssh/config`. Literal IPv6 addresses are accepted; SSH URIs
(`ssh://...`), option strings, and destinations starting with `-` are not.

Suggestions come from `Host` lines in `~/.ssh/config`, `/etc/ssh/ssh_config`,
and the files they `Include`. Discovery handles quoted words, comments, and
Include globs. Relative Include paths resolve against `~/.ssh` for your file and
`/etc/ssh` for the system file. It skips Includes that use `%` tokens, `$`
variables, or `~user`. Wildcard and negated patterns are not listed. Host lines
inside `Match` blocks are listed even though OpenSSH may not apply them.
Discovery stops at 128 files, 16 levels of Include, 1 MiB of config text, or
4096 directory entries. Problems appear as `SSH discovery:` entries in the
palette. A missing suggestion never prevents connecting manually.

Discovery is an index of names, not a configuration resolver. It never runs
`ssh`, never evaluates `Match exec`, and passes the destination to OpenSSH
unchanged.

## Remote shell integration

**SSH: Connect with Bash integration…** starts remote Bash instead of the
server's default shell. Bash 4.4 or later provides command blocks with exit
status, and reports the remote hostname and current directory. Older Bash
starts without these. Bash reads `/etc/bash.bashrc` and `~/.bashrc` as usual,
then loads the Proteus hooks from an anonymous pipe. Nothing is installed on
the server and no startup file is edited.

The command is passed through the server's login shell to `sh`; this works with
sh, bash, zsh, fish, and csh/tcsh login shells. The server needs `sh`, `base64`,
and `bash`. Restricted accounts that only allow a fixed command cannot use this
mode. Plain connections need nothing on the server.

SSH shell panes set `TERM=xterm-256color`, so the server does not need a
Proteus terminfo entry. Pane titles read `SSH DESTINATION · running`, or
`SSH DESTINATION · Bash · running` for integrated sessions, followed by
`· HOST:PATH` once the remote shell reports its location. `running` means the
local `ssh` process is alive, including while it waits for authentication.

Remote directories are never used as local working directories. A new pane
split from an SSH pane starts locally, as usual.

## Images in SSH panes

SSH panes always count as remote. Programs in them cannot show images by
naming a local file, temporary file, or shared-memory object, because those
names would refer to files on your machine. Images sent as inline data still
work. See [images.md](images.md).

## Reconnect and stop

When an SSH session, SFTP client, transfer, or forward exits, its pane keeps
its scrollback and prints the exit status. The title changes to `... · ended
(STATUS)`. Focus the pane and choose **SSH: Reconnect / run again** to start the
same connection in a new pane. For remote tasks and transfers this repeats the
operation. The old pane stays until you close it.

**SSH: Stop session** hangs up the focused SSH process. Close the pane as
usual to remove it.

Reconnecting does not recover programs that were running on the server. Use
tmux or a similar tool on the server if you need that. SSH panes are not
reconnected when Proteus restarts or restores a session.

## Remote workspaces and tasks

Remote projects can use the same `.proteus-workspace` files as local ones (see
[workspaces.md](workspaces.md)).

1. Focus an SSH shell pane and choose **SSH: Load / refresh remote workspace**.
2. Open the palette again. While the request runs it shows **SSH: Loading
   remote workspace…**; then `Remote task: NAME · WORKSPACE · DESTINATION ·
   COMMAND` entries appear.
3. Select a remote task. It opens in a new integrated Bash pane on the same
   destination.

Loading runs a small read-only Python helper over a separate SSH connection. It
needs `python3` and `base64` on the server, and authentication must work
without prompting: keys without a passphrase, an agent, or an existing OpenSSH
multiplexed connection. The connection times out after 10 seconds and
the whole request after 20 seconds. At most four requests run at once, one per
pane. If loading fails, the palette shows an `SSH workspace:` entry with the
reason.

With shell integration, the search starts at the remote shell's last reported
directory; otherwise at the remote home directory. It walks up to the nearest
trusted `.proteus-workspace`, using the same trust rules as local discovery
([workspaces.md](workspaces.md#why-isnt-my-workspace-picked-up)): files and
directories that other users could change are skipped, and a symlinked
definition is an error. The private-group check reads the server's
`/etc/passwd` and `/etc/group`.

The `rust` and `node` presets and task overrides work as locally. Loading reads
the definition, `package.json`, and which lockfiles exist; it never runs task
commands. The result is a snapshot: load again to pick up changes.

Remote task `cwd` values must be relative and must not contain `..`. Just
before a task starts, the server resolves symlinks and checks that the
directory exists and is inside the workspace; otherwise the task does not run.
Running a task also needs `python3` and `bash` on the server. The task pane
asks for authentication like any SSH session. After the command finishes, the
pane stays at an interactive remote Bash prompt with the output intact. Ctrl+C
interrupts the command.

Local tasks are not listed while an SSH pane is focused.

## Transfer files

Focus an SSH pane and choose **SSH: Upload file…** or **SSH: Download file…**.
Enter the full local path first, then the remote path. For an upload the local
file must exist; for a download the local path must not be a directory. SFTP
runs the transfer in a new pane that shows its output, errors, and the result.

Remote relative paths are relative to SFTP's starting directory, usually your
home directory, not the shell pane's directory. Existing files at the
destination may be overwritten. Paths are taken literally, including spaces,
quotes, and glob characters; local paths must be UTF-8. Each transfer is one
file and is listed in a private batch file that is deleted when the process
ends or its pane closes. An interrupted transfer can leave a partial file;
reconnecting starts the transfer again from the beginning.

For directories, recursive copies, or browsing, use **SSH: Open SFTP**, which
opens the interactive `sftp` client with `put`, `get`, and its other commands.
The server must provide the SFTP subsystem.

## Port forwarding

Focus an SSH pane and choose **SSH: Add local port forward…** or **SSH: Add
remote port forward…**, then enter `[bind_address:]port:host:hostport` in
OpenSSH syntax, with brackets for IPv6 addresses where needed. Examples:

```text
8080:localhost:80
127.0.0.1:5433:db.internal:5432
```

Each forward runs as `ssh -N -T -o ExitOnForwardFailure=yes` in its own pane,
whose title starts with `SSH DESTINATION · -L SPEC` (or `-R`). Authentication happens
in that pane. Stop or close the pane to end the forward. These panes disable
connection multiplexing for their own process, so closing the pane removes the
forward instead of leaving it on a shared master connection. All other settings
come from your OpenSSH configuration, and binding rules follow OpenSSH and the
server.
