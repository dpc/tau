//! Hard aggregate persistence capacity.

/// Hard limits covering staged, queued, in-flight, registry, and derived work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PersistenceCapacity {
    /// Maximum authoritative frames admitted but not terminally disposed.
    pub max_frames: usize,
    /// Maximum aggregate retained bytes, including staging reservations.
    pub max_bytes: usize,
    /// Maximum simultaneously registered stream generations.
    pub max_streams: usize,
}

impl Default for PersistenceCapacity {
    fn default() -> Self {
        Self {
            max_frames: 1_024,
            max_bytes: 256 * 1024 * 1024,
            max_streams: 4_096,
        }
    }
}
