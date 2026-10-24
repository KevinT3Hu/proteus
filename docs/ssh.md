# SSH

Proteus uses the installed OpenSSH `ssh` and `sftp` executables. Keys, agents,
jump hosts, proxies, host-key checks, and multiplexing follow your OpenSSH
configuration. Authentication and host-key questions appear in the session's
terminal pane. Proteus does not store passwords or private keys.

## Connect

Open Actions, type `ssh`, then press Tab. Select a discovered alias or **SSH:
Connect to destination** and enter an alias or `user@host`. Configure custom
ports and other connection settings in `~/.ssh/config`. Literal IPv6 addresses
are accepted; SSH URIs and command-line option strings are not.

Suggestions come from `Host` declarations in `~/.ssh/config`, `/etc/ssh/ssh_config`,
and supported `Include` files. Discovery handles quoted words, comments, and
Include globs, with limits on bytes, files, directory entries, and depth. It
never launches SSH, evaluates `Match exec`, or expands environment/token-based
Includes. Conditional Includes can produce extra suggestions. Wildcard and
negated hosts are not enumerated. Discovery warnings appear in Actions. Missing
suggestions never prevent manually connecting.

This is intentionally a best-effort index, not an OpenSSH configuration resolver.
The original destination is passed to OpenSSH unchanged. Proteus does not run
`ssh -G` while opening Actions: configuration evaluation can execute `Match exec`.

## Remote shell integration

**SSH: Connect with Bash integration** explicitly starts remote Bash instead of
the server's default shell. Bash 4.4+ provides command boundaries, exit statuses,
and remote hostname/cwd reporting through the existing terminal integration.
It reads the normal interactive Bash rc files, then loads Proteus hooks from an
anonymous pipe. No remote installation or persistent startup-file edits occur.
The server's command shell must understand POSIX-style quoting; restricted SSH
accounts may disallow this mode. Plain connections require no integration.

SSH shell panes advertise `xterm-256color`, avoiding a remote Proteus terminfo
requirement. Their titles always identify the destination; integration adds the
reported remote hostname/cwd. `running` describes the local SSH process, including
its authentication phase; it is not a claim that authentication has completed.
Remote-reported paths are never used as local working directories.

## Reconnect and stop

When SSH, SFTP, a transfer, or a forward exits, its pane retains scrollback and
shows the process exit status. Focus that pane and choose **SSH: Reconnect / run
again** to start a fresh pane. For tasks and transfers this repeats the operation.
The previous pane remains available for inspection and normal pane-close actions.
**SSH: Stop session** hangs up the selected process. Forwarding panes show the
forward specification in their titles and live in the ordinary pane list.

Reconnect does not recover running remote programs. Use server-side tmux or a
similar tool when remote process continuity is required. SSH metadata is currently
in-memory; application restart/session restoration does not reconnect remotely.

## Remote workspaces and tasks

Focus an SSH shell pane and select **SSH: Load / refresh remote workspace**.
This explicitly runs a read-only helper over a separate SSH connection. Python 3
is required remotely. Authentication must work without prompting (keys/agent or
an existing OpenSSH multiplexed connection). Errors appear in Actions.

With shell integration, discovery starts at the last reported remote cwd;
otherwise it starts at the remote home directory. The nearest `.proteus-workspace`
file wins. It uses the same TOML format, Rust/Node presets, and overrides as
[local workspaces](workspaces.md). Loading reads definitions and optional Node
metadata; it never executes task commands. Reopen Actions after loading to see
**Remote task** entries. Selecting one opens an integrated Bash task pane on the
same destination and retains output after completion. Ctrl+C interrupts its
foreground command normally.

Remote task cwd values must be relative without `..`. The server resolves
symlinks and verifies that cwd remains inside the workspace immediately before
execution. Definitions and metadata have bounded reads; symlink/nonregular
metadata files are rejected. Requests time out after 20 seconds, with at most
four outstanding requests and one per pane. Project commands only run when a
task is selected. The definition is a snapshot: refresh to see changes.

## Transfer files

Focus an SSH pane and choose **SSH: Upload file** or **SSH: Download file**.
Enter the full local file path, followed by the remote file path. SFTP performs
the transfer in a new pane, displaying progress/errors and retaining its result.
Remote relative paths are relative to SFTP's starting directory, usually home,
not the shell pane's cwd. Existing destination files may be overwritten.

Paths are encoded for SFTP's own command parser, without a local shell. Transfer
batch files are private and removed when the process ends or its pane closes.
Files are transferred individually; use **SSH: Open SFTP** for an interactive
client with `put`, `get`, recursive transfers, and directory navigation. The
server must provide an SFTP subsystem. Interrupted transfers can leave a partial
destination; reconnect/rerun starts a new transfer.

## Port forwarding

Focus an SSH pane and choose **SSH: Add local port forward** or **SSH: Add remote
port forward**, then enter `[bind_address:]port:host:hostport` using OpenSSH
syntax (including bracketed IPv6 where needed). Each forward gets a dedicated
`ssh -N -T` pane with `ExitOnForwardFailure=yes`. Authentication remains interactive.
Stop or close that pane to end its process and forward. Binding and reachability
follow OpenSSH/server policies. Managed forwarding panes disable connection
multiplexing for their own process, so closing the owning pane reliably removes
the forward instead of leaving it attached to a shared master. Other SSH settings
still come from your normal configuration.
