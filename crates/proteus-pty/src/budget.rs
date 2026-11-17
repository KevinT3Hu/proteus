//! Byte and message accounting held until a consumer drops its permit.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// How often a waiter that is not woken by a released permit checks `stop`.
const STOP_CHECK: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
pub struct Budget(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    limits: (usize, usize),
    used: Mutex<(usize, usize)>,
    wake: Condvar,
}

#[derive(Debug)]
pub struct Permit {
    budget: Budget,
    bytes: usize,
}

/// The deadline passed before the budget had room.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimedOut;

impl Budget {
    pub fn new(bytes: usize, messages: usize) -> Self {
        Self(Arc::new(Inner { limits: (bytes, messages), used: Mutex::new((0, 0)), wake: Condvar::new() }))
    }

    pub fn try_acquire(&self, bytes: usize) -> Option<Permit> {
        let mut used = self.0.used.lock().unwrap();
        self.take(&mut used, bytes)
    }

    fn take(&self, used: &mut (usize, usize), bytes: usize) -> Option<Permit> {
        if bytes > self.0.limits.0.saturating_sub(used.0) || used.1 >= self.0.limits.1 {
            return None;
        }
        used.0 += bytes;
        used.1 += 1;
        Some(Permit { budget: self.clone(), bytes })
    }

    /// Wait only on producer threads. Cancellation is checked even when no
    /// consumer remains to release a permit.
    pub fn acquire(&self, bytes: usize, stop: &AtomicBool) -> Option<Permit> {
        self.acquire_until(bytes, stop, None).ok()?
    }

    /// Like [`acquire`](Self::acquire), but gives up at `deadline`.
    /// `Ok(None)` when stopped or too large.
    pub fn acquire_until(
        &self,
        bytes: usize,
        stop: &AtomicBool,
        deadline: Option<Instant>,
    ) -> Result<Option<Permit>, TimedOut> {
        if bytes > self.0.limits.0 {
            return Ok(None);
        }
        let mut used = self.0.used.lock().unwrap();
        loop {
            if stop.load(Ordering::Acquire) {
                return Ok(None);
            }
            if let Some(p) = self.take(&mut used, bytes) {
                return Ok(Some(p));
            }
            let mut wait = STOP_CHECK;
            if let Some(d) = deadline {
                let left = d.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(TimedOut);
                }
                wait = wait.min(left);
            }
            used = self.0.wake.wait_timeout(used, wait).unwrap().0;
        }
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut used = self.budget.0.used.lock().unwrap();
        used.0 -= self.bytes;
        used.1 -= 1;
        self.budget.0.wake.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_until_consumed_and_cancellable() {
        let budget = Budget::new(10, 2);
        let a = budget.try_acquire(6).unwrap();
        assert!(budget.try_acquire(5).is_none());
        let b = budget.try_acquire(0).unwrap();
        assert!(budget.try_acquire(0).is_none());
        let waiting = budget.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let cancel = stop.clone();
        let worker = std::thread::spawn(move || waiting.acquire(1, &cancel));
        stop.store(true, Ordering::Release);
        assert!(worker.join().unwrap().is_none());
        drop((a, b));
        assert!(budget.try_acquire(10).is_some());
    }
}
