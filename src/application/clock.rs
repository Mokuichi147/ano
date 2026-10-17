//! The current time, as the conversation state and signatures record it.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch; 0 when the clock is set before it.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}
