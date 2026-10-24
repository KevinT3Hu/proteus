//! SSH UI orchestration. Authentication stays in OpenSSH's PTY.
use super::{App, Msg, textinput::Purpose};
use crate::{
    layout::PaneId,
    project::Workspace,
    ssh::{self, Connection, Destination, Kind},
};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
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
                Err(e) => return self.notice(e),
            }
        }
        if let Some(command) = &command {
            if command.len() > 65536 {
                return self.notice("Remote task command exceeds 64 KiB after quoting".into());
            }
            argv.pop();
            argv.push(command.into());
        }
        match self.split_with(None, None, Some(argv)) {
            Ok(id) => {
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
            Err(e) => self.notice(e),
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
        let sender = self.sender.clone();
        // At most one per pane and four globally; a closed pane cannot turn the
        // request queue into unbounded subprocesses.
        static ACTIVE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        use std::sync::atomic::Ordering;
        if ACTIVE.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| (n < 4).then_some(n + 1)).is_err() {
            remote.loading = false;
            return self.notice("Four remote workspace requests are already running".into());
        }
        let result = std::thread::Builder::new().name("ssh-workspace".into()).spawn(move || {
            let result = fetch_workspace(&destination, &cwd);
            ACTIVE.fetch_sub(1, Ordering::Relaxed);
            let _ = sender.send(Msg::SshWorkspace(id, result));
        });
        if let Err(e) = result {
            ACTIVE.fetch_sub(1, Ordering::Relaxed);
            if let Some(remote) = self.panes.get_mut(&id).and_then(|p| p.remote.as_mut()) {
                remote.loading = false;
            }
            self.notice(e.to_string());
        }
    }
    pub(super) fn ssh_workspace_received(&mut self, id: PaneId, result: Result<Option<Workspace>, String>) {
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

fn fetch_workspace(destination: &Destination, cwd: &str) -> Result<Option<Workspace>, String> {
    use std::os::unix::process::CommandExt;
    let mut child = Command::new("ssh")
        .args([
            "-T",
            "-oBatchMode=yes",
            "-oConnectTimeout=10",
            "--",
            destination.as_str(),
            &ssh::workspace_command(cwd),
        ])
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let result = super::transfer::read(child.stdout.take().expect("piped"), 2 * 1024 * 1024, Duration::from_secs(20));
    // read is deadline bounded; never block the worker on a server that closes
    // stdout but continues running. Kill/reap on every exit path.
    let status = child.try_wait().map_err(|e| e.to_string());
    if !matches!(status, Ok(Some(_)))
        && let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32)
    {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    let _ = child.wait();
    let bytes = result.map_err(|e| format!("remote workspace: {e}"))?;
    if bytes.is_empty() {
        return Err("Remote workspace read failed; needs Python 3 and noninteractive SSH authentication. Check the remote files and connection.".into());
    }
    match status {
        Ok(Some(status)) if !status.success() => Err("Remote workspace read failed; needs Python 3 and noninteractive SSH authentication. Check the remote files and connection.".into()),
        Err(e) => Err(e),
        _ => ssh::parse_workspace(&bytes),
    }
}
