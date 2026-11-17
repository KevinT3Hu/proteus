//! Reused helper threads for plugin calls that block (files, processes,
//! HTTP, the keyring). A plugin waits for at most one such call at a time,
//! so the pool grows only to the number of calls in progress; threads left
//! idle for `IDLE` end.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError, mpsc};
use std::time::Duration;

pub(crate) type Job = Box<dyn FnOnce() + Send>;

const IDLE: Duration = Duration::from_secs(30);

struct Worker {
    id: u64,
    tx: mpsc::Sender<Job>,
}

static IDLE_WORKERS: Mutex<Vec<Worker>> = Mutex::new(Vec::new());
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

fn idle() -> MutexGuard<'static, Vec<Worker>> {
    IDLE_WORKERS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Run `job` on an idle helper thread, or a new one.
pub(crate) fn run(mut job: Job) -> std::io::Result<()> {
    while let Some(w) = idle().pop() {
        match w.tx.send(job) {
            Ok(()) => return Ok(()),
            Err(mpsc::SendError(back)) => job = back,
        }
    }
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new().name("plugin-call".into()).spawn(move || work(id, tx, rx, job))?;
    Ok(())
}

fn work(id: u64, tx: mpsc::Sender<Job>, rx: mpsc::Receiver<Job>, mut job: Job) {
    loop {
        job();
        idle().push(Worker { id, tx: tx.clone() });
        job = match rx.recv_timeout(IDLE) {
            Ok(next) => next,
            Err(_) => {
                let mut workers = idle();
                if let Some(i) = workers.iter().position(|w| w.id == id) {
                    workers.swap_remove(i);
                    return;
                }
                drop(workers);
                // Taken by `run` just now: its job is on the way.
                match rx.recv() {
                    Ok(next) => next,
                    Err(_) => return,
                }
            }
        };
    }
}
