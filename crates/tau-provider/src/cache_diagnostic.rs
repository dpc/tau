//! Non-authoritative, private cache capture admission and process correlation.
//!
//! Reserve a complete record before constructing optional evidence.
//! Reservations include the worker's in-flight record, use no blocking lock,
//! and never drain.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use rand::RngCore as _;

/// Inclusive uncompressed byte ceiling for one scalar record.
pub const MAX_RECORD_BYTES: usize = 256 * 1024;
/// Maximum simultaneously reserved records, including compression/transport.
const MAX_RECORDS: usize = 64;

/// Startup-selected metadata policy, independent of exact capture policy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheDiagnostics {
    /// Do not produce scalar diagnostic records.
    Off,
    /// Capture bounded metadata only where durable capture is permitted.
    #[default]
    Metadata,
}

impl CacheDiagnostics {
    /// Whether this is the default metadata selection.
    pub fn is_metadata(&self) -> bool {
        *self == Self::Metadata
    }
}

/// Private random correlation, with no generic Debug projection.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct DiagnosticId([u8; 16]);

impl std::fmt::Debug for DiagnosticId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DiagnosticId(<private>)")
    }
}

impl DiagnosticId {
    /// Generate 128 random bits; entropy failure disables this diagnostic.
    pub fn random() -> Option<Self> {
        let mut bytes = [0; 16];
        rand::rngs::OsRng.try_fill_bytes(&mut bytes).ok()?;
        Some(Self(bytes))
    }
}

impl serde::Serialize for DiagnosticId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut text = [0; 32];
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for (index, byte) in self.0.iter().copied().enumerate() {
            text[2 * index] = HEX[usize::from(byte >> 4)];
            text[2 * index + 1] = HEX[usize::from(byte & 15)];
        }
        serializer.serialize_str(std::str::from_utf8(&text).expect("hex is ASCII"))
    }
}

/// Process-wide admission and loss counters, never persistence authority.
struct Budget {
    /// Full-size reservations charge at most 64 * 256 KiB = 16 MiB.
    reserved: AtomicUsize,
    /// Next record sequence, allocated even when subsequent admission fails.
    sequence: AtomicU64,
    /// Known losses; downstream harness losses remain unknowable here.
    dropped: AtomicU64,
}

impl Budget {
    /// Construct an empty process budget.
    const fn new() -> Self {
        Self {
            reserved: AtomicUsize::new(0),
            sequence: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        }
    }

    /// Allocate before admission; exhaustion permanently prevents reuse.
    #[allow(
        deprecated,
        reason = "try_update requires Rust 1.95, newer than the workspace MSRV"
    )]
    fn reserve(&'static self) -> Option<Reservation> {
        let sequence = self
            .sequence
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .ok()?;
        if self
            .reserved
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < MAX_RECORDS).then_some(n + 1)
            })
            .is_err()
        {
            self.lose();
            return None;
        }
        Some(Reservation {
            budget: self,
            sequence,
            delivered: false,
        })
    }

    /// Account a known loss without wrapping.
    #[allow(
        deprecated,
        reason = "try_update requires Rust 1.95, newer than the workspace MSRV"
    )]
    fn lose(&self) {
        let _ = self
            .dropped
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some(n.saturating_add(1))
            });
    }
}

/// One process's bounded metadata budget.
static BUDGET: Budget = Budget::new();
/// Process-lifetime diagnostic identity; failed entropy stays unavailable.
static RUN_ID: OnceLock<Option<DiagnosticId>> = OnceLock::new();
/// Build identity supplied once by the executable's existing metadata owner.
static BUILD: OnceLock<String> = OnceLock::new();

/// A full-record reservation retained until transport completion or loss.
pub struct Reservation {
    /// Owner of the count and byte reservation.
    budget: &'static Budget,
    /// Allocated sequence, including holes from dropped records.
    sequence: u64,
    /// Whether the worker successfully handed this record to transport.
    delivered: bool,
}

impl Reservation {
    /// Reserve without waiting, before constructing optional attribution.
    pub fn acquire() -> Option<Self> {
        BUDGET.reserve()
    }

    /// Return the process-local record identity allocated before admission.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Return known losses observed before serializing this admitted row.
    pub fn dropped_records_total(&self) -> u64 {
        self.budget.dropped.load(Ordering::Relaxed)
    }

    /// Mark successful provider-side delivery; not a durability assertion.
    pub(crate) fn delivered(&mut self) {
        self.delivered = true;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.delivered {
            self.budget.lose();
        }
        self.budget.reserved.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Return the cryptographically random process-lifetime diagnostic identity.
pub fn producer_run_id() -> Option<DiagnosticId> {
    *RUN_ID.get_or_init(DiagnosticId::random)
}

/// Freeze the executable's source/build identity without introducing wire
/// fields. Invalid or repeated initialization cannot replace the selected
/// identity.
pub fn initialize_build_identity(identity: String) {
    if !identity.is_empty() && identity.len() <= 128 {
        let _ = BUILD.set(identity);
    }
}

/// Executable-owned source identity; uninitialized library embeddings stay
/// explicitly unknown rather than claiming the caller's current checkout.
pub fn producer_build() -> &'static str {
    BUILD.get_or_init(|| "unknown".to_owned()).as_str()
}

#[cfg(test)]
mod tests;
