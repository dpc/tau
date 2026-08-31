//! Durable state for per-agent Rostra following notifications.
//!
//! This module owns the versioned identity-bound checkpoint file, policy
//! mutations, receipt acknowledgement, and retry timing.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write as _};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rostra_client::{RostraId, SocialPostMaterializationCursor};
use tau_proto::{AgentId, CborValue, ExtensionName, MessageDelivered, MessageFactId, UnixMillis};

use crate::notification_page::ScannedPage;
pub(crate) use crate::notification_pending::Pending;
pub(crate) use crate::notification_registration::{Registration, StoredRegistration};

/// Extension-data schema carried by each canonical Rostra batch.
pub(crate) const SCHEMA: &str = "rostra-new-posts-v2";
/// Schema for the extension-owned durable notification file.
const STATE_SCHEMA: &str = "rostra-notifications-v1";
/// Upper bound for the entire policy/checkpoint file.
const MAX_STATE_FILE_BYTES: usize = 1024 * 1024;
/// One complete decoded row per source page bounds eager database retention
/// before Tau filters it; the pinned API has no streaming or byte-budget scan.
pub(crate) const MATERIALIZATION_PAGE: NonZeroUsize = NonZeroUsize::MIN;
/// Idle time after the final eligible materialization before a report becomes
/// due.
pub(crate) const IDLE_DEBOUNCE: Duration = Duration::from_secs(30);
/// Maximum age of one queued batch before requesting a report.
pub(crate) const MAX_BATCH_AGE: Duration = Duration::from_secs(5 * 60);
/// Minimum spacing between canonical Rostra reports for one agent.
pub(crate) const REPORT_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// One identity-wide, durably allocated Rostra notification report attempt.
///
/// The transparent representation preserves the checkpoint's established CBOR
/// scalar while this type owns checked allocation and canonical fact-ID
/// spelling.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[repr(transparent)]
pub(crate) struct ReportAttempt(u64);

impl ReportAttempt {
    /// Returns the next attempt unless the durable sequence is exhausted.
    fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }

    /// Builds the canonical publisher-scoped fact ID for this attempt.
    pub(crate) fn fact_id(self) -> MessageFactId {
        MessageFactId::new(format!("rostra-batch-v1:{}", self.0))
    }
}

/// Versioned identity-bound contents of the extension-owned state file.
#[derive(serde::Deserialize, serde::Serialize)]
struct StoredState {
    /// File-format discriminator.
    schema: String,
    /// Stable publisher namespace that scopes every allocated report ID.
    publisher: ExtensionName,
    /// Rostra identity that owns every opaque cursor in this file.
    rostra_identity: RostraId,
    /// Next identity-wide publisher message-attempt number.
    next_report_attempt: ReportAttempt,
    /// Enabled agents keyed by canonical agent identifier text.
    agents: BTreeMap<String, StoredRegistration>,
}

/// Process-live identity and checkpoint location for one notification store.
///
/// This is deliberately separate from [`StoredState`]: the checkpoint retains
/// its established flat CBOR fields and this private bundle is never serialized
/// as its own structure.
struct NotificationStoreConfig {
    /// Stable publisher namespace that scopes report facts.
    publisher: ExtensionName,
    /// Rostra identity that owns the checkpoint's opaque cursors.
    identity: RostraId,
    /// Location of this identity-bound notification checkpoint.
    path: PathBuf,
}

/// Extension-owned durable notification policy and live session gates.
#[derive(Default)]
pub(crate) struct State {
    /// Enabled policy and checkpoints by agent.
    registrations: BTreeMap<AgentId, Registration>,
    /// Agents with a successful replay boundary.
    replay_complete: BTreeSet<AgentId>,
    /// Agents currently loaded in a session.
    loaded_agents: BTreeSet<AgentId>,
    /// Agents with another bounded source page ready to scan.
    continuations: BTreeSet<AgentId>,
    /// Complete configured notification-store identity and checkpoint location.
    configured: Option<NotificationStoreConfig>,
    /// Next identity-wide publisher message-attempt number.
    next_report_attempt: ReportAttempt,
    /// Stops persisted mutations after an ambiguous post-rename failure.
    poisoned: bool,
    /// Earliest worker retry after a transient scan or pre-rename persistence
    /// error.
    retry_at: Option<Instant>,
    /// Exponential retry delay for transient worker failures.
    retry_delay: Duration,
    /// Last worker diagnostic, used to avoid repeating the same failure
    /// noisily.
    last_diagnostic: Option<Instant>,
    /// Test-only persistence failure injected at one ordered durability phase.
    #[cfg(test)]
    fault: Option<PersistFault>,
}

/// Classifies whether a persistence failure occurred before or after rename.
enum PersistFailure {
    /// The old durable state remains authoritative.
    BeforeRename(&'static str),
    /// Rename made the candidate visible but directory durability failed.
    AfterRename(&'static str),
}

impl PersistFailure {
    /// Converts a simulated directory phase error into its ordered failure
    /// class.
    fn after_rename(self) -> Self {
        match self {
            Self::BeforeRename(message) | Self::AfterRename(message) => Self::AfterRename(message),
        }
    }
}

/// Ordered persistence phase used by deterministic fault-injection tests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PersistFault {
    /// Fail while writing the replacement file.
    Write,
    /// Fail before syncing the replacement file.
    FileSync,
    /// Fail before atomic rename.
    Rename,
    /// Fail after rename while syncing the parent directory.
    DirectorySync,
}

impl State {
    /// Discards all state on extension deconfiguration.
    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    /// Loads this identity's versioned durable state before workers start.
    pub(crate) fn configure(
        &mut self,
        publisher: ExtensionName,
        identity: RostraId,
        state_dir: &Path,
    ) -> Result<(), &'static str> {
        self.clear();
        self.configured = Some(NotificationStoreConfig {
            publisher,
            identity,
            path: state_dir.join("rostra-notifications-v1.cbor"),
        });
        let configured = self
            .configured
            .as_ref()
            .expect("notification configuration was just installed");
        if !configured.path.exists() {
            return Ok(());
        }
        let mut file =
            File::open(&configured.path).map_err(|_| "notification state cannot be opened")?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take((MAX_STATE_FILE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| "notification state cannot be read")?;
        if MAX_STATE_FILE_BYTES < bytes.len() {
            return Err("notification state exceeds its bound");
        }
        let stored: StoredState = ciborium::de::from_reader(bytes.as_slice())
            .map_err(|_| "notification state is corrupt")?;
        if stored.schema != STATE_SCHEMA
            || stored.publisher != configured.publisher
            || stored.rostra_identity != configured.identity
        {
            return Err("notification state schema, publisher, or identity does not match");
        }
        for (agent, registration) in stored.agents {
            let agent =
                AgentId::parse(agent).map_err(|_| "notification state contains invalid agent")?;
            self.registrations
                .insert(agent, Registration::from_stored(&registration));
        }
        self.next_report_attempt = stored.next_report_attempt;
        Ok(())
    }

    /// Injects one deterministic pre- or post-rename failure in tests.
    fn fault(&self, phase: PersistFault) -> Result<(), PersistFailure> {
        #[cfg(test)]
        if self.fault == Some(phase) {
            return Err(PersistFailure::BeforeRename(
                "injected notification persistence failure",
            ));
        }
        let _ = phase;
        Ok(())
    }

    /// Writes one complete candidate state before installing it in memory.
    fn persist(
        &self,
        registrations: &BTreeMap<AgentId, Registration>,
        next_report_attempt: ReportAttempt,
    ) -> Result<(), PersistFailure> {
        if self.poisoned {
            return Err(PersistFailure::BeforeRename(
                "notification state durability is uncertain",
            ));
        }
        let configured = self
            .configured
            .as_ref()
            .ok_or(PersistFailure::BeforeRename(
                "notification state is unavailable",
            ))?;
        let stored = StoredState {
            schema: STATE_SCHEMA.to_owned(),
            publisher: configured.publisher.clone(),
            rostra_identity: configured.identity,
            next_report_attempt,
            agents: registrations
                .iter()
                .map(|(agent, registration)| (agent.to_string(), registration.stored()))
                .collect(),
        };
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&stored, &mut bytes)
            .map_err(|_| PersistFailure::BeforeRename("notification state cannot be encoded"))?;
        if MAX_STATE_FILE_BYTES < bytes.len() {
            return Err(PersistFailure::BeforeRename(
                "notification state exceeds its bound",
            ));
        }
        let temp = configured.path.with_extension("cbor.tmp");
        if temp.exists() {
            // A same-directory temporary file was never renamed, so no durable
            // candidate became authoritative. The extension mutex owns this name.
            fs::remove_file(&temp).map_err(|_| {
                PersistFailure::BeforeRename("notification state temporary file cannot be removed")
            })?;
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|_| {
                PersistFailure::BeforeRename("notification state temporary file cannot be created")
            })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|_| {
                    PersistFailure::BeforeRename("notification state permissions cannot be set")
                })?;
        }
        self.fault(PersistFault::Write)?;
        file.write_all(&bytes)
            .map_err(|_| PersistFailure::BeforeRename("notification state cannot be written"))?;
        self.fault(PersistFault::FileSync)?;
        file.sync_all()
            .map_err(|_| PersistFailure::BeforeRename("notification state cannot be synced"))?;
        drop(file);
        self.fault(PersistFault::Rename)?;
        fs::rename(&temp, &configured.path)
            .map_err(|_| PersistFailure::BeforeRename("notification state cannot be replaced"))?;
        self.fault(PersistFault::DirectorySync)
            .map_err(|failure| failure.after_rename())?;
        File::open(configured.path.parent().ok_or(PersistFailure::AfterRename(
            "notification state parent unavailable",
        ))?)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| {
            PersistFailure::AfterRename("notification state directory cannot be synced")
        })?;
        Ok(())
    }

    /// Commits a candidate state, installing it only after the file is durable.
    fn commit(
        &mut self,
        registrations: BTreeMap<AgentId, Registration>,
    ) -> Result<(), &'static str> {
        match self.persist(&registrations, self.next_report_attempt) {
            Ok(()) => {
                self.registrations = registrations;
                Ok(())
            }
            Err(PersistFailure::BeforeRename(message)) => Err(message),
            Err(PersistFailure::AfterRename(message)) => {
                // Rename made the candidate visible. Keep memory aligned with that
                // on-disk state, then poison all later persisted mutations because
                // directory durability is ambiguous.
                self.registrations = registrations;
                self.poisoned = true;
                Err(message)
            }
        }
    }

    /// Allocates and durably advances one identity-wide report attempt before
    /// its publisher-scoped message ID becomes visible.
    pub(crate) fn allocate_report_attempt(&mut self) -> Result<ReportAttempt, &'static str> {
        if self.poisoned {
            return Err("notification state durability is uncertain");
        }
        let allocated = self.next_report_attempt;
        let next = allocated
            .next()
            .ok_or("notification report attempt counter is exhausted")?;
        match self.persist(&self.registrations, next) {
            Ok(()) => {
                self.next_report_attempt = next;
                Ok(allocated)
            }
            Err(PersistFailure::BeforeRename(message)) => Err(message),
            Err(PersistFailure::AfterRename(message)) => {
                self.next_report_attempt = next;
                self.poisoned = true;
                Err(message)
            }
        }
    }

    /// Returns the next report attempt in deterministic state tests.
    #[cfg(test)]
    pub(crate) fn next_report_attempt(&self) -> u64 {
        self.next_report_attempt.0
    }

    /// Records a transient worker failure using bounded exponential backoff.
    pub(crate) fn record_retry(&mut self) {
        let delay = if self.retry_delay.is_zero() {
            Duration::from_secs(1)
        } else {
            self.retry_delay
                .saturating_mul(2)
                .min(Duration::from_secs(60))
        };
        self.retry_delay = delay;
        self.retry_at = Instant::now().checked_add(delay);
    }

    /// Clears retry backoff after a complete error-free worker pass.
    pub(crate) fn clear_retry(&mut self) {
        self.retry_at = None;
        self.retry_delay = Duration::ZERO;
    }

    /// Returns whether a worker failure may emit its rate-limited diagnostic.
    pub(crate) fn should_log_failure(&mut self) -> bool {
        let now = Instant::now();
        if self
            .last_diagnostic
            .is_some_and(|previous| now.duration_since(previous) < Duration::from_secs(60))
        {
            return false;
        }
        self.last_diagnostic = Some(now);
        true
    }

    /// Returns whether the worker may issue another source operation now.
    pub(crate) fn retry_ready(&self) -> bool {
        !self.poisoned
            && self
                .retry_at
                .is_none_or(|retry_at| retry_at <= Instant::now())
    }

    /// Persists one immutable first-enable baseline before reporting tool
    /// success.
    pub(crate) fn enable(
        &mut self,
        agent: AgentId,
        baseline: SocialPostMaterializationCursor,
    ) -> Result<(), &'static str> {
        if self.poisoned {
            return Err("notification state durability is uncertain");
        }
        if self.registrations.contains_key(&agent) {
            return Ok(());
        }
        let mut next = self.registrations.clone();
        next.insert(
            agent,
            Registration {
                baseline,
                committed: baseline,
                last_canonical_report_unix_ms: None,
                pending: None,
                inflight_end: None,
                queued_since_unix_ms: None,
            },
        );
        self.commit(next)
    }

    /// Removes one registration durably before reporting tool success.
    pub(crate) fn disable(&mut self, agent: &AgentId) -> Result<(), &'static str> {
        if self.poisoned {
            return Err("notification state durability is uncertain");
        }
        if !self.registrations.contains_key(agent) {
            return Ok(());
        }
        let mut next = self.registrations.clone();
        next.remove(agent);
        self.commit(next)?;
        self.continuations.remove(agent);
        Ok(())
    }

    /// Marks one loaded agent available for reports.
    pub(crate) fn loaded(&mut self, agent: AgentId) {
        self.loaded_agents.insert(agent);
    }

    /// Removes live session gates without changing durable policy.
    pub(crate) fn unloaded(&mut self, agent: &AgentId) {
        self.loaded_agents.remove(agent);
        self.replay_complete.remove(agent);
        self.continuations.remove(agent);
    }

    /// Opens delivery only after successful historical replay.
    pub(crate) fn replay_complete(&mut self, agent: AgentId) {
        self.replay_complete.insert(agent);
    }

    /// Returns agents currently safe for source reconciliation.
    pub(crate) fn eligible_agents(&self) -> Vec<AgentId> {
        if self.poisoned {
            return Vec::new();
        }
        self.registrations
            .keys()
            .filter(|agent| {
                self.loaded_agents.contains(*agent) && self.replay_complete.contains(*agent)
            })
            .cloned()
            .collect()
    }

    /// Returns one immutable registration snapshot for an asynchronous feed
    /// read.
    pub(crate) fn scan_snapshot(&self, agent: &AgentId) -> Option<Registration> {
        self.registrations.get(agent).cloned()
    }

    /// Merges one completed source page after verifying its original snapshot.
    pub(crate) fn merge_page(
        &mut self,
        agent: &AgentId,
        snapshot: &Registration,
        page: ScannedPage,
    ) -> Result<(), &'static str> {
        let Some(current) = self.registrations.get(agent) else {
            return Ok(());
        };
        if current.committed != snapshot.committed
            || current.pending.as_ref().map(|pending| pending.end)
                != snapshot.pending.as_ref().map(|pending| pending.end)
        {
            return Ok(());
        }
        let mut next = self.registrations.clone();
        let registration = next
            .get_mut(agent)
            .expect("registration remains in candidate state");
        if let Some(pending) = registration.pending.as_mut() {
            pending.end = page.scanned_through;
            if page.count != 0 {
                pending.count = NonZeroUsize::new(pending.count.get() + page.count)
                    .expect("pending selected-post count remains nonzero");
                pending.last_queued_at = Instant::now();
            }
        } else if page.count == 0 {
            if page.had_items {
                registration.committed = page.scanned_through;
            }
        } else {
            let queued_ms = registration.queued_since_unix_ms.unwrap_or_else(now_ms);
            let elapsed = now_ms().get().saturating_sub(queued_ms.get());
            let now = Instant::now();
            registration.queued_since_unix_ms = Some(queued_ms);
            registration.pending = Some(Pending {
                end: page.scanned_through,
                first_queued_at: now
                    .checked_sub(Duration::from_millis(elapsed))
                    .unwrap_or(now),
                last_queued_at: now,
                count: NonZeroUsize::new(page.count)
                    .expect("selected page constructs a nonzero pending count"),
            });
        }
        self.commit(next)?;
        if page.exhausted {
            self.continuations.remove(agent);
        } else {
            self.continuations.insert(agent.clone());
        }
        Ok(())
    }

    /// Returns a due report's typed identity and immutable pending page.
    pub(crate) fn due_report(&self, agent: &AgentId) -> Option<(ExtensionName, RostraId, Pending)> {
        let registration = self.registrations.get(agent)?;
        let now = Instant::now();
        if registration.inflight_end.is_some()
            || registration
                .due_at(now, now_ms(), IDLE_DEBOUNCE, MAX_BATCH_AGE, REPORT_INTERVAL)
                .is_none_or(|due| now < due)
        {
            return None;
        }
        let configured = self.configured.as_ref()?;
        Some((
            configured.publisher.clone(),
            configured.identity,
            registration.pending.clone()?,
        ))
    }

    /// Marks the exact successfully enqueued report as waiting for its live
    /// echo.
    pub(crate) fn mark_inflight(&mut self, agent: &AgentId, end: SocialPostMaterializationCursor) {
        if let Some(registration) = self.registrations.get_mut(agent)
            && registration
                .pending
                .as_ref()
                .is_some_and(|pending| pending.end == end)
        {
            registration.inflight_end = Some(end);
        }
    }

    /// Installs one immediately due pending page for deterministic worker
    /// tests.
    #[cfg(test)]
    pub(crate) fn set_pending_due(
        &mut self,
        agent: &AgentId,
        end: SocialPostMaterializationCursor,
        count: NonZeroUsize,
    ) {
        let now = Instant::now();
        if let Some(registration) = self.registrations.get_mut(agent) {
            registration.pending = Some(Pending {
                end,
                first_queued_at: now.checked_sub(MAX_BATCH_AGE).unwrap_or(now),
                last_queued_at: now.checked_sub(MAX_BATCH_AGE).unwrap_or(now),
                count,
            });
        }
    }

    /// Returns the next timing deadline, if a selected page is pending.
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        let now = Instant::now();
        if let Some(retry_at) = self.retry_at
            && now < retry_at
        {
            return Some(retry_at);
        }
        if self.continuations.iter().any(|agent| {
            self.loaded_agents.contains(agent)
                && self.replay_complete.contains(agent)
                && self
                    .registrations
                    .get(agent)
                    .is_some_and(|registration| registration.inflight_end.is_none())
        }) {
            return Some(now);
        }
        let now_ms = now_ms();
        self.retry_at
            .into_iter()
            .chain(self.eligible_agents().into_iter().filter_map(|agent| {
                let registration = self.registrations.get(&agent)?;
                registration
                    .inflight_end
                    .is_none()
                    .then(|| {
                        registration.due_at(
                            now,
                            now_ms,
                            IDLE_DEBOUNCE,
                            MAX_BATCH_AGE,
                            REPORT_INTERVAL,
                        )
                    })
                    .flatten()
            }))
            .min()
    }

    /// Commits the exact in-flight canonical message echo before another page.
    pub(crate) fn acknowledge(
        &mut self,
        delivered: &MessageDelivered,
    ) -> Result<bool, &'static str> {
        if self.poisoned {
            return Err("notification state durability is uncertain");
        }
        let Some(end) = report_end(delivered) else {
            return Ok(false);
        };
        if self
            .configured
            .as_ref()
            .map(|configured| configured.publisher.as_str())
            != Some(delivered.publisher_extension_id.as_str())
        {
            return Ok(false);
        }
        let agent =
            AgentId::parse(delivered.agent_id.as_str()).map_err(|_| "invalid report target")?;
        let Some(registration) = self.registrations.get(&agent) else {
            return Ok(false);
        };
        // Only the report this process emitted can acknowledge a page. A matching
        // schema/cursor from an earlier delivery or another batch must not skip feed
        // rows.
        if registration.inflight_end != Some(end) {
            return Ok(false);
        }
        let mut next = self.registrations.clone();
        let registration = next
            .get_mut(&agent)
            .expect("registration remains in candidate state");
        registration.committed = end;
        registration.inflight_end = None;
        registration.pending = None;
        registration.queued_since_unix_ms = None;
        registration.last_canonical_report_unix_ms = Some(now_ms());
        self.commit(next)?;
        // The preceding report page may have stopped before the source tip.
        // Reconciliation verifies that condition with a bounded next scan.
        self.continuations.insert(agent);
        Ok(true)
    }
}

/// Decodes a matching report cursor from private message metadata.
fn report_end(delivered: &MessageDelivered) -> Option<SocialPostMaterializationCursor> {
    let data = delivered.extension_data.value();
    if tau_proto::cbor_text_field(data, "schema").as_deref() != Some(SCHEMA) {
        return None;
    }
    decode_value(tau_proto::cbor_field(data, "scanned_through")?)
}

/// Deserializes an opaque value through Tau's existing CBOR bridge.
fn decode_value<T: serde::de::DeserializeOwned>(value: &CborValue) -> Option<T> {
    serde_json::from_value(serde_json::to_value(value).ok()?).ok()
}

/// Returns a saturating Unix-millisecond clock for durable timing.
pub(crate) fn now_ms() -> UnixMillis {
    unix_millis_since_epoch(SystemTime::now().duration_since(UNIX_EPOCH))
}

/// Converts an epoch-relative duration into a saturating Unix-millisecond
/// timestamp, retaining the pre-epoch fallback.
fn unix_millis_since_epoch(
    since_epoch: Result<Duration, std::time::SystemTimeError>,
) -> UnixMillis {
    UnixMillis::new(since_epoch.map_or(0, duration_ms))
}

/// Converts a duration to a saturating millisecond count.
pub(crate) fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "notifications_tests.rs"]
mod notifications_tests;
