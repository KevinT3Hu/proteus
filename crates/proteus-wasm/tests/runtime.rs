//! Runs the fixture plugin (tests/fixture, built for wasm32-wasip2).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use proteus_wasm::types::{Event, PaneInfo};
use proteus_wasm::{Ack, Call, FromPlugin, Key, Permissions, Reply, Runtime, Spec};

/// A real fixture component. Missing prerequisites fail rather than skip tests.
fn fixture() -> &'static [u8] {
    static WASM: OnceLock<Vec<u8>> = OnceLock::new();
    WASM.get_or_init(|| {
        if let Some(path) = std::env::var_os("PROTEUS_WASM_FIXTURE") {
            let bytes = std::fs::read(path).expect("read explicit fixture component");
            assert!(bytes.starts_with(b"\0asm"), "fixture must be WebAssembly");
            return bytes;
        }
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixture");
        let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/plugin-fixture");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let sysroot = Command::new(rustc).args(["--print", "sysroot"]).output().expect("locate Rust sysroot");
        let sysroot = PathBuf::from(String::from_utf8_lossy(&sysroot.stdout).trim());
        assert!(
            sysroot.join("lib/rustlib/wasm32-wasip2/lib").is_dir(),
            "wasm32-wasip2 is required: rustup target add wasm32-wasip2, or set PROTEUS_WASM_FIXTURE to a built fixture component"
        );
        let ok = Command::new(cargo)
            .args(["build", "--release", "--quiet", "--locked", "--target", "wasm32-wasip2", "--manifest-path"])
            .arg(dir.join("Cargo.toml"))
            .arg("--target-dir")
            .arg(&target)
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "building tests/fixture failed");
        std::fs::read(target.join("wasm32-wasip2/release/fixture.wasm")).expect("read built fixture component")
    })
    .as_slice()
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
    storage: PathBuf,
    root: PathBuf,
}

fn start(name: &str, granted: Permissions) -> Host {
    let wasm = fixture().to_vec();
    let root = std::env::temp_dir().join(format!("proteus-fixture-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let data = root.join("data");
    let storage = root.join("state.storage.toml");
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
        config: serde_json::json!({"position": "right", "width": 40}),
        key: Key { id: name.into(), instance: 1 },
        hash: proteus_wasm::source::sha256(&wasm),
        wasm,
        granted,
        data_dir: data.clone(),
        storage: storage.clone(),
        memory: 64 << 20,
        compiled: Some(data.with_extension("compiled")),
    };
    let handle = runtime().start(spec, sink).unwrap();
    Host { rx, handle, data, storage, root }
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
                        Call::Locale => Ok(Reply::Text("zh-CN".into())),
                        Call::ListPanes => {
                            Ok(Reply::Panes(vec![PaneInfo { id: 1, title: "sh".into(), ..pane() }, pane()]))
                        }
                        Call::SendText { .. } => Ok(Reply::Unit),
                        Call::ReadScreen { .. } => Ok(Reply::Text("$ ".into())),
                        Call::OpenSurface(_) => Ok(Reply::Surface(7)),
                        _ => Ok(Reply::Unit),
                    };
                    if let Ack::Answer(r) = reply {
                        let _ = r.send(answer);
                    }
                    match c {
                        Call::Notify { text, .. } => seen.push(text),
                        Call::Draw(s, f) => seen.push(format!("draw {s} {}", f.lines[0][0].text)),
                        Call::SetSurfaceTitle(s, title) => seen.push(format!("title {s} {title}")),
                        Call::RegisterAction { id, name, .. } => seen.push(format!("action {id} ({name})")),
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
        let _ = std::fs::remove_dir_all(&self.root);
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
    let h = start("perms", granted);
    assert_eq!(h.wait(|m| matches!(m, FromPlugin::Started)), ["action fixture (Fixture action)", "init", "started"]);
    assert_eq!(h.act("fixture"), ["fixture invoked"]);
    assert_eq!(h.act("panes"), ["panes: Ok(2)"]);
    assert_eq!(h.act("type"), [r#"type: Err("permission denied: write-pty")"#]);
    assert_eq!(h.act("screen"), [r#"screen: Err("permission denied: read-screen")"#]);
    assert_eq!(h.act("surface"), ["draw 7 hello", "surface: Ok(7)"]);
    assert_eq!(h.act("surface-title"), ["title 7 文件", "title: Ok(())"]);
    // Its data directory works; the rest of the file system is not there.
    assert_eq!(h.act_n("fs", 3), ["write: Ok(())", r#"read: Ok("hi")"#, "etc: Err(NotFound)"]);
    assert_eq!(std::fs::read_to_string(h.data.join("x")).unwrap(), "hi");
    h.handle.event(Event::PaneOpened(5));
    assert_eq!(h.wait(|m| matches!(m, FromPlugin::Call(Call::Notify { .. }, _))), ["opened 5"]);
    assert_eq!(h.act("timer"), ["timer 1"]);
}

#[test]
fn events_need_permissions() {
    let h = start("noperm", Permissions::default());
    // Without `ui` the action cannot be registered: init fails.
    let seen = h.wait(|m| matches!(m, FromPlugin::Failed(_)));
    assert_eq!(seen, [r#"failed: init failed: permission denied: ui"#]);
}

#[test]
fn failures_stop_the_instance() {
    let ui = Permissions { ui: true, ..Permissions::default() };
    let h = start("fail", ui.clone());
    assert_eq!(h.wait(|m| matches!(m, FromPlugin::Failed(_))), ["failed: init failed: asked to fail"]);

    let h = start("panic", ui.clone());
    h.wait(|m| matches!(m, FromPlugin::Started));
    let seen = h.act("panic");
    let failed = seen.last().unwrap();
    assert!(failed.starts_with("failed: crashed: panicked at src/lib.rs:"), "{seen:?}");
    assert!(failed.ends_with(": fixture panic"), "{seen:?}");

    let h = start("alloc", ui.clone());
    h.wait(|m| matches!(m, FromPlugin::Started));
    let seen = h.act("alloc");
    assert!(seen.last().unwrap().starts_with("failed: crashed: memory allocation of"), "{seen:?}");

    let h = start("loop", ui.clone());
    h.wait(|m| matches!(m, FromPlugin::Started));
    let t = Instant::now();
    let seen = h.act("loop");
    assert!(seen.last().unwrap().contains("ran longer than"), "{seen:?}");
    assert!(t.elapsed() < proteus_wasm::runtime::CALL_LIMIT + Duration::from_secs(2));

    // Stopping interrupts a busy plugin at once.
    let h = start("stop", ui);
    h.wait(|m| matches!(m, FromPlugin::Started));
    h.handle.event(Event::Action("loop".into()));
    std::thread::sleep(Duration::from_millis(100));
    let t = Instant::now();
    h.handle.stop();
    let seen = h.wait(|m| matches!(m, FromPlugin::Failed(_) | FromPlugin::Exited));
    assert_eq!(seen, ["failed: stopped"]);
    assert!(t.elapsed() < Duration::from_secs(1));

    // A plugin asleep in a WASI call is held to the same limits.
    let h = start("sleep", Permissions { ui: true, ..Permissions::default() });
    h.wait(|m| matches!(m, FromPlugin::Started));
    let t = Instant::now();
    let seen = h.act("sleep");
    assert!(seen.last().unwrap().contains("ran longer than"), "{seen:?}");
    assert!(t.elapsed() < proteus_wasm::runtime::CALL_LIMIT + Duration::from_secs(2));

    let h = start("sleep-stop", Permissions { ui: true, ..Permissions::default() });
    h.wait(|m| matches!(m, FromPlugin::Started));
    h.handle.event(Event::Action("sleep".into()));
    std::thread::sleep(Duration::from_millis(100));
    let t = Instant::now();
    h.handle.stop();
    let seen = h.wait(|m| matches!(m, FromPlugin::Failed(_) | FromPlugin::Exited));
    assert_eq!(seen, ["failed: stopped"]);
    assert!(t.elapsed() < Duration::from_secs(1));
}

#[test]
fn queued_events_do_not_starve_timers_and_file_modes_are_scoped() {
    let h = start("fairness", Permissions { ui: true, ..Default::default() });
    h.wait(|m| matches!(m, FromPlugin::Started));
    h.handle.event(Event::Action("timer".into()));
    for _ in 0..30 {
        h.handle.event(Event::Action("work".into()));
    }
    h.handle.event(Event::Action("sentinel".into()));
    let seen = h.wait(|m| matches!(m, FromPlugin::Call(Call::Notify { text, .. }, _) if text == "sentinel"));
    assert_eq!(seen, ["timer 1", "sentinel"]);
    assert_eq!(
        h.act_n("modes", 4),
        [
            "private mode: Ok(384)",
            "chmod: Ok(())",
            "new mode: Ok(448)",
            "outside grant: Err(\"/etc/hostname: not inside the plugin's directories (permission `fs`)\")"
        ]
    );
}

#[test]
fn sdk_translates_using_the_host_locale() {
    let h = start("locale", Permissions { ui: true, ..Permissions::default() });
    h.wait(|m| matches!(m, FromPlugin::Started));
    h.handle.event(Event::Action("locale".into()));
    let seen = h.wait(|m| matches!(m, FromPlugin::Call(Call::Notify { text, .. }, _) if text == "你好 WASM"));
    assert!(seen.iter().any(|s| s == "你好 WASM"));
    let seen = h.wait(|m| matches!(m, FromPlugin::Call(Call::Notify { text, .. }, _) if text == "WASM plural: other"));
    assert!(seen.iter().any(|s| s == "WASM plural: other"));
}

#[test]
fn plugin_reads_its_own_config() {
    let h = start("config", Permissions { ui: true, ..Permissions::default() });
    h.wait(|m| matches!(m, FromPlugin::Started));
    assert_eq!(h.act("config"), ["config: \"right\" 40"]);
}

#[test]
fn host_flood_is_bounded_by_unhandled_calls_and_stop_interrupts_backpressure() {
    let h = start("host-flood", Permissions { ui: true, ..Default::default() });
    h.wait(|m| matches!(m, FromPlugin::Started));
    h.handle.event(Event::Action("host-flood".into()));
    // Logs need no answer: the plugin sends several before the host handles any.
    let mut held = Vec::new();
    for _ in 0..64 {
        let (_, message) = h.rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let FromPlugin::Call(Call::Log(_), Ack::Done(credit)) = message else { panic!("expected an unanswered log") };
        held.push(credit);
    }
    // None of them handled yet: no more may queue.
    assert!(matches!(h.rx.recv_timeout(Duration::from_millis(100)), Err(mpsc::RecvTimeoutError::Timeout)));
    // Handling one lets one more through.
    held.pop();
    let (_, message) = h.rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let FromPlugin::Call(Call::Log(_), Ack::Done(credit)) = message else { panic!("expected an unanswered log") };
    held.push(credit);
    assert!(matches!(h.rx.recv_timeout(Duration::from_millis(100)), Err(mpsc::RecvTimeoutError::Timeout)));
    h.handle.stop();
    let (_, message) = h.rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(matches!(message, FromPlugin::Failed(_) | FromPlugin::Exited));
    drop(held);
}

#[test]
fn storage_corruption_is_preserved_and_legacy_api_errors_reach_the_host() {
    let h = start("storage", Permissions { ui: true, ..Default::default() });
    h.wait(|m| matches!(m, FromPlugin::Started));
    assert_eq!(h.act("storage-set"), ["storage-set: Ok(())"]);
    assert_eq!(h.act("storage-read"), ["storage: Some(\"persisted\")"]);
    let path = &h.storage;
    let malformed = "not = [valid";
    std::fs::write(path, malformed).unwrap();
    assert!(h.act("storage-set")[0].starts_with("storage-set: Err("));
    let seen = h.act_n("storage-delete", 2);
    assert!(seen[0].contains("malformed TOML"));
    assert_eq!(seen[1], "delete returned");
    let seen = h.act_n("storage-read", 2);
    assert!(seen[0].contains("malformed TOML"));
    assert_eq!(seen[1], "storage: None");
    let seen = h.act_n("storage-keys", 2);
    assert!(seen[0].contains("malformed TOML"));
    assert_eq!(seen[1], "keys: []");
    assert_eq!(std::fs::read_to_string(path).unwrap(), malformed);
    std::fs::write(path, "other = 'kept'\n").unwrap();
    assert_eq!(h.act("storage-set"), ["storage-set: Ok(())"]);
    assert_eq!(h.act("storage-keys"), ["keys: [\"key\", \"other\"]"]);
    assert_eq!(h.act("storage-delete"), ["delete returned"]);
    assert_eq!(h.act("storage-read"), ["storage: None"]);
}

#[test]
fn event_floods_are_filtered_coalesced_and_dropped_without_stopping_the_plugin() {
    let h = start("event-flood", Permissions { ui: true, read_panes: true, ..Default::default() });
    h.wait(|m| matches!(m, FromPlugin::Started));
    h.handle.event(Event::Action("busy".into()));
    std::thread::sleep(Duration::from_millis(50));
    for i in 0..1000 {
        h.handle.event(Event::Bell(1));
        h.handle.event(Event::TitleChanged((1, format!("t{i}"))));
    }
    for _ in 0..1000 {
        h.handle.event(Event::ConfigChanged);
    }
    assert!(h.handle.dropped() > 0);
    std::thread::sleep(Duration::from_millis(500));
    h.handle.event(Event::Action("sentinel".into()));
    let seen = h.wait(|m| {
        matches!(m, FromPlugin::Call(Call::Notify { text, .. }, _) if text == "sentinel")
            || matches!(m, FromPlugin::Failed(_) | FromPlugin::Exited)
    });
    assert_eq!(seen, ["title 1 t999", "sentinel"]);
    assert_eq!(h.act("fixture"), ["fixture invoked"]);

    // Without `read-panes` pane events never take a slot.
    let h = start("event-filter", Permissions { ui: true, ..Default::default() });
    h.wait(|m| matches!(m, FromPlugin::Started));
    for i in 0..1000 {
        h.handle.event(Event::TitleChanged((i, "x".into())));
        h.handle.event(Event::CommandStarted((i, Some("secret".into()))));
    }
    assert_eq!(h.handle.dropped(), 0);
    assert_eq!(h.act("fixture"), ["fixture invoked"]);
}
