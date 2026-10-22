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
        host::register_action("fixture", None)?;
        host::notify("init", false);
        Ok(())
    }

    fn on_event(event: Event) {
        match event {
            Event::Action(a) => match a.as_str() {
                "loop" =>
                {
                    #[allow(clippy::empty_loop)]
                    loop {}
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
                "timer" => {
                    host::set_timer(20, false);
                }
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
                            &Frame { lines: vec![vec![text("hello")]], cursor: None, background: None, images: vec![] },
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
