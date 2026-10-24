//! Test plugin: does what an `action` event names and reports with `notify`.

use proteus_plugin::types::{Event, Frame, SurfaceId, SurfaceKind, SurfaceOptions, UiEvent};
use proteus_plugin::{Guest, host, text};

struct Fixture;

fn report(what: &str, r: Result<impl std::fmt::Debug, impl std::fmt::Debug>) {
    host::notify(&format!("{what}: {r:?}"), false);
}

impl Guest for Fixture {
    fn init() -> Result<(), String> {
        if std::fs::read_to_string("/data/fail-init").is_ok() {
            return Err("asked to fail".into());
        }
        host::register_action("fixture", "Fixture action", None)?;
        host::notify("init", false);
        Ok(())
    }

    fn on_event(event: Event) {
        match event {
            Event::Action(a) => match a.as_str() {
                "fixture" => host::notify("fixture invoked", false),
                "config" => {
                    let cfg: proteus_plugin::serde_json::Value = host::config().unwrap();
                    host::notify(&format!("config: {} {}", cfg["position"], cfg["width"]), false);
                }
                "locale" => {
                    let mut catalog = proteus_plugin::i18n::catalog().unwrap();
                    catalog.add_resource("zh", "hello = 你好 { $name }").unwrap();
                    let mut args = proteus_plugin::i18n::FluentArgs::new();
                    args.set("name", "WASM");
                    host::notify(&catalog.format("hello", Some(&args)).unwrap(), false);
                    catalog.add_resource("pl", "files = { $count ->\n    [one] one\n    [few] few\n    [many] many\n   *[other] other\n}\n").unwrap();
                    catalog.set_locale("pl-PL").unwrap();
                    let mut args = proteus_plugin::i18n::FluentArgs::new();
                    args.set("count", 1.5);
                    host::notify(&format!("WASM plural: {}", catalog.format("files", Some(&args)).unwrap()), false);
                }
                "loop" =>
                {
                    #[allow(clippy::empty_loop)]
                    loop {}
                }
                "host-flood" => {
                    for _ in 0..100_000 { host::log("queued payload"); }
                }
                "storage-set" => report("storage-set", host::storage_set("key", "persisted")),
                "storage-read" => host::notify(&format!("storage: {:?}", host::storage_get("key")), false),
                "storage-keys" => host::notify(&format!("keys: {:?}", host::storage_keys()), false),
                "storage-delete" => {
                    host::storage_delete("key");
                    host::notify("delete returned", false);
                }
                "panic" => panic!("on purpose"),
                "alloc" => {
                    let v: Vec<u8> = vec![1; 1 << 30];
                    host::notify(&format!("allocated {}", v.len()), false);
                }
                "panes" => report("panes", host::list_panes().map(|p| p.len())),
                "type" => report("type", host::send_text(1, "x", false)),
                "screen" => report("screen", host::read_screen(1, 0)),
                "fs" => {
                    report("write", std::fs::write("/data/x", "hi"));
                    report("read", std::fs::read_to_string("/data/x"));
                    report("etc", std::fs::read_to_string("/etc/hostname").map(|_| ()).map_err(|e| e.kind()));
                }
                "work" => std::thread::sleep(std::time::Duration::from_millis(5)),
                "sentinel" => host::notify("sentinel", false),
                "modes" => {
                    host::create_private("/data/private", false).unwrap();
                    report("private mode", host::file_mode("/data/private"));
                    report("chmod", host::set_file_mode("/data/private", 0o700));
                    report("new mode", host::file_mode("/data/private"));
                    report("outside grant", host::set_file_mode("/etc/hostname", 0o600));
                }
                "timer" => {
                    host::set_timer(20, false);
                }
                "surface-title" => report("title", host::set_surface_title(7, "文件")),
                "surface" => {
                    let o = SurfaceOptions {
                        kind: SurfaceKind::SidebarRight,
                        title: "t".into(),
                        cols: 20,
                        rows: 0,
                        focus: false,
                        pane: None,
                    };
                    let s = host::open_surface(&o);
                    if let Ok(s) = s {
                        let _ = host::draw(
                            s,
                            &Frame {
                                lines: vec![vec![text("hello")]],
                                cursor: None,
                                background: None,
                                images: vec![],
                                accessibility: vec![],
                            },
                        );
                    }
                    report("surface", s);
                }
                _ => host::notify(&format!("unknown {a}"), true),
            },
            Event::Timer(id) => host::notify(&format!("timer {id}"), false),
            Event::PaneOpened(p) => host::notify(&format!("opened {p}"), false),
            _ => {}
        }
    }

    fn on_ui(s: SurfaceId, e: UiEvent) {
        host::notify(&format!("ui {s} {e:?}"), false);
    }
}

proteus_plugin::export!(Fixture);
