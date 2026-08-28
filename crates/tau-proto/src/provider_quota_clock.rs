//! Scalar clock domains used by provider quota telemetry.
//!
//! The transparent wrappers preserve quota-report wire scalars while preventing
//! milliseconds, seconds, and signed server offsets from being interchanged.

use serde::{Deserialize, Serialize};

macro_rules! quota_clock_scalar {
    ($name:ident, $scalar:ty, $description:literal, $value_description:literal) => {
        #[doc = $description]
        #[derive(
            Clone,
            Copy,
            Debug,
            Default,
            Deserialize,
            Eq,
            Hash,
            Ord,
            PartialEq,
            PartialOrd,
            Serialize,
        )]
        #[serde(transparent)]
        pub struct $name(#[doc = $value_description] $scalar);

        impl $name {
            /// Constructs this clock value from its scalar wire representation.
            #[must_use]
            pub const fn new(value: $scalar) -> Self {
                Self(value)
            }

            /// Returns this clock value's scalar representation for a named conversion.
            #[must_use]
            pub const fn get(self) -> $scalar {
                self.0
            }
        }
    };
}

quota_clock_scalar!(
    UnixMillis,
    u64,
    "A Unix timestamp measured in milliseconds.",
    "Milliseconds since the Unix epoch."
);
quota_clock_scalar!(
    UnixSeconds,
    u64,
    "A Unix timestamp measured in seconds.",
    "Seconds since the Unix epoch."
);
quota_clock_scalar!(
    QuotaWindowSeconds,
    u64,
    "A provider-declared quota-window duration measured in seconds.",
    "Quota-window duration in seconds."
);
quota_clock_scalar!(
    SignedSeconds,
    i64,
    "A signed provider-declared duration measured in seconds.",
    "Signed duration in seconds."
);
quota_clock_scalar!(
    ServerOffsetMillis,
    i64,
    "A signed offset between the provider server clock and Unix time in milliseconds.",
    "Signed provider-server offset in milliseconds."
);

#[cfg(test)]
#[path = "provider_quota_clock/tests.rs"]
mod tests;
