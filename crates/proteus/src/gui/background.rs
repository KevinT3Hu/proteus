//! Work off the GUI thread whose result comes back to it.

use super::{App, Msg};

impl App {
    /// Run `job` on a pooled thread, then `then` with its result on the GUI
    /// thread.
    pub(super) fn in_background<T: Send + 'static>(
        &self,
        job: impl FnOnce() -> T + Send + 'static,
        then: impl FnOnce(&mut App, T) + Send + 'static,
    ) -> Result<(), String> {
        let tx = self.sender.clone();
        proteus_tasks::spawn_blocking(move || {
            let result = job();
            let _ = tx.send(Msg::Run(Box::new(move |app| then(app, result))));
        })
        .map(drop)
    }

    /// Run `task` (which only waits) on the task runtime, then `then` with
    /// its result on the GUI thread.
    pub(super) fn in_task<T: Send + 'static>(
        &self,
        task: impl Future<Output = T> + Send + 'static,
        then: impl FnOnce(&mut App, T) + Send + 'static,
    ) -> Result<(), String> {
        let tx = self.sender.clone();
        proteus_tasks::spawn(async move {
            let result = task.await;
            let _ = tx.send(Msg::Run(Box::new(move |app| then(app, result))));
        })
        .map(drop)
    }
}
