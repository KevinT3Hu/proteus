//! SSH UI orchestration. Authentication stays in OpenSSH's PTY.
use super::{App, textinput::Purpose};
use crate::{
    layout::PaneId,
    project::Workspace,
    ssh::{self, Connection, Destination, Kind},
};
use std::{
    os::fd::OwnedFd,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Pick {
    Connect { integrated: bool },
    Host(String),
    Reconnect(PaneId),
    Workspace(PaneId),
    Task(PaneId, crate::project::Task),
    Sftp(PaneId),
    Upload(PaneId),
    Download(PaneId),
    Forward(PaneId, bool),
    Stop(PaneId),
    Warning(String),
}
pub(super) struct Remote {
    pub connection: Connection,
    pub closed: bool,
    pub loading: bool,
    pub workspace: Option<Workspace>,
    pub error: Option<String>,
    pub batch: Option<ssh::Batch>,
    pub command: Option<String>,
}

impl App {
    pub(super) fn ssh_action(&mut self, action: ssh::Action) {
        use ssh::Action;
        let id = self.focused_id();
        if !matches!(action, Action::Connect | Action::ConnectBash) && self.focused_pane().remote.is_none() {
            return self.notice("Focus an SSH pane first".into());
        }
        self.ssh_pick(match action {
            Action::Connect => Pick::Connect { integrated: false },
            Action::ConnectBash => Pick::Connect { integrated: true },
            Action::Reconnect => Pick::Reconnect(id),
            Action::Workspace => Pick::Workspace(id),
            Action::Sftp => Pick::Sftp(id),
            Action::Upload => Pick::Upload(id),
            Action::Download => Pick::Download(id),
            Action::ForwardLocal => Pick::Forward(id, false),
            Action::ForwardRemote => Pick::Forward(id, true),
            Action::Stop => Pick::Stop(id),
        });
    }
    pub(super) fn ssh_items(&self) -> Vec<(String, Pick)> {
        let mut items = Vec::new();
        items
            .extend(self.ssh_suggestions.hosts.iter().map(|h| (format!("SSH: Connect to {h}"), Pick::Host(h.clone()))));
        items.extend(
            self.ssh_suggestions.warnings.iter().map(|w| (format!("SSH discovery: {w}"), Pick::Warning(w.clone()))),
        );
        let id = self.focused_id();
        if let Some(remote) = self.focused_pane().remote.as_ref() {
            let dest = remote.connection.destination.as_str();
            if matches!(remote.connection.kind, Kind::Shell { .. }) {
                if remote.loading {
                    items.push((
                        "SSH: Loading remote workspace…".into(),
                        Pick::Warning("Remote workspace request is still running".into()),
                    ));
                }

                if let Some(error) = &remote.error {
                    items.push((format!("SSH workspace: {error}"), Pick::Warning(error.clone())));
                }
                if let Some(workspace) = &remote.workspace {
                    items.extend(workspace.tasks.iter().map(|task| {
                        (
                            format!(
                                "Remote task: {} · {} · {dest} · {}",
                                task.name,
                                workspace.name,
                                super::workspace::shell_join(&task.command)
                            ),
                            Pick::Task(id, task.clone()),
                        )
                    }));
                }
            }
        }
        items
    }

    pub(super) fn ssh_pick(&mut self, pick: Pick) {
        match pick {
            Pick::Connect { integrated } => self.open_input(
                Purpose::SshConnect(integrated),
                "",
                "SSH alias or user@host; OpenSSH handles authentication".into(),
            ),
            Pick::Host(host) => self.ssh_connect(&host, false),
            Pick::Reconnect(id) => {
                if let Some(connection) = self
                    .panes
                    .get(&id)
                    .and_then(|p| p.remote.as_ref())
                    .map(|r| (r.connection.clone(), r.command.clone()))
                {
                    self.ssh_open(connection.0, connection.1);
                }
            }
            Pick::Warning(warning) => self.notice(warning),
            Pick::Stop(id) => {
                if let Some(pane) = self.panes.get_mut(&id) {
                    pane.finish();
                }
            }
            Pick::Workspace(id) => {
                if self
                    .panes
                    .get(&id)
                    .and_then(|p| p.remote.as_ref())
                    .is_some_and(|r| matches!(r.connection.kind, Kind::Shell { .. }))
                {
                    self.ssh_workspace(id);
                } else {
                    self.notice("Focus an SSH shell pane to load its workspace".into());
                }
            }
            Pick::Task(id, task) => {
                let data = self.panes.get(&id).and_then(|p| p.remote.as_ref()).and_then(|r| {
                    r.workspace.as_ref().map(|w| {
                        (r.connection.destination.clone(), w.path.parent().unwrap_or(Path::new("/")).to_path_buf())
                    })
                });
                if let Some((destination, root)) = data {
                    self.ssh_open(
                        Connection { destination, kind: Kind::Shell { integrated: true } },
                        Some(ssh::task_command(&root, &task)),
                    );
                }
            }
            Pick::Sftp(id) => {
                if let Some(destination) = self.ssh_destination(id) {
                    self.ssh_open(Connection { destination, kind: Kind::Sftp }, None);
                }
            }
            Pick::Forward(id, remote) => self.open_input(
                Purpose::SshForward(id, remote),
                "",
                "[bind_address:]port:host:hostport; session pane owns the forwarding lifetime".into(),
            ),
            Pick::Upload(id) | Pick::Download(id) => {
                let upload = matches!(pick, Pick::Upload(_));
                self.open_input(
                    Purpose::SshTransferLocal(id, upload),
                    "",
                    if upload {
                        "Local file to upload (full path)"
                    } else {
                        "Local destination file (full path; may be overwritten)"
                    }
                    .into(),
                );
            }
        }
    }
    fn ssh_destination(&self, id: PaneId) -> Option<Destination> {
        self.panes.get(&id)?.remote.as_ref().map(|r| r.connection.destination.clone())
    }
    pub(super) fn ssh_connect(&mut self, value: &str, integrated: bool) {
        match Destination::parse(value) {
            Ok(destination) => self.ssh_open(Connection { destination, kind: Kind::Shell { integrated } }, None),
            Err(e) => self.notice(e),
        }
    }
    fn ssh_open(&mut self, connection: Connection, command: Option<String>) {
        let mut argv = connection.argv();
        let mut batch = None;
        if let Kind::Transfer { upload, local, remote } = &connection.kind {
            match ssh::Batch::new(*upload, local, remote) {
                Ok(file) => {
                    argv = connection.batch_argv(&file.path);
                    batch = Some(file);
                }
                Err(e) => return self.notice_error(format!("cannot prepare the SFTP transfer: {e}")),
            }
        }
        if let Some(command) = &command {
            if command.len() > 65536 {
                return self.notice("Remote task command exceeds 64 KiB after quoting".into());
            }
            argv.pop();
            argv.push(command.into());
        }
        tracing::debug!(kind = ?connection.kind, argv = %ssh::argv_for_log(&argv), "opening ssh pane");
        match self.split_with(None, None, Some(argv)) {
            Ok(id) => {
                tracing::info!(pane = id, connection = %connection.label(), "ssh pane opened");
                if let Some(pane) = self.panes.get_mut(&id) {
                    pane.remote = Some(Remote {
                        connection,
                        closed: false,
                        loading: false,
                        workspace: None,
                        error: None,
                        batch,
                        command,
                    });
                    pane.expire_proc_title();
                    pane.refresh_proc_title();
                }
                self.update_title();
            }
            Err(e) => self.notice_error(format!("{}: {e}", connection.label())),
        }
    }
    pub(super) fn ssh_submit(&mut self, purpose: Purpose, text: String) {
        match purpose {
            Purpose::SshConnect(integrated) => self.ssh_connect(&text, integrated),
            Purpose::SshForward(id, remote) => match ssh::forwarding(&text) {
                Ok(spec) => {
                    if let Some(destination) = self.ssh_destination(id) {
                        self.ssh_open(Connection { destination, kind: Kind::Forward { remote, spec } }, None);
                    }
                }
                Err(e) => self.notice(e),
            },
            Purpose::SshTransferLocal(id, upload) => {
                let local = PathBuf::from(&text);
                if !local.is_absolute() || (upload && !local.is_file()) || (!upload && local.is_dir()) {
                    return self.notice("Expected a full local file path".into());
                }
                self.ssh_transfer_path = Some((id, upload, local));
                self.open_input(
                    Purpose::SshTransferRemote(id, upload),
                    "",
                    "Remote file path (may be overwritten on upload)".into(),
                );
            }
            Purpose::SshTransferRemote(id, upload) => {
                if let Some((pending, direction, local)) = self.ssh_transfer_path.take()
                    && pending == id
                    && direction == upload
                    && let Some(destination) = self.ssh_destination(id)
                {
                    self.ssh_open(
                        Connection { destination, kind: Kind::Transfer { upload, local, remote: text } },
                        None,
                    );
                }
            }
            _ => {}
        }
    }
    /// Preserve scrollback and mark lifecycle without treating EOF as a request
    /// to remove the pane. Explicit pane close still removes it normally.
    pub(super) fn ssh_closed(&mut self, id: PaneId) {
        let Some(pane) = self.panes.get_mut(&id) else { return };
        let Some(remote) = pane.remote.as_ref() else { return self.close_pane(id) };
        let label = remote.connection.label();
        let status = pane.finish();
        if status == 0 {
            tracing::info!(pane = id, connection = %label, status, "ssh process ended");
        } else {
            tracing::warn!(pane = id, connection = %label, status, "ssh process failed");
        }
        if let Some(remote) = &mut pane.remote {
            remote.closed = true;
            remote.batch = None;
        }
        pane.ended_title(format!("{label} · ended ({status})"));
        pane.term
            .advance(format!("\r\n[SSH process ended: {status}. Actions → SSH: Reconnect / run again]\r\n").as_bytes());
        self.mark_all_dirty();
        self.update_title();
    }
    fn ssh_workspace(&mut self, id: PaneId) {
        let Some(pane) = self.panes.get_mut(&id) else { return };
        let cwd = pane.term.cwd().map(|c| c.path.clone()).unwrap_or_default();
        let Some(remote) = &mut pane.remote else { return };
        if remote.loading {
            return;
        }
        remote.loading = true;
        remote.error = None;
        let destination = remote.connection.destination.clone();
        // At most one per pane and four globally; a closed pane cannot turn the
        // request queue into unbounded subprocesses.
        static ACTIVE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
        let Ok(permit) = ACTIVE.try_acquire() else {
            remote.loading = false;
            return self.notice("Four remote workspace requests are already running".into());
        };
        let fetch = async move {
            let result = fetch_workspace(&destination, &cwd).await;
            drop(permit);
            result
        };
        if let Err(e) = self.in_task(fetch, move |app, result| app.ssh_workspace_received(id, result)) {
            if let Some(remote) = self.panes.get_mut(&id).and_then(|p| p.remote.as_mut()) {
                remote.loading = false;
            }
            self.notice_error(format!("cannot start the remote workspace request: {e}"));
        }
    }
    fn ssh_workspace_received(&mut self, id: PaneId, result: Result<Option<Workspace>, String>) {
        if let Some(remote) = self.panes.get_mut(&id).and_then(|p| p.remote.as_mut()) {
            remote.loading = false;
            match result {
                Ok(workspace) => {
                    remote.workspace = workspace;
                    remote.error = remote.workspace.is_none().then(|| {
                        "No .proteus-workspace found above the remote cwd (or home without shell integration)".into()
                    });
                }
                Err(e) => {
                    remote.workspace = None;
                    remote.error = Some(e);
                }
            }
        }
        if self.w.input.as_ref().is_some_and(|i| i.purpose == Purpose::Palette) {
            self.palette_update();
        }
        self.w.dirty = true;
    }
}

async fn fetch_workspace(destination: &Destination, cwd: &str) -> Result<Option<Workspace>, String> {
    const FAILED: &str = "Remote workspace read failed; needs Python 3 and noninteractive SSH authentication. Check the remote files and connection.";
    let command = ssh::workspace_command(cwd);
    let args = ["-T", "-oBatchMode=yes", "-oConnectTimeout=10", "--", destination.as_str(), &command];
    tracing::debug!(
        argv = %ssh::argv_for_log(&std::iter::once("ssh").chain(args).map(Into::into).collect::<Vec<_>>()),
        "fetching remote workspace"
    );
    let mut child = tokio::process::Command::new("ssh")
        .args(args)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("cannot run ssh: {e}"))?;
    let pipe = |p: Option<OwnedFd>| p.ok_or_else(|| std::io::Error::other("not piped"));
    let stdout = pipe(child.stdout.take().and_then(|p| p.into_owned_fd().ok()));
    let stderr = pipe(child.stderr.take().and_then(|p| p.into_owned_fd().ok()));
    let stderr =
        proteus_tasks::spawn(async { super::transfer::read(stderr?, 64 * 1024, Duration::from_secs(25)).await });
    let result = match stdout {
        Ok(fd) => super::transfer::read(fd, 2 * 1024 * 1024, Duration::from_secs(20)).await,
        Err(e) => Err(e),
    };
    // read is deadline bounded; a server that closes stdout but keeps
    // running is killed with its process group.
    let status = child.try_wait().map_err(|e| format!("ssh: {e}"));
    if !matches!(status, Ok(Some(_)))
        && let Some(pid) = child.id().and_then(|id| rustix::process::Pid::from_raw(id as i32))
    {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    let _ = child.wait().await;
    let stderr = match stderr {
        Ok(task) => task.await.ok().and_then(Result::ok).unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let stderr = String::from_utf8_lossy(&stderr);
    let stderr = stderr.trim();
    let fail = |what: String| {
        let mut start = stderr.len().saturating_sub(4096);
        while !stderr.is_char_boundary(start) {
            start += 1;
        }
        tracing::warn!(
            destination = destination.as_str(),
            status = ?status,
            stderr = &stderr[start..],
            "remote workspace: {what}"
        );
        match last_line(stderr) {
            Some(line) => format!("{what} ({line})"),
            None => what,
        }
    };
    let bytes = match result {
        Ok(bytes) => bytes,
        Err(e) => return Err(fail(format!("remote workspace: {e}"))),
    };
    if bytes.is_empty() {
        return Err(fail(FAILED.into()));
    }
    match &status {
        Ok(Some(s)) if !s.success() => Err(fail(FAILED.into())),
        Err(e) => Err(fail(e.clone())),
        _ => ssh::parse_workspace(&bytes).inspect_err(|e| {
            tracing::warn!(destination = destination.as_str(), "remote workspace: {e}");
        }),
    }
}

/// The last nonempty line of `text`, cut to a length fit for a notice.
fn last_line(text: &str) -> Option<String> {
    const MAX: usize = 200;
    let line = text.lines().map(str::trim).rfind(|l| !l.is_empty())?;
    let mut out: String = line.chars().filter(|c| !c.is_control()).take(MAX).collect();
    if line.chars().count() > MAX {
        out.push('…');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::last_line;

    #[test]
    fn stderr_summary_is_one_short_line() {
        assert_eq!(last_line(""), None);
        assert_eq!(last_line("\n  \n"), None);
        let stderr = "Warning: Permanently added 'h'\nuser@h: Permission denied (publickey).\n\n";
        assert_eq!(last_line(stderr).as_deref(), Some("user@h: Permission denied (publickey)."));
        let long = last_line(&format!("x\u{1b}{}", "y".repeat(500))).unwrap();
        assert_eq!(long.chars().count(), 201);
        assert!(long.ends_with('…') && !long.contains('\u{1b}'));
    }
}
