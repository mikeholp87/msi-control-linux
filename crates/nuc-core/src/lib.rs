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
