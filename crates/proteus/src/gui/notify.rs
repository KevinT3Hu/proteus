//! Desktop notifications over D-Bus (`org.freedesktop.Notifications`).
//!
//! A worker thread owns the session-bus connection (opened on the first
//! notification, so startup does not pay for it). A second thread listens
//! for clicks: `ActivationToken` (an xdg-activation token, spec 1.2) then
//! `ActionInvoked` reach the event loop as [`Msg::NotifyActivated`].

use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use smithay_client_toolkit::reexports::calloop::channel::Sender;
use zbus::MatchRule;
use zbus::blocking::{Connection, MessageIterator};
use zbus::message::Type;
use zbus::zvariant::Value;

use super::Msg;
use crate::layout::PaneId;

const DEST: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";

struct Req {
    pane: PaneId,
    summary: String,
    body: String,
}

#[derive(Default)]
struct Shared {
    /// Notification id → pane.
    panes: HashMap<u32, PaneId>,
    /// Activation tokens that arrived before their `ActionInvoked`.
    tokens: HashMap<u32, String>,
}

pub struct Notifier {
    tx: Option<mpsc::Sender<Req>>,
    events: Sender<Msg>,
    failed: bool,
}

impl Notifier {
    pub fn new(events: Sender<Msg>) -> Self {
        Self { tx: None, events, failed: false }
    }

    /// Show a notification; clicking it focuses `pane`. Errors (no bus, no
    /// notification daemon) are reported once on stderr.
    pub fn send(&mut self, pane: PaneId, summary: String, body: String) {
        if self.failed {
            return;
        }
        if self.tx.is_none() {
            let (tx, rx) = mpsc::channel();
            let events = self.events.clone();
            if std::thread::Builder::new().name("notify".into()).spawn(move || worker(rx, events)).is_err() {
                self.failed = true;
                return;
            }
            self.tx = Some(tx);
        }
        if let Some(tx) = &self.tx
            && tx.send(Req { pane, summary, body }).is_err()
        {
            self.failed = true;
        }
    }
}

fn worker(rx: mpsc::Receiver<Req>, events: Sender<Msg>) {
    let conn = match Connection::session() {
        Ok(c) => c,
        Err(e) => {
            return eprintln!("proteus: notifications: cannot connect to the session bus: {e}");
        }
    };
    let shared = Arc::new(Mutex::new(Shared::default()));
    {
        let (conn, shared) = (conn.clone(), shared.clone());
        let _ =
            std::thread::Builder::new().name("notify-signals".into()).spawn(move || listen(&conn, &shared, &events));
    }
    let mut warned = false;
    for req in rx {
        let mut hints: HashMap<&str, Value> = HashMap::new();
        hints.insert("desktop-entry", Value::from(super::APP_ID));
        hints.insert("urgency", Value::U8(1));
        let actions = vec!["default", "Show"];
        let body = (
            super::APP_NAME,
            0u32,
            "utilities-terminal",
            req.summary.as_str(),
            req.body.as_str(),
            actions,
            hints,
            -1i32,
        );
        match conn
            .call_method(Some(DEST), PATH, Some(DEST), "Notify", &body)
            .and_then(|m| m.body().deserialize::<u32>())
        {
            Ok(id) => {
                if let Ok(mut s) = shared.lock() {
                    s.panes.insert(id, req.pane);
                }
            }
            Err(e) if !warned => {
                warned = true;
                eprintln!("proteus: notifications: {e}");
            }
            Err(_) => {}
        }
    }
}

fn listen(conn: &Connection, shared: &Mutex<Shared>, events: &Sender<Msg>) {
    let rule = match MatchRule::builder().msg_type(Type::Signal).interface(DEST).and_then(|b| b.path(PATH)) {
        Ok(b) => b.build(),
        Err(_) => return,
    };
    let Ok(iter) = MessageIterator::for_match_rule(rule, conn, Some(64)) else {
        return;
    };
    for msg in iter.flatten() {
        let header = msg.header();
        let Some(member) = header.member() else {
            continue;
        };
        let Ok(mut s) = shared.lock() else { return };
        match member.as_str() {
            "ActivationToken" => {
                if let Ok((id, token)) = msg.body().deserialize::<(u32, String)>() {
                    s.tokens.insert(id, token);
                }
            }
            "ActionInvoked" => {
                if let Ok((id, _action)) = msg.body().deserialize::<(u32, String)>()
                    && let Some(&pane) = s.panes.get(&id)
                {
                    let token = s.tokens.remove(&id);
                    if events.send(Msg::NotifyActivated(pane, token)).is_err() {
                        return;
                    }
                }
            }
            "NotificationClosed" => {
                if let Ok((id, _reason)) = msg.body().deserialize::<(u32, u32)>() {
                    s.panes.remove(&id);
                    s.tokens.remove(&id);
                }
            }
            _ => {}
        }
    }
}
