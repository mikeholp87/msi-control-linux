pub mod config;
pub mod error;
pub mod model;
pub mod units;

pub use error::{AppError, Result};
pub use model::*;
pub use units::*;

/// UTC timestamp helper used by all crates so stored data is always UTC (ms epoch).
pub fn utc_now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Lock a `Mutex` even if another thread panicked while holding it.
///
/// Every mutex in this workspace guards plain data that is replaced or
/// appended to whole, so a poisoned guard is still consistent. Recovering
/// keeps one worker panic from cascading into the GTK main thread.
pub trait MutexExt<T> {
    fn lock_or_recover(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> MutexExt<T> for std::sync::Mutex<T> {
    fn lock_or_recover(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Whole milliseconds in `d`, saturating instead of truncating.
pub fn millis_u64(d: std::time::Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Whole milliseconds in `d` as `i64`, saturating instead of truncating.
pub fn millis_i64(d: std::time::Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}
