//! Byte and message accounting held until a consumer drops its permit.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

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

impl Budget {
    pub fn new(bytes: usize, messages: usize) -> Self {
        Self(Arc::new(Inner { limits: (bytes, messages), used: Mutex::new((0, 0)), wake: Condvar::new() }))
    }

    pub fn try_acquire(&self, bytes: usize) -> Option<Permit> {
        let mut used = self.0.used.lock().unwrap();
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
        if bytes > self.0.limits.0 {
            return None;
        }
        loop {
            if stop.load(Ordering::Acquire) {
                return None;
            }
            if let Some(p) = self.try_acquire(bytes) {
                return Some(p);
            }
            let used = self.0.used.lock().unwrap();
            let _ = self.0.wake.wait_timeout(used, Duration::from_millis(10)).unwrap();
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
