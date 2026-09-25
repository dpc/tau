//! Lossless serialization of the timestamps used for discovery collisions.

use std::time::{Duration, SystemTime};

use serde::de::Error;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Serialize absent or signed timestamps without losing subsecond precision.
pub(super) fn serialize<S: Serializer>(
    time: &Option<SystemTime>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    time.map(|time| {
        let (before, duration) = match time.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(duration) => (false, duration),
            Err(error) => (true, error.duration()),
        };
        (before, duration.as_secs(), duration.subsec_nanos())
    })
    .serialize(serializer)
}

/// Restore the exact sampled timestamp, rejecting invalid or unrepresentable
/// values.
pub(super) fn deserialize<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<SystemTime>, D::Error> {
    Option::<(bool, u64, u32)>::deserialize(deserializer)?
        .map(|(before, seconds, nanos)| {
            if 1_000_000_000 <= nanos {
                return Err(Error::custom("invalid timestamp nanoseconds"));
            }
            let duration = Duration::new(seconds, nanos);
            if before {
                SystemTime::UNIX_EPOCH.checked_sub(duration)
            } else {
                SystemTime::UNIX_EPOCH.checked_add(duration)
            }
            .ok_or_else(|| Error::custom("sampled timestamp out of range"))
        })
        .transpose()
}
