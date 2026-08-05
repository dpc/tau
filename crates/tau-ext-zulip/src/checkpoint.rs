use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use fs2::FileExt as _;
use tau_proto::MessageFactId;

/// Exclusively owned, identity-scoped durable Zulip message checkpoint.
pub(crate) struct CheckpointStore {
    /// Final atomically replaced checkpoint path.
    path: PathBuf,
    /// Process-lifetime advisory lock preventing concurrent identity owners.
    _lock: File,
}

/// Versioned on-disk message position.
#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    /// Persistence schema version.
    version: u8,
    /// Highest durably completed native Zulip message ID.
    message_id: u64,
}

/// Ordered acknowledgement and retry state for one checkpoint namespace.
pub(crate) struct CheckpointRuntime {
    /// Identity-scoped atomic store and retained process lock.
    store: CheckpointStore,
    /// Last durably completed native message ID.
    position: Option<u64>,
    /// Observed native IDs in ascending advancement order.
    pending: BTreeMap<u64, Candidate>,
    /// Whether bounded history recovery has more work.
    catch_up_needed: bool,
}

/// One observed native position. Entries remain barriers until filtering or a
/// correlated post-commit echo completes them; retry entries are never skipped.
enum Candidate {
    /// Route/filter processing is currently evaluating the message.
    Processing,
    /// The submitted report awaits its correlated canonical fact.
    Awaiting(MessageFactId),
    /// Submission failed and the exact native message must be fetched again.
    Retry,
    /// Filtering or canonical self-observation completed this position.
    Complete,
}

impl CheckpointStore {
    /// Open the identity namespace and retain its exclusive process lock.
    pub(crate) fn open(state_dir: &Path, id_key: &[u8; 32]) -> io::Result<Self> {
        fs::create_dir_all(state_dir)?;
        let mut hasher = blake3::Hasher::new_keyed(id_key);
        hasher.update(b"tau-ext-zulip/checkpoint-namespace/v1\0");
        let namespace = hasher.finalize().to_hex();
        let path = state_dir.join(format!("message-position-{namespace}.json"));
        let lock_path = state_dir.join(format!("message-position-{namespace}.lock"));
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path)?;
        lock.try_lock_exclusive()?;
        Ok(Self { path, _lock: lock })
    }

    /// Read the checkpoint, rejecting corruption and unsupported versions.
    pub(crate) fn load(&self) -> io::Result<Option<u64>> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let checkpoint: Checkpoint = serde_json::from_slice(&bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if checkpoint.version != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported Zulip checkpoint version",
            ));
        }
        Ok(Some(checkpoint.message_id))
    }

    /// Atomically replace and durably synchronize the checkpoint.
    pub(crate) fn store(&self, message_id: u64) -> io::Result<()> {
        let parent = self.path.parent().expect("checkpoint has parent");
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        let bytes = serde_json::to_vec(&Checkpoint {
            version: 1,
            message_id,
        })
        .map_err(io::Error::other)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        temporary.persist(&self.path).map_err(|error| error.error)?;
        File::open(parent)?.sync_all()
    }
}

impl CheckpointRuntime {
    /// Open one runtime from its durable identity namespace.
    pub(crate) fn open(state_dir: &Path, id_key: &[u8; 32]) -> io::Result<Self> {
        let store = CheckpointStore::open(state_dir, id_key)?;
        let position = store.load()?;
        Ok(Self {
            store,
            position,
            pending: BTreeMap::new(),
            catch_up_needed: true,
        })
    }

    /// Return the durable high-water position, if initialized.
    pub(crate) fn position(&self) -> Option<u64> {
        self.position
    }

    /// Return whether history recovery should run before ordinary long polling.
    pub(crate) fn catch_up_needed(&self) -> bool {
        self.catch_up_needed
    }

    /// Set whether the bounded history traversal reached the current newest
    /// tip.
    pub(crate) fn set_more_history(&mut self, more: bool) {
        self.catch_up_needed = more || self.has_retry();
    }

    /// Begin processing a previously unseen position. Retry barriers may be
    /// attempted again; completed or awaiting positions remain deduplicated.
    pub(crate) fn begin(&mut self, message_id: u64) -> bool {
        if self.position.is_some_and(|position| message_id <= position) {
            return false;
        }
        match self.pending.get(&message_id) {
            None | Some(Candidate::Retry) => {
                self.pending.insert(message_id, Candidate::Processing);
                true
            }
            _ => false,
        }
    }

    /// Record successful report submission and await its canonical echo.
    pub(crate) fn submitted(&mut self, message_id: u64, fact_id: MessageFactId) {
        if matches!(self.pending.get(&message_id), Some(Candidate::Processing)) {
            self.pending
                .insert(message_id, Candidate::Awaiting(fact_id));
        }
    }

    /// Complete a position that current sender/route policy filtered out.
    pub(crate) fn filtered(&mut self, message_id: u64) {
        if matches!(self.pending.get(&message_id), Some(Candidate::Processing)) {
            self.pending.insert(message_id, Candidate::Complete);
        }
    }

    /// Retain a failed report as a non-advancing retry barrier.
    pub(crate) fn retry(&mut self, message_id: u64) {
        self.pending.insert(message_id, Candidate::Retry);
        self.catch_up_needed = true;
    }

    /// Correlate one canonical fact without relying on evictable reply
    /// authority.
    pub(crate) fn acknowledge(&mut self, fact_id: &MessageFactId) -> bool {
        let Some((&message_id, _)) = self.pending.iter().find(|(_, candidate)| {
            matches!(candidate, Candidate::Awaiting(expected) if expected == fact_id)
        }) else {
            return false;
        };
        self.pending.insert(message_id, Candidate::Complete);
        true
    }

    /// Add the first-use baseline after the startup live queue has been
    /// drained.
    pub(crate) fn baseline(&mut self, message_id: u64) {
        self.pending
            .entry(message_id)
            .or_insert(Candidate::Complete);
    }

    /// Return whether unresolved reports provide recovery backpressure.
    pub(crate) fn has_outstanding(&self) -> bool {
        self.pending
            .values()
            .any(|candidate| matches!(candidate, Candidate::Awaiting(_)))
    }

    /// Return the oldest failed position that must be fetched and submitted
    /// again.
    pub(crate) fn retry_position(&self) -> Option<u64> {
        self.pending.iter().find_map(|(message_id, candidate)| {
            matches!(candidate, Candidate::Retry).then_some(*message_id)
        })
    }

    fn has_retry(&self) -> bool {
        self.pending
            .values()
            .any(|candidate| matches!(candidate, Candidate::Retry | Candidate::Processing))
    }

    /// Persist and remove the highest contiguous completed observed prefix.
    pub(crate) fn advance(&mut self) -> io::Result<()> {
        let mut completed = self.position;
        for (&message_id, candidate) in &self.pending {
            if !matches!(candidate, Candidate::Complete) {
                break;
            }
            completed = Some(message_id);
        }
        let Some(completed) = completed.filter(|value| Some(*value) != self.position) else {
            return Ok(());
        };
        self.store.store(completed)?;
        self.position = Some(completed);
        self.pending.retain(|message_id, _| completed < *message_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
