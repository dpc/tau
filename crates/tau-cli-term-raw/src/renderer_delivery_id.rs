//! Process-local identities for renderer deliveries.

/// Identifies one socket delivery as it crosses the CLI renderer pipeline.
///
/// The identity is process-local and never serialized. Production code creates
/// it only in the renderer allocator; its raw value is available solely for
/// diagnostics and other primitive boundaries.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RendererDeliveryId(u64);

impl RendererDeliveryId {
    /// Creates one renderer delivery identity from the renderer allocator's
    /// next raw value.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the raw value for a primitive ABI or diagnostic boundary.
    pub const fn get(self) -> u64 {
        self.0
    }
}
