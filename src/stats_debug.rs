//! Optional diagnostics, printed every 5 seconds when CYBERHUD_DEBUG is set.
use std::sync::atomic::{AtomicU64, Ordering};

pub static BG_REBUILDS: AtomicU64 = AtomicU64::new(0);
pub static BAND_UPDATES: AtomicU64 = AtomicU64::new(0);
pub static BAND_CHANGES: AtomicU64 = AtomicU64::new(0);
pub static MAX_GAP_US: AtomicU64 = AtomicU64::new(0);

pub fn enabled() -> bool {
    std::env::var_os("CYBERHUD_DEBUG").is_some()
}

pub fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

pub fn take(counter: &AtomicU64) -> u64 {
    counter.swap(0, Ordering::Relaxed)
}
