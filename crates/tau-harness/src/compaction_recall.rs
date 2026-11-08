//! Rebuildable recall objects, derived solely from committed semantic history.
//!
//! This is a searchable view, not provider-wire replay. No provider call,
//! private capture, referenced file, or referenced artifact is read to complete
//! it.

#[cfg(test)]
mod tests;
mod transcript;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use tau_core::{AgentEntry, AgentHistoryPrefix, AgentTree, NodeId};
use tau_proto::{AgentId, ArtifactDescriptor, ArtifactError, ArtifactOp, ArtifactValue};

use crate::artifact_store::ArtifactStore;

/// Independent producer budget; raising artifact transfer policy does not raise
/// the amount of history this best-effort producer replays.
const REPLAY_BYTES: u64 = 64 * 1024 * 1024;
/// Maximum ancestors rebuilt in one job; no recursive embedding or unbounded
/// cold-start work.
const MAX_ANCESTORS: usize = 32;

/// Fixed, content-free reasons presented without exposing private I/O errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecallFailure {
    /// Persistent transcript storage was not authorized for this owner.
    Disabled,
    /// Rebuilding the source tree would exceed the independent history budget.
    ReplayLimit,
    /// Storage, admission, deadline, cancellation or directory bounds failed.
    Unavailable,
}

/// Captured committed boundary and revocable persistent-agent authority.
pub(crate) struct RecallRequest {
    /// Exact complete-written prefix captured before worker admission.
    pub(crate) prefix: AgentHistoryPrefix,
    /// Exact agent whose history supplies the archive.
    pub(crate) agent_id: AgentId,
    /// Immutable standalone boundary, not the latest mutable head.
    pub(crate) boundary: NodeId,
    /// Runtime-only cancellation shared with the parked materialization.
    pub(crate) live: Arc<AtomicBool>,
    /// Configured directory limits, further narrowed by producer policy.
    pub(crate) limits: tau_artifact::Limits,
    /// Nonrenewable admission deadline, including frontier and queue waits.
    pub(crate) deadline: Instant,
}

/// Private worker completion, never a new semantic journal event.
pub(crate) struct RecallCompleted {
    /// Exact source owner.
    pub(crate) agent_id: AgentId,
    /// Exact immutable source boundary.
    pub(crate) boundary: NodeId,
    /// Identity shared only with the admitted runtime request.
    pub(crate) live: Arc<AtomicBool>,
    /// Complete archive or an honest best-effort failure.
    pub(crate) result: Result<ArtifactDescriptor, RecallFailure>,
    /// Physical work remains charged until the loop consumes this completion.
    pub(crate) _permit: crate::artifact_worker::Permit,
}

impl RecallRequest {
    /// Replays a bounded committed journal under deletion exclusion,
    /// reconstructs the exact replaced semantic windows, then stores
    /// normalized directories.
    pub(crate) fn run(
        &self,
        store: &mut ArtifactStore,
    ) -> Result<ArtifactDescriptor, RecallFailure> {
        let cancelled = || !self.live.load(Ordering::Acquire) || Instant::now() >= self.deadline;
        self.prefix
            .lease()
            .with_agent_io_guard(|agent_id| {
                if agent_id != &self.agent_id || cancelled() {
                    return Err(RecallFailure::Unavailable);
                }
                if self.prefix.encoded_bytes() > REPLAY_BYTES {
                    return Err(RecallFailure::ReplayLimit);
                }
                let records = self.prefix.read().map_err(|_| RecallFailure::Unavailable)?;
                let tree = AgentTree::try_from_events(agent_id.clone(), &records)
                    .map_err(|_| RecallFailure::Unavailable)?;
                drop(records);
                store_windows(
                    store,
                    &tree,
                    &self.agent_id,
                    self.boundary,
                    self.limits,
                    &cancelled,
                )
                .map_err(|_| RecallFailure::Unavailable)
            })
            .map_err(|_| RecallFailure::Unavailable)?
    }
}

/// Prepares the current window first, then adds as much direct ancestry as the
/// finite shared job budget permits. Older failures are explicit manifest gaps.
fn store_windows(
    store: &mut ArtifactStore,
    tree: &AgentTree,
    agent_id: &AgentId,
    boundary: NodeId,
    limits: tau_artifact::Limits,
    cancelled: &impl Fn() -> bool,
) -> Result<ArtifactDescriptor, ArtifactError> {
    let mut prepared = Vec::new();
    let mut next = Some(boundary);
    let mut remaining = limits.expanded_bytes.min(32 * 1024 * 1024);
    while let Some(boundary) = next {
        if cancelled() {
            return Err(ArtifactError::Unavailable);
        }
        if prepared.len() == MAX_ANCESTORS {
            break;
        }
        let directory = tempfile::tempdir().map_err(|_| ArtifactError::Io)?;
        // Prepare the newest complete window first. An oversized/missing
        // ancestor must not consume its budget or disable useful recall.
        // Reserve small fixed space for adding one parent descriptor later.
        let render = transcript::render(
            tree,
            agent_id,
            boundary,
            None,
            directory.path(),
            remaining.saturating_sub(512),
            cancelled,
        );
        let used = match render {
            Ok(used) => used + 512,
            Err(error) if prepared.is_empty() => return Err(error),
            Err(_) => break,
        };
        remaining -= used;
        prepared.push((directory, used));
        next = replaced_window(tree, boundary)?.1.replacement_boundary;
    }
    let mut parent = None;
    let count = prepared.len();
    for (index, (directory, used)) in prepared.into_iter().rev().enumerate() {
        if cancelled() {
            return Err(ArtifactError::Unavailable);
        }
        let result = (|| {
            transcript::link_parent(directory.path(), parent.as_ref())?;
            let bytes = tau_artifact::pack(
                directory.path(),
                tau_artifact::Limits {
                    stored_bytes: limits.stored_bytes.min(32 * 1024 * 1024),
                    expanded_bytes: used,
                },
                cancelled,
            )
            .map_err(|_| ArtifactError::Unavailable)?;
            put(store, &bytes, cancelled)
        })();
        if index + 1 == count {
            return result;
        }
        // A failed older archive is an explicit ancestry gap, not a
        // failed current archive or an invitation to redo provider work.
        parent = result.ok();
    }
    Err(ArtifactError::Unavailable)
}

/// Selects the historical provider window at `suffix_end`, then applies the
/// same semantic cut as compact-request materialization. The accepted
/// replacement and preserved suffix are never mistaken for replaced input.
fn replaced_window(
    tree: &AgentTree,
    boundary: NodeId,
) -> Result<(&AgentEntry, tau_core::ActiveProviderWindow<'_>), ArtifactError> {
    let entry = &tree.node(boundary).ok_or(ArtifactError::Unavailable)?.entry;
    let AgentEntry::Compaction {
        cut, suffix_end, ..
    } = entry
    else {
        return Err(ArtifactError::Unavailable);
    };
    let mut window = tree.active_provider_window(suffix_end.as_option());
    match cut {
        tau_proto::AgentHead::Root => {
            window.replacement = None;
            window.replacement_boundary = None;
            window.transcript.clear();
        }
        tau_proto::AgentHead::Node(node) if window.replacement_boundary == Some(*node) => {
            window.transcript.clear();
        }
        tau_proto::AgentHead::Node(node) => {
            let index = window
                .transcript
                .iter()
                .position(|(id, _)| id == node)
                .ok_or(ArtifactError::Unavailable)?;
            window.transcript.truncate(index + 1);
        }
    }
    Ok((entry, window))
}

/// Uses ordinary CAS upload semantics, including retention renewal. No root or
/// permanent pin is created; partial transfers are always disconnected.
fn put(
    store: &mut ArtifactStore,
    bytes: &[u8],
    cancelled: &impl Fn() -> bool,
) -> Result<ArtifactDescriptor, ArtifactError> {
    let connection = "internal-compaction-recall";
    let result = (|| {
        let now = crate::artifact_worker::unix_now();
        let ArtifactValue::Upload { upload } = store.execute(
            connection,
            connection,
            ArtifactOp::Begin {
                identity: tau_proto::ArtifactIdentity::metadata(
                    None,
                    Some(tau_artifact::DIRECTORY_MIME.to_owned()),
                )?,
                size: tau_proto::ArtifactSize::new(bytes.len() as u64)
                    .map_err(|_| ArtifactError::Invalid)?,
            },
            now,
        )?
        else {
            return Err(ArtifactError::Io);
        };
        for (index, chunk) in bytes.chunks(tau_proto::ARTIFACT_CHUNK_BYTES).enumerate() {
            if cancelled() {
                return Err(ArtifactError::Unavailable);
            }
            store.execute(
                connection,
                connection,
                ArtifactOp::Write {
                    upload: upload.clone(),
                    offset: (index * tau_proto::ARTIFACT_CHUNK_BYTES) as u64,
                    bytes: chunk.to_vec(),
                },
                now,
            )?;
        }
        if cancelled() {
            return Err(ArtifactError::Unavailable);
        }
        match store.execute(connection, connection, ArtifactOp::Finalize { upload }, now)? {
            ArtifactValue::Descriptor(descriptor) => Ok(descriptor),
            _ => Err(ArtifactError::Io),
        }
    })();
    store.disconnect(connection);
    result
}
