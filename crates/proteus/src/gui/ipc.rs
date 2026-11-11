//! Dispatch control requests on the GUI thread through the same host as Lua.
use super::App;
use crate::ipc::{Call, Request};
use crate::layout::Axis;
use crate::script::{Host, Spawn};
use serde_json::{Value, json};

impl App {
    pub(super) fn ipc_call(&mut self, call: Call) {
        if std::time::Instant::now() >= call.deadline {
            let _ = call.reply.send(Err("request expired before execution".into()));
            return;
        }
        let result = self.ipc_request(call.request);
        let _ = call.reply.send(result);
    }

    fn ipc_pane(&self, pane: Option<u64>) -> Result<u64, String> {
        let id = pane.or_else(|| self.focused()).ok_or("no focused pane")?;
        if !self.panes.contains_key(&id) {
            return Err(format!("no pane {id}"));
        }
        Ok(id)
    }

    fn ipc_request(&mut self, request: Request) -> Result<Value, String> {
        match request {
            Request::List {} => Ok(Value::Array(
                self.pane_ids()
                    .into_iter()
                    .filter_map(|id| {
                        let p = self.pane_info(id)?;
                        Some(json!({"id":p.id,"title":p.title,"name":p.name,"cwd":p.cwd,"remote":p.remote,
                    "process":p.process,"command":p.command,"window":p.window,"tab":p.tab,
                    "focused":p.focused,"floating":p.floating,"cols":p.cols,"rows":p.rows}))
                    })
                    .collect(),
            )),
            Request::SendText { pane, text, paste } => {
                let id = self.ipc_pane(pane)?;
                self.send_text(id, &text, paste);
                Ok(json!({"pane":id}))
            }
            Request::Read { pane, scrollback } => {
                let id = self.ipc_pane(pane)?;
                if scrollback > 100_000 {
                    return Err("scrollback limit is 100000 rows".into());
                }
                if self.panes[&id].private {
                    return Err("pane is private".into());
                }
                let t = &self.panes[&id].term;
                let grid = t.grid();
                let first = grid.abs_line(0).saturating_sub(scrollback as u64).max(grid.first_line());
                let mut bytes = 0usize;
                for line in first..grid.end_line() {
                    let Some(row) = grid.abs_row(line) else {
                        continue;
                    };
                    for cell in row.cells() {
                        bytes = bytes.saturating_add(cell.ch().len_utf8() + cell.extra().map_or(0, str::len));
                    }
                    bytes = bytes.saturating_add(1);
                    if bytes > 2 << 20 {
                        return Err("requested text exceeds the 2 MiB limit".into());
                    }
                }
                Ok(json!(self.read_screen(id, scrollback)))
            }
            Request::Focus { pane } => {
                let id = self.ipc_pane(Some(pane))?;
                self.focus(id);
                Ok(json!({"pane":id}))
            }
            Request::Close { pane } => {
                let id = self.ipc_pane(Some(pane))?;
                self.close(id);
                Ok(json!({"pane":id}))
            }
            Request::Action { pane, id } => {
                let action = crate::modes::Action::from_name(&id).ok_or_else(|| format!("unknown action: {id}"))?;
                if let Some(pane) = pane {
                    let pane = self.ipc_pane(Some(pane))?;
                    self.focus(pane);
                } else {
                    self.activate_kbd();
                }
                self.dispatch(action);
                Ok(Value::Null)
            }
            Request::Split { pane, axis } => {
                let pane = self.ipc_pane(pane)?;
                let axis = match axis.as_deref() {
                    None => None,
                    Some("x") => Some(Axis::X),
                    Some("y") => Some(Axis::Y),
                    _ => return Err("axis must be x or y".into()),
                };
                Host::split(self, pane, axis, Spawn::default()).map(|id| json!({"pane":id}))
            }
            Request::NewTab { cwd, command } => {
                if cwd.as_ref().is_some_and(|p| !std::path::Path::new(p).is_dir()) {
                    return Err("cwd must be an existing directory".into());
                }
                let cmd = if command.is_empty() { None } else { Some(crate::spec::Cmd::Argv(command)) };
                Host::new_tab(self, Spawn { cwd, cmd }).map(|id| json!({"pane":id}))
            }
        }
    }
}
