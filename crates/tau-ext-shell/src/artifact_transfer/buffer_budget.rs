//! Admission covering queued preparation, transfer and final local writes.

use std::sync::{Arc, Mutex};

/// Shared original-byte capacity; large transfers run alone rather than
/// multiplying the configured object maximum by the command queue length.
#[derive(Clone, Default)]
pub(super) struct BufferBudget {
    /// Reserved maximum bytes, including operations that have not allocated
    /// yet.
    used: Arc<Mutex<u64>>,
}

/// Releases capacity on every terminal, cancellation and worker failure path.
pub(super) struct Reservation {
    /// Shared accounting owner.
    budget: BufferBudget,
    /// Maximum bytes reserved before local preparation or download.
    bytes: u64,
}

impl BufferBudget {
    /// Admits up to 128 MiB concurrently, or one larger bounded object alone.
    pub(super) fn reserve(&self, bytes: u64) -> Result<Reservation, String> {
        let mut used = self.used.lock().expect("artifact buffer budget poisoned");
        if bytes > tau_proto::ARTIFACT_MAX_BYTES
            || (used.saturating_add(bytes) > 128 * 1024 * 1024 && *used != 0)
        {
            return Err("artifact buffer capacity is busy; retry this tool call".to_owned());
        }
        *used += bytes;
        Ok(Reservation {
            budget: self.clone(),
            bytes,
        })
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut used = self
            .budget
            .used
            .lock()
            .expect("artifact buffer budget poisoned");
        *used -= self.bytes;
    }
}
