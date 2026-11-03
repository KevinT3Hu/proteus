//! OpenSSH is the configuration authority. This module only discovers suggestions
//! and constructs argv; discovery never evaluates Match, expands commands or runs ssh.
use std::{
    collections::BTreeSet,
    ffi::OsString,
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination(String);
impl Destination {
    pub fn parse(value: &str) -> Result<Self, String> {
        if value.is_empty()
            || value.len() > 1024
            || value.starts_with('-')
            || value.chars().any(|c| c.is_control() || c.is_whitespace())
            || !value.chars().all(|c| c.is_ascii_alphanumeric() || "_.-@:%[]".contains(c))
            || value.split('@').count() > 2
            || value.split('@').any(|p| p.is_empty() || p.starts_with('-'))
        {
            return Err("Expected an SSH alias or [user@]host (configure ports in ~/.ssh/config)".into());
        }
        Ok(Self(value.into()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Shell { integrated: bool },
    Forward { remote: bool, spec: String },
    Sftp,
    Transfer { upload: bool, local: PathBuf, remote: String },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    pub destination: Destination,
    pub kind: Kind,
}
impl Connection {
    pub fn argv(&self) -> Vec<OsString> {
        let mut args: Vec<String> = match &self.kind {
            Kind::Shell { .. } => vec!["env".into(), "TERM=xterm-256color".into(), "ssh".into(), "-t".into()],
            Kind::Forward { remote, spec } => vec![
                "ssh".into(),
                "-N".into(),
                "-T".into(),
                "-o".into(),
                "ExitOnForwardFailure=yes".into(),
                "-oControlMaster=no".into(),
                "-oControlPath=none".into(),
                if *remote { "-R" } else { "-L" }.into(),
                spec.clone(),
            ],
            Kind::Sftp | Kind::Transfer { .. } => vec!["sftp".into()],
        };
        args.extend(["--".into(), self.destination.0.clone()]);
        if matches!(self.kind, Kind::Shell { integrated: true }) {
            args.push(bash_command(None, None));
        }
        args.into_iter().map(OsString::from).collect()
    }
    pub fn batch_argv(&self, path: &Path) -> Vec<OsString> {
        let mut argv = self.argv();
        argv.splice(1..1, [OsString::from("-oBatchMode=no"), OsString::from("-b"), path.as_os_str().to_owned()]);
        argv
    }
    pub fn label(&self) -> String {
        match &self.kind {
            Kind::Shell { integrated } => {
                format!("SSH {}{}", self.destination.0, if *integrated { " · Bash" } else { "" })
            }
            Kind::Forward { remote, spec } => {
                format!("SSH {} · {} {}", self.destination.0, if *remote { "-R" } else { "-L" }, spec)
            }
            Kind::Sftp => format!("SFTP {}", self.destination.0),
            Kind::Transfer { upload, .. } => {
                format!("SFTP {} · {}", self.destination.0, if *upload { "upload" } else { "download" })
            }
        }
    }
}

/// Opt-in Bash integration: rcfile is an anonymous pipe, no remote files installed.
/// The outer bash provides process substitution; the inner shell reads normal rc
/// files before hooks. Task commands remain argv and execute only after selection.
pub fn bash_command(cwd: Option<&Path>, task: Option<&[String]>) -> String {
    remote(&bash_script(cwd, task))
}

/// POSIX `sh` code starting the integrated interactive Bash.
fn bash_script(cwd: Option<&Path>, task: Option<&[String]>) -> String {
    let hooks = include_str!("../../proteus-pty/shell-integration/bash/proteus.bash");
    let mut rc =
        String::from("[[ -r /etc/bash.bashrc ]] && source /etc/bash.bashrc\n[[ -r ~/.bashrc ]] && source ~/.bashrc\n");
    rc.push_str(hooks);
    if let Some(cwd) = cwd {
        rc.push_str(&format!("\ncd -- {} || return\n", quote(&cwd.to_string_lossy())));
    }
    if let Some(task) = task {
        let command = task.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ");
        rc.push_str(&format!("\nbuiltin printf '\\e]133;A\\aTask> \\e]133;B\\a%s\\n\\e]133;C\\a' {}\n{command}\n__proteus_status=$?\nbuiltin printf '\\e]133;D;%s\\a' \"$__proteus_status\"\n", quote(&command)));
    }
    let launch = format!("exec bash --rcfile <(printf %s {}) -i", quote(&rc));
    format!("exec bash -c {}", quote(&launch))
}

/// A remote command that any login shell (sh, bash, zsh, fish, csh/tcsh)
/// passes unchanged to `sh`: the script travels as base64, so the login
/// shell sees no newlines, backslashes or nested quotes.
pub fn remote(script: &str) -> String {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(script);
    format!("sh -c 'eval \"$(printf %s {encoded} | base64 -d)\"'")
}

pub fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn forwarding(value: &str) -> Result<String, String> {
    if value.is_empty()
        || value.len() > 1024
        || value.chars().any(|c| c.is_control() || c.is_whitespace())
        || !value.contains(':')
    {
        return Err("Expected [bind_address:]port:host:hostport (OpenSSH forwarding syntax)".into());
    }
    Ok(value.into())
}

#[derive(Default)]
pub struct Suggestions {
    pub hosts: Vec<String>,
    pub warnings: Vec<String>,
}

pub fn discover(home: &Path, system: &Path) -> Suggestions {
    let mut scan = Scan {
        home,
        system,
        seen: BTreeSet::new(),
        hosts: BTreeSet::new(),
        bytes: 0,
        entries: 0,
        warnings: Vec::new(),
    };
    scan.file(&home.join(".ssh/config"), false, 0);
    scan.file(&system.join("ssh_config"), true, 0);
    Suggestions { hosts: scan.hosts.into_iter().collect(), warnings: scan.warnings }
}
struct Scan<'a> {
    home: &'a Path,
    system: &'a Path,
    seen: BTreeSet<PathBuf>,
    hosts: BTreeSet<String>,
    bytes: usize,
    entries: usize,
    warnings: Vec<String>,
}
impl Scan<'_> {
    fn warn(&mut self, msg: String) {
        if self.warnings.len() < 16 {
            self.warnings.push(msg);
        }
    }
    fn file(&mut self, path: &Path, system: bool, depth: usize) {
        if self.seen.len() >= 128 || depth > 16 || self.bytes >= 1024 * 1024 {
            self.warn("SSH suggestion scan limit reached".into());
            return;
        }
        let Ok(canonical) = path.canonicalize() else { return };
        if !self.seen.insert(canonical) {
            return;
        }
        use std::os::unix::fs::OpenOptionsExt;
        let result = (|| -> std::io::Result<String> {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32)
                .open(path)?;
            if !file.metadata()?.is_file() {
                return Err(std::io::Error::other("expected regular file"));
            }
            let mut s = String::new();
            file.take((1024 * 1024 - self.bytes + 1) as u64).read_to_string(&mut s)?;
            Ok(s)
        })();
        let text = match result {
            Ok(s) => s,
            Err(e) => {
                self.warn(format!("{}: {e}", path.display()));
                return;
            }
        };
        self.bytes += text.len();
        if self.bytes > 1024 * 1024 {
            self.warn("SSH config exceeds discovery limit".into());
            return;
        }
        for line in text.lines() {
            let Some(tokens) = words(line) else {
                self.warn(format!("{}: skipped unsupported config syntax", path.display()));
                continue;
            };
            let Some(key) = tokens.first() else { continue };
            if key.eq_ignore_ascii_case("host") {
                for host in &tokens[1..] {
                    if self.hosts.len() < 4096 && !host.contains(['[', ']']) && Destination::parse(host).is_ok() {
                        self.hosts.insert(host.clone());
                    }
                }
            } else if key.eq_ignore_ascii_case("include") {
                for include in &tokens[1..] {
                    if include.contains(['%', '$']) || (include.starts_with('~') && !include.starts_with("~/")) {
                        self.warn(format!("Dynamic SSH Include skipped: {include}"));
                        continue;
                    }
                    let include = if let Some(rel) = include.strip_prefix("~/") {
                        self.home.join(rel)
                    } else if Path::new(include).is_absolute() {
                        PathBuf::from(include)
                    } else if system {
                        self.system.join(include)
                    } else {
                        self.home.join(".ssh").join(include)
                    };
                    for path in self.expand(&include) {
                        self.file(&path, system, depth + 1);
                    }
                }
            }
        }
    }
    fn expand(&mut self, pattern: &Path) -> Vec<PathBuf> {
        let mut paths = vec![PathBuf::new()];
        for component in pattern.components() {
            let part = component.as_os_str().to_string_lossy();
            if !part.contains(['*', '?', '[']) {
                for path in &mut paths {
                    path.push(component);
                }
                continue;
            }
            let Some(regex) = glob_regex(&part) else {
                self.warn(format!("Unsupported SSH Include pattern: {}", pattern.display()));
                return Vec::new();
            };
            let mut next = Vec::new();
            for path in paths {
                if let Ok(entries) = std::fs::read_dir(&path) {
                    for entry in entries {
                        self.entries += 1;
                        if self.entries > 4096 {
                            self.warn("SSH Include directory scan limit reached".into());
                            return Vec::new();
                        }
                        if let Ok(entry) = entry
                            && regex.is_match(&entry.file_name().to_string_lossy())
                        {
                            next.push(entry.path());
                        }
                    }
                }
            }
            next.sort();
            paths = next;
        }
        paths
    }
}

fn glob_regex(pattern: &str) -> Option<regex::Regex> {
    let mut out = String::from("^");
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '*' => out.push_str(".*"),
            '?' => out.push('.'),
            '[' => {
                out.push('[');
                let mut first = true;
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == ']' {
                        closed = true;
                        break;
                    }
                    if first && c == '!' {
                        out.push('^');
                    } else if c == '\\' {
                        out.push_str("\\\\");
                    } else {
                        out.push(c);
                    }
                    first = false;
                }
                if !closed {
                    return None;
                }
                out.push(']');
            }
            c => out.push_str(&regex::escape(&c.to_string())),
        }
    }
    out.push('$');
    regex::Regex::new(&out).ok()
}

/// Narrow lexical reader for Host/Include discovery, not a configuration resolver.
fn words(line: &str) -> Option<Vec<String>> {
    let mut result = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escape = false;
    for c in line.chars() {
        if escape {
            word.push(c);
            escape = false;
            continue;
        }
        if c == '\\' {
            escape = true;
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                word.push(c);
            }
            continue;
        }
        match c {
            '\'' | '"' => quote = Some(c),
            '#' => break,
            '=' if result.len() <= 1 && (word.is_empty() || result.is_empty()) => {
                if !word.is_empty() {
                    result.push(std::mem::take(&mut word));
                }
            }
            c if c.is_whitespace() => {
                if !word.is_empty() {
                    result.push(std::mem::take(&mut word));
                }
            }
            c => word.push(c),
        }
    }
    if quote.is_some() || escape {
        return None;
    }
    if !word.is_empty() {
        result.push(word);
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_is_bounded_and_does_not_evaluate_match_exec() {
        let root = std::env::temp_dir().join(format!("proteus-ssh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".ssh/conf.d")).unwrap();
        std::fs::create_dir_all(root.join("etc")).unwrap();
        std::fs::write(root.join(".ssh/config"), "Host=one two *.example !excluded\nInclude conf.d/*.conf\nMatch exec \"touch should-not-exist\"\nHost conditional\nInclude %h.conf\n").unwrap();
        std::fs::write(root.join(".ssh/conf.d/a.conf"), "Host \"three\" # comment\nInclude config\n").unwrap();
        std::fs::write(root.join("etc/ssh_config"), "Host system\n").unwrap();
        let found = discover(&root, &root.join("etc"));
        assert_eq!(found.hosts, ["conditional", "one", "system", "three", "two"]);
        assert_eq!(found.warnings.len(), 1);
        assert!(!root.join("should-not-exist").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn arguments_are_not_shell_code_or_options() {
        for invalid in ["", "-oProxyCommand=evil", "host\nnext", "x y", "ssh://host", "*.host"] {
            assert!(Destination::parse(invalid).is_err());
        }
        let c = Connection {
            destination: Destination::parse("alice@dev").unwrap(),
            kind: Kind::Shell { integrated: false },
        };
        assert_eq!(c.argv().last().unwrap(), "alice@dev");
        assert!(c.argv().iter().any(|s| s == "--"));
        assert!(forwarding("8080:localhost:80").is_ok());
        assert!(forwarding("8080:localhost:80\n!sh").is_err());
        assert!(bash_script(None, None).contains("--rcfile"));
        assert!(bash_command(None, None).bytes().all(|b| b.is_ascii_graphic() || b == b' '));
    }

    #[test]
    fn remote_commands_survive_any_login_shell() {
        let script = "printf '%s|' \"it's\" 'a\\b' \"$((1+1))\"\nprintf 'line2'";
        let command = remote(script);
        let mut tried = 0;
        for shell in ["sh", "bash", "zsh", "fish", "tcsh", "csh"] {
            let Ok(out) = std::process::Command::new(shell).arg("-c").arg(&command).output() else { continue };
            tried += 1;
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(stdout.ends_with("it's|a\\b|2|line2"), "{shell}: {out:?}");
        }
        assert!(tried > 0);
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    root: PathBuf,
    definition: String,
    package: Option<String>,
    locks: Vec<String>,
}

pub fn workspace_command(cwd: &str) -> String {
    remote(&format!("exec python3 -c {} {}", quote(include_str!("ssh-workspace.py")), quote(cwd)))
}

pub fn parse_workspace(bytes: &[u8]) -> Result<Option<crate::project::Workspace>, String> {
    let snapshot: Option<Snapshot> =
        serde_json::from_slice(bytes).map_err(|e| format!("remote workspace response: {e}"))?;
    let Some(s) = snapshot else { return Ok(None) };
    if !s.root.is_absolute() || s.root.to_string_lossy().chars().any(char::is_control) || s.locks.len() > 4 {
        return Err("invalid remote workspace root".into());
    }
    crate::project::Workspace::parse(
        &s.root,
        &s.definition,
        |_| s.package.clone().filter(|s| s.len() <= 256 * 1024).ok_or_else(|| "remote package.json unavailable".into()),
        |p| s.locks.iter().any(|lock| lock == p),
        |p| {
            if p.components().any(|c| !matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir)) {
                return Err("remote cwd must be a relative path without '..'".into());
            }
            Ok(s.root.join(p))
        },
    )
    .map(Some)
}

/// Validate symlink containment on the server, then exec a Bash task shell.
pub fn task_command(root: &Path, task: &crate::project::Task) -> String {
    let code = "import os,sys\nr=os.path.realpath(sys.argv[1]);p=os.path.realpath(sys.argv[2])\nif os.path.commonpath([r,p])!=r or not os.path.isdir(p): raise ValueError('task cwd escapes workspace or does not exist')\nos.chdir(p)\nos.execvp('bash',['bash','-c',sys.argv[3]])";
    remote(&format!(
        "exec python3 -c {} {} {} {}",
        quote(code),
        quote(&root.to_string_lossy()),
        quote(&task.cwd.to_string_lossy()),
        quote(&bash_script(None, Some(&task.command)))
    ))
}

/// A private SFTP batch file, retained until its pane is closed.
pub struct Batch {
    file: proteus_pty::persistence::StateFile,
    pub path: PathBuf,
}
impl Drop for Batch {
    fn drop(&mut self) {
        let _ = self.file.remove();
    }
}
impl Batch {
    pub fn new(upload: bool, local: &Path, remote: &str) -> Result<Self, String> {
        Self::in_directory(&proteus_pty::shell::runtime_dir().join("ssh"), upload, local, remote)
    }
    fn in_directory(directory: &Path, upload: bool, local: &Path, remote: &str) -> Result<Self, String> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let local = local.to_str().ok_or("SFTP paths must be UTF-8")?;
        let (src, dst) = if upload { (local, remote) } else { (remote, local) };
        let line = format!("{} {} {}\n", if upload { "put" } else { "get" }, sftp_path(src)?, sftp_path(dst)?);
        if line.len() >= 8192 {
            return Err("SFTP command exceeds 8 KiB".into());
        }
        let path = directory.join(format!("transfer-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let file = proteus_pty::persistence::StateFile::new(&path).map_err(|e| e.to_string())?;
        file.write(line.as_bytes()).map_err(|e| e.to_string())?;
        Ok(Self { file, path })
    }
}
fn sftp_path(path: &str) -> Result<String, String> {
    if path.is_empty() || path.len() > 16384 || path.chars().any(char::is_control) {
        return Err("Expected a nonempty file path without control characters".into());
    }
    // SFTP has its own lexer and globbing layer, independent from shell quoting.
    let path = if path.starts_with('-') { format!("./{path}") } else { path.to_owned() };
    let mut out = String::from("\"");
    for c in path.chars() {
        if matches!(c, '\\' | '"') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Connect,
    ConnectBash,
    Reconnect,
    Workspace,
    Sftp,
    Upload,
    Download,
    ForwardLocal,
    ForwardRemote,
    Stop,
}
impl Action {
    pub const ALL: [Self; 10] = [
        Self::Connect,
        Self::ConnectBash,
        Self::Reconnect,
        Self::Workspace,
        Self::Sftp,
        Self::Upload,
        Self::Download,
        Self::ForwardLocal,
        Self::ForwardRemote,
        Self::Stop,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Self::Connect => "ssh.connect",
            Self::ConnectBash => "ssh.connect_bash",
            Self::Reconnect => "ssh.reconnect",
            Self::Workspace => "ssh.workspace",
            Self::Sftp => "ssh.sftp",
            Self::Upload => "ssh.upload",
            Self::Download => "ssh.download",
            Self::ForwardLocal => "ssh.forward_local",
            Self::ForwardRemote => "ssh.forward_remote",
            Self::Stop => "ssh.stop",
        }
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        process::{Command, Stdio},
        time::{Duration, Instant},
    };

    fn command(connection: &Connection, config: &str) -> Command {
        configured_argv(connection.argv(), config)
    }
    fn configured_argv(mut argv: Vec<OsString>, config: &str) -> Command {
        let program = argv.remove(0);
        let offset = argv.iter().position(|a| a == "ssh").map_or(0, |i| i + 1);
        argv.splice(offset..offset, [OsString::from("-F"), config.into()]);
        let mut c = Command::new(program);
        c.args(argv);
        c
    }
    fn run(c: &mut Command) -> std::process::Output {
        let output = c.stdin(Stdio::null()).output().unwrap();
        assert!(
            output.status.success(),
            "status={} stderr={} stdout={}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
        output
    }
    #[test]
    #[ignore = "run with python3 scripts/test-ssh.py (isolated real sshd)"]
    fn openssh_roundtrip() {
        let config = std::env::var("PROTEUS_SSH_TEST_CONFIG").expect("use scripts/test-ssh.py");
        let root = PathBuf::from(std::env::var_os("PROTEUS_SSH_TEST_ROOT").expect("use scripts/test-ssh.py"));
        let dest = Destination::parse("proteus-loopback").unwrap();
        let connection = Connection { destination: dest.clone(), kind: Kind::Shell { integrated: false } };
        let output = run(command(&connection, &config).arg("printf '%s' proteus-connected"));
        assert_eq!(output.stdout, b"proteus-connected");
        // A missing pinned host key must fail, never silently accept the host.
        let output = Command::new("ssh")
            .args([
                "-F",
                &config,
                "-oUserKnownHostsFile=/dev/null",
                "-oGlobalKnownHostsFile=/dev/null",
                "--",
                dest.as_str(),
                "true",
            ])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("Host key verification failed"));

        // Exercise actual interactive integration on a PTY, including a failing
        // command's OSC status and remote cwd. No GUI/compositor is required.
        let integrated = Connection { destination: dest.clone(), kind: Kind::Shell { integrated: true } };
        let mut argv = integrated.argv();
        let program = argv.remove(0);
        let offset = argv.iter().position(|a| a == "ssh").unwrap() + 1;
        argv.splice(offset..offset, [OsString::from("-F"), config.clone().into()]);
        let spec = proteus_pty::CommandSpec::new(program).args(argv);
        let mut pty = proteus_pty::Pty::spawn(&spec, proteus_pty::WindowSize::new(100, 24)).unwrap();
        let mut master = pty.try_clone_master().unwrap();
        rustix::io::ioctl_fionbio(&master, true).unwrap();
        let mut terminal = proteus_core::Terminal::new(100, 24, Default::default());
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut sent = false;
        let mut finished = false;
        let mut bytes = [0; 8192];
        while Instant::now() < deadline {
            if !sent && pty.raw_input() && terminal.cwd().is_some() {
                master.write_all(b"false\r").unwrap();
                sent = true;
            }
            match master.read(&mut bytes) {
                Ok(n) if n > 0 => {
                    terminal.advance(&bytes[..n]);
                    let events = terminal.take_events();
                    if events
                        .iter()
                        .any(|e| matches!(e, proteus_core::Event::CommandFinished { exit_code: Some(1), .. }))
                    {
                        finished = true;
                        break;
                    }
                }
                Ok(_) => break,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(10)),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => panic!("PTY: {e}"),
            }
        }
        pty.hangup();
        let _ = pty.wait();
        assert!(finished, "interactive shell did not report failing command status");
        assert!(terminal.cwd().is_some());

        let project = root.join("project ' spaced");
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::write(project.join(".proteus-workspace"), "name='Remote'\npresets=['rust']\n[tasks.test]\ncommand=['printf', '%s', 'remote task ; literal']\ncwd='src'\n").unwrap();
        let output = run(command(&connection, &config).arg(workspace_command(project.join("src").to_str().unwrap())));
        let workspace = parse_workspace(&output.stdout).unwrap().unwrap();
        assert_eq!(workspace.name, "Remote");
        let task = workspace.tasks.iter().find(|t| t.id == "test").unwrap();
        let output = run(command(&connection, &config).arg(task_command(&project, task)));
        assert!(String::from_utf8_lossy(&output.stdout).contains("remote task ; literal"));
        let mut terminal = proteus_core::Terminal::new(100, 24, Default::default());
        terminal.advance(&output.stdout);
        assert!(
            terminal
                .take_events()
                .iter()
                .any(|e| matches!(e, proteus_core::Event::CommandFinished { exit_code: Some(0), .. }))
        );
        assert!(terminal.cwd().is_some());
        // Re-evaluate symlinks at task launch, not just at discovery time.
        std::fs::remove_dir(project.join("src")).unwrap();
        std::os::unix::fs::symlink(&root, project.join("src")).unwrap();
        let output =
            command(&connection, &config).arg(task_command(&project, task)).stdin(Stdio::null()).output().unwrap();
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stdout).contains("remote task ; literal"));

        // A real SFTP server verifies the two escaping layers, including glob
        // metacharacters and leading '-' in a remote file name.
        let source = root.join("source ' \" \\ [x]*?.txt");
        let remote = root.join("remote ' \" \\ [x]*?.txt");
        let download = root.join("download.txt");
        std::fs::write(&source, b"transfer bytes\0\xff").unwrap();
        for (upload, local) in [(true, &source), (false, &download)] {
            let batch = Batch::in_directory(&root.join("batches"), upload, local, remote.to_str().unwrap()).unwrap();
            let sftp = Connection {
                destination: dest.clone(),
                kind: Kind::Transfer {
                    upload,
                    local: local.to_path_buf(),
                    remote: remote.to_string_lossy().into_owned(),
                },
            };
            let output = run(&mut configured_argv(sftp.batch_argv(&batch.path), &config));
            assert!(
                output.status.success(),
                "{} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert_eq!(std::fs::read(download).unwrap(), std::fs::read(source).unwrap());

        // Both directions of forwarding carry actual bytes; stopping the
        // owning ssh process closes the listener.
        for reverse in [false, true] {
            let target = TcpListener::bind("127.0.0.1:0").unwrap();
            let target_port = target.local_addr().unwrap().port();
            let reserve = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = reserve.local_addr().unwrap().port();
            drop(reserve);
            let forward = Connection {
                destination: dest.clone(),
                kind: Kind::Forward { remote: reverse, spec: format!("127.0.0.1:{port}:127.0.0.1:{target_port}") },
            };
            let mut child = command(&forward, &config)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)) {
                    break stream;
                }
                assert!(Instant::now() < deadline, "forward did not start");
                assert!(child.try_wait().unwrap().is_none(), "forward exited");
                std::thread::sleep(Duration::from_millis(20));
            };
            target.set_nonblocking(true).unwrap();
            let (mut incoming, _) = loop {
                match target.accept() {
                    Ok(s) => break s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline);
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            incoming.write_all(b"forwarded").unwrap();
            let mut data = [0; 9];
            stream.read_exact(&mut data).unwrap();
            assert_eq!(&data, b"forwarded");
            child.kill().unwrap();
            child.wait().unwrap();
            drop(stream);
            drop(incoming);
            let end = Instant::now() + Duration::from_secs(3);
            while TcpStream::connect(("127.0.0.1", port)).is_ok() {
                assert!(Instant::now() < end, "forward survived its owner");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

#[cfg(test)]
mod validation_tests {
    use super::*;
    #[test]
    fn remote_snapshots_and_transfer_batches_reject_hostile_input() {
        assert!(parse_workspace(b"null").unwrap().is_none());
        for (root, definition) in [
            ("relative", "presets=['rust']"),
            ("/project", "[tasks.x]\ncommand=['pwd']\ncwd='../escape'"),
            ("/project", "[tasks.x]\ncommand=['pwd']\ncwd='/escape'"),
            ("/project", "[tasks.x]\ncommand=[\"pwd\\nnext\"]"),
        ] {
            let bytes =
                serde_json::to_vec(&serde_json::json!({"root":root,"definition":definition,"package":null,"locks":[]}))
                    .unwrap();
            assert!(parse_workspace(&bytes).is_err(), "{definition}");
        }
        assert!(sftp_path("path\n!touch /tmp/no").is_err());
        assert_eq!(sftp_path("-filename").unwrap(), "\"./-filename\"");
        let root = std::env::temp_dir().join(format!("proteus-ssh-batch-{}", std::process::id()));
        let batch = Batch::in_directory(&root, true, Path::new("/source"), "destination").unwrap();
        let path = batch.path.clone();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        drop(batch);
        assert!(!path.exists());
        assert!(Batch::in_directory(&root, true, Path::new("/source"), &"x".repeat(8192)).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
