//! UI exposed directly to init.lua and its ordinary require() modules.
use super::{App, plugins::CONFIG};
use proteus_wasm::{
    Key,
    types::{self, Frame, UiEvent},
};

impl App {
    // --- surfaces owned by the config (`p.ui`) ------------------------------------------

    fn config_key(&self) -> Key {
        Key { id: CONFIG.into(), instance: self.sc.generation as u64 }
    }

    fn config_surface(&self, id: u32) -> Result<(), String> {
        let k = self.config_key();
        if self.pl.surfaces.get(&id).is_some_and(|s| s.owner == k) { Ok(()) } else { Err(format!("no surface {id}")) }
    }

    pub(super) fn config_surface_open(&mut self, o: types::SurfaceOptions) -> Result<u32, String> {
        let k = self.config_key();
        let r = self.open_surface(&k, o);
        // Lua draws on its first `resize` (sidebars and floats got theirs
        // in the layout).
        if let Ok(id) = r
            && self.pl.surfaces.get(&id).is_some_and(|s| s.pane.is_some())
        {
            let size = self.pl.surfaces.get(&id).map(|s| s.size).unwrap_or_default();
            self.sc
                .queue
                .push_back(super::scripting::ScriptCall::Ui(id, UiEvent::Resize((size.0 as u32, size.1 as u32))));
        }
        r
    }

    pub(super) fn config_surface_draw(&mut self, id: u32, frame: Frame) -> Result<(), String> {
        self.config_surface(id)?;
        self.show_frame(id, frame)
    }

    pub(super) fn config_surface_close(&mut self, id: u32) {
        if self.config_surface(id).is_ok() {
            self.drop_surface(id);
        }
    }

    pub(super) fn config_surface_focus(&mut self, id: u32) -> Result<(), String> {
        self.config_surface(id)?;
        self.focus_surface(id);
        Ok(())
    }

    pub(super) fn config_surface_size(&self, id: u32) -> Option<(u32, u32)> {
        self.config_surface(id).ok()?;
        self.pl.surfaces.get(&id).map(|s| (s.size.0 as u32, s.size.1 as u32))
    }

    /// The script was reloaded: its surfaces go.
    pub(super) fn drop_config_surfaces(&mut self) {
        let gone: Vec<u32> = self.pl.surfaces.iter().filter(|(_, s)| s.owner.id == CONFIG).map(|(id, _)| *id).collect();
        for id in gone {
            self.drop_surface(id);
        }
    }
}
