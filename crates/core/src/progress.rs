use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

/// Cheap shared progress/cancel flag for long operations (scans, searches, deletions).
#[derive(Default)]
pub struct Progress {
    pub done: AtomicU64,
    pub total: AtomicU64,
    pub items: AtomicU64,
    pub errors: AtomicU64,
    cancelled: AtomicBool,
}

impl Progress {
    pub fn cancel(&self) {
        self.cancelled.store(true, Relaxed);
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Relaxed)
    }
    pub fn fraction(&self) -> f32 {
        let t = self.total.load(Relaxed);
        if t == 0 { 0.0 } else { (self.done.load(Relaxed) as f32 / t as f32).min(1.0) }
    }
}
