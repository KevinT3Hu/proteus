//! Runs the fixture plugin (tests/fixture, built for wasm32-wasip2).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use proteus_wasm::types::{Event, PaneInfo};
use proteus_wasm::{Call, FromPlugin, Key, Permissions, Reply, Runtime, Spec};

/// The fixture component, or `None` if the wasm32-wasip2 target is missing.
fn fixture() -> Option<&'static [u8]> {
    static WASM: OnceLock<Option<Vec<u8>>> = OnceLock::new();
    WASM.get_or_init(|| {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixture");
        let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/plugin-fixture");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let sysroot = Command::new("rustc").args(["--print", "sysroot"]).output().ok()?;
        let sysroot = PathBuf::from(String::from_utf8_lossy(&sysroot.stdout).trim());
        if !sysroot.join("lib/rustlib/wasm32-wasip2").is_dir() {
            eprintln!("skipping: no wasm32-wasip2 target (rustup target add wasm32-wasip2)");
            return None;
        }
        let ok = Command::new(cargo)
            .args(["build", "--quiet", "--target", "wasm32-wasip2", "--manifest-path"])
            .arg(dir.join("Cargo.toml"))
            .arg("--target-dir")
            .arg(&target)
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "building tests/fixture failed");
        std::fs::read(target.join("wasm32-wasip2/debug/fixture.wasm")).ok()
    })
    .as_deref()
}

fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| Runtime::new().unwrap())
}

/// A fake terminal: answers calls, records the rest.
struct Host {
    rx: mpsc::Receiver<(Key, FromPlugin)>,
    handle: proteus_wasm::Handle,
    data: PathBuf,
}

fn start(name: &str, granted: Permissions) -> Option<Host> {
    let wasm = fixture()?.to_vec();
    let data = std::env::temp_dir().join(format!("proteus-fixture-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    if name == "fail" {
        std::fs::write(data.join("fail-init"), "").unwrap();
    }
    let (tx, rx) = mpsc::channel();
    let tx = std::sync::Mutex::new(tx);
    let sink: proteus_wasm::Sink = Arc::new(move |k, m| {
        let _ = tx.lock().unwrap().send((k, m));
    });
    let spec = Spec {
        key: Key { id: name.into(), instance: 1 },
        hash: proteus_wasm::store::sha256(&wasm),
        wasm,
        granted,
        data_dir: data.clone(),
        storage: data.with_extension("storage.toml"),
        memory: 64 << 20,
        stderr: false,
        compiled: Some(data.with_extension("compiled")),
    };
    let handle = runtime().start(spec, sink).unwrap();
    Some(Host { rx, handle, data })
}

impl Host {
    /// Messages until `until` matches one (answering calls on the way).
    fn wait(&self, until: impl Fn(&FromPlugin) -> bool) -> Vec<String> {
        let mut seen = Vec::new();
        let end = Instant::now() + Duration::from_secs(30);
        loop {
            let left = end.saturating_duration_since(Instant::now());
            let (_, m) = self.rx.recv_timeout(left).unwrap_or_else(|_| panic!("timed out; saw {seen:?}"));
            let done = until(&m);
            match m {
                FromPlugin::Call(c, reply) => {
                    let answer = match &c {
                        Call::ListPanes => {
                            Ok(Reply::Panes(vec![PaneInfo { id: 1, title: "sh".into(), ..pane() }, pane()]))
                        }
                        Call::SendText { .. } => Ok(Reply::Unit),
                        Call::ReadScreen { .. } => Ok(Reply::Text("$ ".into())),
                        Call::OpenSurface(_) => Ok(Reply::Surface(7)),
                        _ => Ok(Reply::Unit),
                    };
                    if let Some(r) = reply {
                        let _ = r.send(answer);
                    }
                    match c {
                        Call::Notify { text, .. } => seen.push(text),
                        Call::Draw(s, f) => seen.push(format!("draw {s} {}", f.lines[0][0].text)),
                        Call::RegisterAction(a, _) => seen.push(format!("action {a}")),
                        _ => {}
                    }
                }
                FromPlugin::Started => seen.push("started".into()),
                FromPlugin::Failed(e) => seen.push(format!("failed: {e}")),
                FromPlugin::Exited => seen.push("exited".into()),
            }
            if done {
                return seen;
            }
        }
    }

    fn act(&self, a: &str) -> Vec<String> {
        self.act_n(a, 1)
    }

    /// Run action `a`, collecting until `n` notifications (or a failure).
    fn act_n(&self, a: &str, n: usize) -> Vec<String> {
        self.handle.event(Event::Action(a.into()));
        let count = std::cell::Cell::new(0);
        self.wait(|m| match m {
            FromPlugin::Call(Call::Notify { .. }, _) => {
                count.set(count.get() + 1);
                count.get() == n
            }
            FromPlugin::Failed(_) => true,
            _ => false,
        })
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.data);
    }
}

fn pane() -> PaneInfo {
    PaneInfo {
        id: 2,
        title: String::new(),
        name: None,
        cwd: None,
        process: None,
        command: None,
        window: 1,
        tab: 1,
        focused: false,
        floating: false,
        cols: 80,
        rows: 24,
    }
}

#[test]
fn permissions_are_enforced() {
    let granted = Permissions { read_panes: true, ui: true, ..Permissions::default() };
    let Some(h) = start("perms", granted) else { return };
    assert_eq!(h.wait(|m| matches!(m, FromPlugin::Started)), ["action fixture", "init", "started"]);
    assert_eq!(h.act("panes"), ["panes: Ok(2)"]);
    assert_eq!(h.act("type"), [r#"type: Err("permission denied: write-pty")"#]);
    assert_eq!(h.act("screen"), [r#"screen: Err("permission denied: read-screen")"#]);
    assert_eq!(h.act("surface"), ["draw 7 hello", "surface: Ok(7)"]);
    // Its data directory works; the rest of the file system is not there.
    assert_eq!(h.act_n("fs", 3), ["write: Ok(())", r#"read: Ok("hi")"#, "etc: Err(NotFound)"]);
    assert_eq!(std::fs::read_to_string(h.data.join("x")).unwrap(), "hi");
    h.handle.event(Event::PaneOpened(5));
    assert_eq!(h.wait(|m| matches!(m, FromPlugin::Call(Call::Notify { .. }, _))), ["opened 5"]);
    assert_eq!(h.act("timer"), ["timer 1"]);
}

#[test]
fn events_need_permissions() {
    let Some(h) = start("noperm", Permissions::default()) else { return };
    // Without `ui` the action cannot be registered: init fails.
    let seen = h.wait(|m| matches!(m, FromPlugin::Failed(_)));
    assert_eq!(seen, [r#"failed: init failed: permission denied: ui"#]);
}

#[test]
fn failures_stop_the_instance() {
    let ui = Permissions { ui: true, ..Permissions::default() };
    let Some(h) = start("fail", ui.clone()) else { return };
    assert_eq!(h.wait(|m| matches!(m, FromPlugin::Failed(_))), ["failed: init failed: asked to fail"]);

    let h = start("panic", ui.clone()).unwrap();
    h.wait(|m| matches!(m, FromPlugin::Started));
    let seen = h.act("panic");
    assert!(seen.last().unwrap().starts_with("failed: crashed"), "{seen:?}");

    let h = start("alloc", ui.clone()).unwrap();
    h.wait(|m| matches!(m, FromPlugin::Started));
    let seen = h.act("alloc");
    assert!(seen.last().unwrap().starts_with("failed:"), "{seen:?}");

    let h = start("loop", ui.clone()).unwrap();
    h.wait(|m| matches!(m, FromPlugin::Started));
    let t = Instant::now();
    let seen = h.act("loop");
    assert!(seen.last().unwrap().contains("ran longer than"), "{seen:?}");
    assert!(t.elapsed() < proteus_wasm::runtime::CALL_LIMIT + Duration::from_secs(2));

    // Stopping interrupts a busy plugin at once.
    let h = start("stop", ui).unwrap();
    h.wait(|m| matches!(m, FromPlugin::Started));
    h.handle.event(Event::Action("loop".into()));
    std::thread::sleep(Duration::from_millis(100));
    let t = Instant::now();
    h.handle.stop();
    let seen = h.wait(|m| matches!(m, FromPlugin::Failed(_) | FromPlugin::Exited));
    assert_eq!(seen, ["failed: stopped"]);
    assert!(t.elapsed() < Duration::from_secs(1));
}
