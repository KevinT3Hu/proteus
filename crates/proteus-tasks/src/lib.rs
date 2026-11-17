//! The process's one Tokio runtime, for the terminal and its plugins.
//!
//! Async tasks on its workers only wait (sockets, pipes, children, timers)
//! and never block or compute for long. Work that blocks and then ends
//! (files, `/proc`, git, the keyring) goes through [`spawn_blocking`], which
//! runs it on a reused OS thread. Long-lived or CPU-heavy work (pty I/O,
//! plugin instances, image decoding) keeps its own threads.
//!
//! Child processes started through it are reaped by Tokio, which installs a
//! `SIGCHLD` handler (`SA_RESTART`): blocking `poll` calls elsewhere may then
//! return `EINTR` and must retry.

use std::sync::OnceLock;
use std::time::Duration;

use tokio::runtime::{Builder, EnterGuard, Handle, Runtime};
use tokio::task::JoinHandle;

const WORKERS: usize = 2;
/// Beyond this, blocking jobs wait for a free thread.
const MAX_BLOCKING: usize = 64;
const KEEP_ALIVE: Duration = Duration::from_secs(30);

/// The runtime, started on first use.
pub fn handle() -> Result<&'static Handle, String> {
    static RUNTIME: OnceLock<Result<Runtime, String>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            Builder::new_multi_thread()
                .worker_threads(WORKERS)
                .max_blocking_threads(MAX_BLOCKING)
                .thread_keep_alive(KEEP_ALIVE)
                .thread_name("proteus-tasks")
                .enable_all()
                .build()
                .map_err(|e| format!("cannot start the task runtime: {e}"))
        })
        .as_ref()
        .map(Runtime::handle)
        .map_err(Clone::clone)
}

/// Run `f` on a pooled thread, in the caller's tracing span.
pub fn spawn_blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<JoinHandle<T>, String> {
    let span = tracing::Span::current();
    Ok(handle()?.spawn_blocking(move || span.in_scope(f)))
}

/// Run `task` on a worker, in the caller's tracing span. It must only wait.
pub fn spawn<F>(task: F) -> Result<JoinHandle<F::Output>, String>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    use tracing::Instrument;
    Ok(handle()?.spawn(task.instrument(tracing::Span::current())))
}

/// Make the runtime current on this thread while the guard lives, for
/// what registers with it on creation (child processes, async fds).
pub fn enter() -> Result<EnterGuard<'static>, String> {
    Ok(handle()?.enter())
}
