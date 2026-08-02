//! Durable and live state for one notification registration.

use std::time::{Duration, Instant};

use rostra_client::SocialPostMaterializationCursor;

use crate::notification_pending::Pending;
use crate::notification_state::duration_ms;

/// Durable contents for one enabled agent in the notification state file.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub(crate) struct StoredRegistration {
    /// Immutable first-enable feed tip.
    pub(crate) baseline: SocialPostMaterializationCursor,
    /// Cursor acknowledging every disposed feed row before it.
    pub(crate) committed: SocialPostMaterializationCursor,
    /// Wall-clock time of the most recently canonical report.
    pub(crate) last_canonical_report_unix_ms: Option<u64>,
    /// Durable first queued time so restart preserves max batch age.
    pub(crate) queued_since_unix_ms: Option<u64>,
}

/// Per-agent durable policy plus transient report coordination.
#[derive(Clone, Debug)]
pub(crate) struct Registration {
    /// Immutable first-enable feed tip.
    pub(crate) baseline: SocialPostMaterializationCursor,
    /// Last durably disposed feed cursor.
    pub(crate) committed: SocialPostMaterializationCursor,
    /// Most recent canonical report time.
    pub(crate) last_canonical_report_unix_ms: Option<u64>,
    /// Current source page, if it contains selected posts.
    pub(crate) pending: Option<Pending>,
    /// Report cursor awaiting canonical message echo.
    pub(crate) inflight_end: Option<SocialPostMaterializationCursor>,
    /// Recovered first queued wall-clock time.
    pub(crate) queued_since_unix_ms: Option<u64>,
}

impl Registration {
    /// Rebuilds live state from the durable file representation.
    pub(crate) fn from_stored(stored: &StoredRegistration) -> Self {
        Self {
            baseline: stored.baseline,
            committed: stored.committed,
            last_canonical_report_unix_ms: stored.last_canonical_report_unix_ms,
            pending: None,
            inflight_end: None,
            queued_since_unix_ms: stored.queued_since_unix_ms,
        }
    }

    /// Extracts exactly the durable portion of the registration.
    pub(crate) fn stored(&self) -> StoredRegistration {
        StoredRegistration {
            baseline: self.baseline,
            committed: self.committed,
            last_canonical_report_unix_ms: self.last_canonical_report_unix_ms,
            queued_since_unix_ms: self.queued_since_unix_ms,
        }
    }

    /// Returns the earliest permitted report time under all timing limits.
    pub(crate) fn due_at(
        &self,
        now: Instant,
        now_ms: u64,
        idle_debounce: Duration,
        max_batch_age: Duration,
        report_interval: Duration,
    ) -> Option<Instant> {
        let pending = self.pending.as_ref()?;
        let idle = pending.last_queued_at.checked_add(idle_debounce)?;
        let age = pending.first_queued_at.checked_add(max_batch_age)?;
        let spacing = self.last_canonical_report_unix_ms.map_or(now, |last| {
            let remaining = last
                .saturating_add(duration_ms(report_interval))
                .saturating_sub(now_ms);
            now.checked_add(Duration::from_millis(remaining))
                .unwrap_or(now)
        });
        Some(idle.min(age).max(spacing))
    }
}
