//! Harness-owned transcript retention, independent of ordinary textual links.
//!
//! All methods run off-loop. A successful retain must precede admission of any
//! journal record that relies on it. Failure or cancellation may leak
//! protection; compaction, branch changes, and failed journal appends must not
//! release it.

use std::collections::{BTreeMap, HashSet};

use serde::de::Error as _;
use serde::{Deserialize, Serialize};
use tau_proto::{AgentId, ArtifactSize};

use super::*;

#[cfg(test)]
mod tests;

/// Bounds one manifest's allocation independently of total transcript size.
const MANIFEST_MAX_BYTES: u64 = 16 * 1024 * 1024;
/// An incomplete scan preserves all candidates, rather than guessing liveness.
const SCAN_BUDGET: Duration = Duration::from_secs(2);
/// Detects a missing registry instead of mistaking it for a legacy empty store.
const REGISTRY_MARKER: &str = ".transcript-roots-v1";

/// Conservative union of every artifact retained by one agent's durable
/// history.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TranscriptRoots {
    /// Closed bookkeeping format; unknown formats prevent cleanup.
    version: u32,
    /// Stable transcript owner, checked against the containing directory.
    agent_id: AgentId,
    /// Unique physical objects; logical occurrence quotas belong to admission.
    #[serde(deserialize_with = "unique_artifacts")]
    artifacts: BTreeMap<ArtifactKey, ArtifactSize>,
}

impl ArtifactStore {
    /// Verifies bytes and durably protects them before the caller admits a
    /// record.
    ///
    /// The caller must hold the durable agent writer lease throughout this
    /// operation and subsequent record admission. This prevents history
    /// deletion racing root creation. Returned bytes still require actual
    /// media validation. No public Artifact RPC exposes this operation, and
    /// it does not renew age.
    #[cfg(test)]
    pub(crate) fn retain_transcript_artifact(
        &self,
        agent_id: &AgentId,
        descriptor: &ArtifactDescriptor,
    ) -> Result<Vec<u8>, ArtifactError> {
        self.retain_transcript_artifact_with(agent_id, descriptor, sync_dir)
    }

    /// Validates actual bytes before publishing protection under the same CAS
    /// lock.
    ///
    /// The caller must already own scoped agent deletion exclusion. Validation
    /// must complete here, not return deferred I/O or an unverified descriptor.
    pub(crate) fn retain_checked_transcript_artifact(
        &self,
        agent_id: &AgentId,
        descriptor: &ArtifactDescriptor,
        validate: impl FnOnce(&[u8]) -> Result<(), ArtifactError>,
    ) -> Result<(), ArtifactError> {
        self.retain_checked_transcript_artifact_with(agent_id, descriptor, validate, sync_dir)
            .map(|_| ())
    }

    /// Keeps the final publication sync injectable for crash-cut tests.
    #[cfg(test)]
    fn retain_transcript_artifact_with(
        &self,
        agent_id: &AgentId,
        descriptor: &ArtifactDescriptor,
        sync_registry: impl FnOnce(&Path) -> Result<(), ArtifactError>,
    ) -> Result<Vec<u8>, ArtifactError> {
        self.retain_checked_transcript_artifact_with(
            agent_id,
            descriptor,
            |_| Ok(()),
            sync_registry,
        )
    }

    /// Keeps physical byte validation and publication inside one cleanup
    /// exclusion.
    fn retain_checked_transcript_artifact_with(
        &self,
        agent_id: &AgentId,
        descriptor: &ArtifactDescriptor,
        validate: impl FnOnce(&[u8]) -> Result<(), ArtifactError>,
        sync_registry: impl FnOnce(&Path) -> Result<(), ArtifactError>,
    ) -> Result<Vec<u8>, ArtifactError> {
        let _lock = self.lock(true)?;
        let agents = self.root.parent().ok_or(ArtifactError::Io)?.join("agents");
        match fs::symlink_metadata(tau_core::retired_agent_tombstone(&agents, agent_id)) {
            Ok(_) => return Err(ArtifactError::Unavailable),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(error)),
        }
        require_directory(&agents.join(agent_id.as_str()))?;
        if self.descriptor(&descriptor.key)? != *descriptor {
            return Err(ArtifactError::Integrity);
        }
        let digest = self.storage_digest(&descriptor.key)?;
        let directory = self.object_directory(&digest)?;
        require_directory(&directory)?;
        let bytes = read_regular(&directory.join("data"), descriptor.size.get())?;
        verify(descriptor, &bytes)?;
        validate(&bytes)?;
        self.protect_transcript_object(descriptor)?;
        let registry = self.prepare_transcript_registry()?;
        let directory = registry.join(agent_id.as_str());
        let mut roots = match fs::symlink_metadata(&directory) {
            Ok(_) => read_roots(&directory, agent_id)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => TranscriptRoots {
                version: 1,
                agent_id: agent_id.clone(),
                artifacts: BTreeMap::new(),
            },
            Err(error) => return Err(io_error(error)),
        };
        if roots
            .artifacts
            .insert(descriptor.key.clone(), descriptor.size)
            .is_some_and(|previous| previous != descriptor.size)
        {
            return Err(ArtifactError::Integrity);
        }
        check_manifest_size(&roots, MANIFEST_MAX_BYTES)?;
        private_dir(&directory)?;
        // Even a duplicate retries all syncs: a previous rename may have
        // succeeded before its directory sync failed.
        replace_json(&directory.join("roots.json"), &roots)?;
        sync_registry(&registry)?;
        sync_dir(&self.root)?;
        Ok(bytes)
    }

    /// Removes protection only after the caller durably removed the whole
    /// history.
    ///
    /// Only agent-retention cleanup calls this, after its staged-tree removal
    /// and directory sync succeed. Busy or failed release deliberately
    /// leaks roots.
    pub(crate) fn release_deleted_transcript(
        &self,
        agent_id: &AgentId,
    ) -> Result<(), ArtifactError> {
        // Do not create an artifact domain merely because an ordinary agent
        // died.
        if !self.root.try_exists().map_err(io_error)? {
            return Ok(());
        }
        let _lock = self.lock(true)?;
        let Some(registry) = self.transcript_registry()? else {
            return Ok(());
        };
        let directory = registry.join(agent_id.as_str());
        match fs::symlink_metadata(&directory) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(io_error(error)),
        }
        read_roots(&directory, agent_id)?;
        self.detach(&directory)
    }

    /// Finds protected candidates under the caller's exclusive cleanup lock.
    ///
    /// Memory retains at most the bounded candidate set plus one bounded
    /// manifest; elapsed-time exhaustion never yields a partial authoritative
    /// set.
    pub(super) fn retained_transcript_artifacts(
        &self,
        candidates: &HashSet<ArtifactKey>,
        report_corrupt: &mut impl FnMut(&Path),
    ) -> Result<HashSet<ArtifactKey>, ArtifactError> {
        self.scan_transcript_roots(candidates, report_corrupt, SCAN_BUDGET)
    }

    /// The explicit budget permits deterministic exhausted-scan regression
    /// tests.
    fn scan_transcript_roots(
        &self,
        candidates: &HashSet<ArtifactKey>,
        report_corrupt: &mut impl FnMut(&Path),
        budget: Duration,
    ) -> Result<HashSet<ArtifactKey>, ArtifactError> {
        let registry = match self.transcript_registry() {
            Ok(Some(registry)) => registry,
            Ok(None) => return Ok(HashSet::new()),
            Err(error) => {
                report_corrupt(&self.root.join("transcript-roots"));
                return Err(error);
            }
        };
        let started = Instant::now();
        let mut retained = HashSet::new();
        for entry in fs::read_dir(&registry).map_err(io_error)? {
            if started.elapsed() >= budget {
                return Err(ArtifactError::Busy);
            }
            let entry = entry.map_err(io_error)?;
            let result = AgentId::parse(entry.file_name().to_string_lossy())
                .map_err(|_| ArtifactError::Integrity)
                .and_then(|owner| read_roots(&entry.path(), &owner));
            let roots = match result {
                Ok(roots) => roots,
                Err(error) => {
                    report_corrupt(&entry.path());
                    return Err(error);
                }
            };
            // Public identities protect their full private storage locators.
            retained.extend(
                candidates
                    .iter()
                    .filter(|candidate| roots.artifacts.contains_key(*candidate))
                    .cloned(),
            );
        }
        if started.elapsed() >= budget {
            return Err(ArtifactError::Busy);
        }
        Ok(retained)
    }

    /// Opens an existing registry without healing missing retention authority.
    fn transcript_registry(&self) -> Result<Option<PathBuf>, ArtifactError> {
        let registry = self.root.join("transcript-roots");
        let marker = self.root.join(REGISTRY_MARKER);
        if !path_present(&registry)? && !path_present(&marker)? {
            return Ok(None);
        }
        require_directory(&registry)?;
        let contents = read_regular(&marker, 16)?;
        if contents != b"1\n" {
            return Err(ArtifactError::Integrity);
        }
        Ok(Some(registry))
    }

    /// Publishes the registry marker before any transcript can acquire roots.
    fn prepare_transcript_registry(&self) -> Result<PathBuf, ArtifactError> {
        if let Some(registry) = self.transcript_registry()? {
            return Ok(registry);
        }
        let registry = self.root.join("transcript-roots");
        private_dir(&registry)?;
        write_synced(&self.root.join(REGISTRY_MARKER), b"1\n")?;
        sync_dir(&registry)?;
        sync_dir(&self.root)?;
        Ok(registry)
    }
}

/// Rejects an oversized union without evicting an older retained key.
fn check_manifest_size(roots: &TranscriptRoots, maximum: u64) -> Result<(), ArtifactError> {
    let encoded = serde_json::to_vec(roots).map_err(|_| ArtifactError::Integrity)?;
    if encoded.len() as u64 > maximum {
        return Err(ArtifactError::Invalid);
    }
    Ok(())
}

/// Refuses malformed directory metadata instead of treating it as no roots.
pub(super) fn require_directory(path: &Path) -> Result<(), ArtifactError> {
    let metadata = fs::symlink_metadata(path).map_err(io_error)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(ArtifactError::Integrity);
    }
    Ok(())
}

/// Reads one bounded complete manifest; a missing file is never an empty set.
fn read_roots(directory: &Path, agent_id: &AgentId) -> Result<TranscriptRoots, ArtifactError> {
    require_directory(directory)?;
    let bytes = read_regular(&directory.join("roots.json"), MANIFEST_MAX_BYTES)?;
    let roots: TranscriptRoots =
        serde_json::from_slice(&bytes).map_err(|_| ArtifactError::Integrity)?;
    if roots.version != 1 || roots.agent_id != *agent_id {
        return Err(ArtifactError::Integrity);
    }
    Ok(roots)
}

/// Missing entries are distinct from malformed entries, including dangling
/// links.
pub(super) fn path_present(path: &Path) -> Result<bool, ArtifactError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error(error)),
    }
}

/// Bounds allocation and rejects special files in retention authority.
pub(super) fn read_regular(path: &Path, maximum: u64) -> Result<Vec<u8>, ArtifactError> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(io_error)?;
    if !file.metadata().map_err(io_error)?.is_file() {
        return Err(ArtifactError::Integrity);
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(maximum + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 > maximum {
        return Err(ArtifactError::Integrity);
    }
    Ok(bytes)
}

/// Duplicate JSON keys are corrupt authority, not a last-writer-wins root set.
fn unique_artifacts<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<ArtifactKey, ArtifactSize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    /// Validates uniqueness while keeping one bounded manifest in memory.
    struct UniqueArtifacts;
    impl<'de> serde::de::Visitor<'de> for UniqueArtifacts {
        type Value = BTreeMap<ArtifactKey, ArtifactSize>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("unique artifact keys")
        }

        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> Result<Self::Value, A::Error> {
            let mut entries = BTreeMap::new();
            while let Some((key, size)) = map.next_entry()? {
                if entries.insert(key, size).is_some() {
                    return Err(A::Error::custom("duplicate artifact key"));
                }
            }
            Ok(entries)
        }
    }
    deserializer.deserialize_map(UniqueArtifacts)
}
