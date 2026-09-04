//! Per-conversation deduplication of large, byte-identical tool
//! results.
//!
//! Models occasionally re-issue identical reads, repeat the same
//! probing shell command (`jj status`, `cargo check` after a no-op
//! edit), or emit the same parallel tool call twice in one batch.
//! Each repetition pins a copy of the tool output into the prompt
//! prefix forever, both bloating the steady-state context and
//! defeating the prompt cache for every subsequent turn that has to
//! re-anchor on the larger prefix.
//!
//! This module replaces the *content* of any tool result whose CBOR
//! encoding hashes to the same value as a result already on the
//! conversation's branch with a short raw pointer
//! (`<tool_name> tool output identical to previous tool call: <call_id>`).
//! The first occurrence is kept verbatim — only the duplicates are collapsed.
//! The durable pointer carries `HarnessDedupPointer` presentation and gains its
//! `<tau_internal>` envelope only when the harness projects it to a Provider.
//! The model can cross-reference the pointer to the original `call_id` which is
//! still present earlier in its own context.
//!
//! Three invariants protect correctness:
//!
//! 1. **Branch isolation.** The map is per-conversation and rebuilt from the
//!    conversation's branch when the cursor moves non-linearly (e.g.
//!    `UiNavigateTree` to a sibling tip). A pointer can never reference a
//!    `call_id` the model can't see in its own assembled history.
//!
//! 2. **First-write-only.** Replacement happens at result-intake time, before
//!    the result is folded into the agent tree. Once recorded the entry is
//!    frozen for the rest of the agent branch, preserving the harness's
//!    linear-prefix invariant for the upstream prompt cache.
//!
//! 3. **Threshold gated.** Results whose serialized form is below
//!    [`DEFAULT_THRESHOLD_BYTES`] are not deduped at all — the pointer adds
//!    indirection, so smaller repeats aren't worth the extra hop the model has
//!    to make to recover the original.

use std::collections::HashMap;
use std::io::{self, Write};

use tau_core::AgentEntry;
use tau_proto::{CborValue, NodeId, ToolCallId, ToolResultStatus};

/// Minimum CBOR-serialized size of a tool result to consider
/// deduping. Below this, the pointer text is comparable to the
/// original content and the model cost of the redirect outweighs the
/// savings. 1 KiB keeps dedup limited to outputs large enough that the context
/// savings justify the extra model hop needed to recover the original.
pub(crate) const DEFAULT_THRESHOLD_BYTES: usize = 1024;

/// 16-byte truncated BLAKE3 digest of the CBOR-serialized result
/// content. BLAKE3 picked over SHA-256 for raw speed — this hash
/// runs synchronously on the harness's main loop on every tool
/// result. Truncation gives a ~10⁻¹⁹ collision probability per
/// pair. Hash equality only selects candidate anchors; borrowed canonical
/// content equality confirms every replacement, so a collision cannot alias
/// unrelated outputs.
pub(crate) type ResultHash = [u8; 16];

/// Hash and encoded byte length computed without materializing the encoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ResultFingerprint {
    /// Truncated content digest used to select possible matches.
    pub(crate) hash: ResultHash,
    /// Exact encoded length used for the dedup threshold and collision
    /// filtering.
    pub(crate) encoded_len: usize,
}

/// Location and identity of one retained canonical result.
#[derive(Clone, Debug)]
struct DedupAnchor {
    /// Call whose full result remains in the transcript.
    call_id: ToolCallId,
    /// Exact aggregate node, or pending until its parallel round materializes.
    node_id: Option<NodeId>,
}

/// Per-conversation dedup state. Tracks the hash of every full-fat
/// tool result (and tool error message) seen on the current branch,
/// keyed back to the first `call_id` that produced that content.
///
/// `built_for` records the [`NodeId`] the map was last synchronized
/// with. When the conversation's cursor moves non-linearly (a
/// navigation), [`Self::needs_rebuild`] returns true and the harness
/// rebuilds from the new branch before the next dedup decision.
#[derive(Debug, Default, Clone)]
pub(crate) struct ResultDedupMap {
    map: HashMap<ResultHash, Vec<DedupAnchor>>,
    built_for: Option<NodeId>,
    pending: Vec<(ResultHash, ToolCallId)>,
}

impl ResultDedupMap {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Returns `true` when the cached map's notion of the conversation
    /// head differs from `current`, i.e. the conversation jumped to a
    /// branch that wasn't a linear extension of where the map was
    /// built. The harness clears and rebuilds in that case.
    pub(crate) fn needs_rebuild(&self, current: Option<NodeId>) -> bool {
        self.built_for != current
    }

    /// Replace contents from a freshly walked branch. Called after
    /// [`Self::needs_rebuild`] reports a mismatch, or eagerly on
    /// session resume. `branch` must walk from tip to root;
    /// reverse candidate lookup then preserves the oldest canonical anchor.
    pub(crate) fn rebuild_from_branch<'a>(
        &mut self,
        branch: impl IntoIterator<Item = (NodeId, &'a AgentEntry)>,
        new_head: Option<NodeId>,
        threshold: usize,
    ) {
        self.map.clear();
        self.pending.clear();
        for (node_id, entry) in branch {
            let AgentEntry::ToolResults { items } = entry else {
                continue;
            };
            for item in items.iter().rev() {
                let fingerprint = match &item.status {
                    ToolResultStatus::Success => fingerprint_value(&item.output.raw),
                    ToolResultStatus::Error { message } => {
                        fingerprint_error(message, non_null_details(&item.output.raw))
                    }
                    ToolResultStatus::Cancelled { reason } => {
                        fingerprint_error(reason, non_null_details(&item.output.raw))
                    }
                };
                if fingerprint.encoded_len < threshold {
                    continue;
                }
                self.map
                    .entry(fingerprint.hash)
                    .or_default()
                    .push(DedupAnchor {
                        call_id: item.call_id.clone(),
                        node_id: Some(node_id),
                    });
            }
        }
        self.built_for = new_head;
    }

    /// Finds the oldest hash candidate whose borrowed canonical content
    /// matches.
    pub(crate) fn find_matching(
        &self,
        hash: &ResultHash,
        mut matches: impl FnMut(Option<NodeId>, &ToolCallId) -> bool,
    ) -> Option<&ToolCallId> {
        self.map
            .get(hash)?
            .iter()
            .rev()
            .find_map(|anchor| matches(anchor.node_id, &anchor.call_id).then_some(&anchor.call_id))
    }

    /// Returns the oldest anchor for unit tests that inspect map construction.
    #[cfg(test)]
    pub(crate) fn lookup(&self, hash: &ResultHash) -> Option<&ToolCallId> {
        self.map
            .get(hash)?
            .iter()
            .rev()
            .map(|anchor| &anchor.call_id)
            .next()
    }

    /// Records a fresh anchor pending the result's transcript fold.
    pub(crate) fn insert(&mut self, hash: ResultHash, call_id: ToolCallId) {
        self.map.entry(hash).or_default().push(DedupAnchor {
            call_id: call_id.clone(),
            node_id: None,
        });
        self.pending.push((hash, call_id));
    }

    /// Promotes newly materialized parallel-round anchors to exact node ids.
    pub(crate) fn resolve_pending(
        &mut self,
        mut resolution: impl FnMut(&ToolCallId) -> Option<Option<NodeId>>,
    ) {
        self.pending.retain(|(hash, call_id)| {
            let Some(node_id) = resolution(call_id) else {
                return true;
            };
            let Some(anchors) = self.map.get_mut(hash) else {
                return false;
            };
            let Some(index) = anchors
                .iter()
                .position(|anchor| anchor.node_id.is_none() && anchor.call_id == *call_id)
            else {
                return false;
            };
            if let Some(node_id) = node_id {
                anchors[index].node_id = Some(node_id);
            } else {
                anchors.remove(index);
            }
            false
        });
    }

    /// Advance the map's "built for" cursor without touching the
    /// table. Called after an event commits and the conversation head
    /// moves linearly to the just-folded node — the map is already
    /// in sync with that branch tip, so no rebuild is needed.
    ///
    /// **Skips when `built_for` is `None`.** That state means the map
    /// has never been populated for this conversation (fresh harness
    /// after session resume; map cleared after a navigation). A
    /// commit at this stage might be a non-dedup-eligible event (a
    /// user message from session re-init, a message projection) whose
    /// fold doesn't pass through `dedup_tool_result`. Advancing
    /// unconditionally would mark the map as "in sync with this new
    /// head" while still empty, making the next dedup intake skip
    /// the rebuild and miss every historical entry on the branch.
    /// The lazy rebuild on the next dedup intake is what populates
    /// the map; this method is only an optimization for the
    /// already-built case.
    pub(crate) fn note_head_advanced_to(&mut self, new_head: NodeId) {
        if self.built_for.is_some() {
            self.built_for = Some(new_head);
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.map.values().map(Vec::len).sum()
    }

    #[cfg(test)]
    pub(crate) fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

/// Restores the live `Option` representation from persisted null details.
pub(crate) fn non_null_details(value: &CborValue) -> Option<&CborValue> {
    (!matches!(value, CborValue::Null)).then_some(value)
}

/// Writer that streams canonical bytes into BLAKE3 while counting them.
struct FingerprintWriter {
    /// Incremental digest state.
    hasher: blake3::Hasher,
    /// Number of canonical bytes observed.
    encoded_len: usize,
    /// Serializer write calls, retained for deterministic work accounting.
    writes: usize,
}

impl FingerprintWriter {
    /// Finishes the streaming digest and truncates it to the map key width.
    fn finish(self) -> ResultFingerprint {
        let digest = self.hasher.finalize();
        let mut hash = [0_u8; 16];
        hash.copy_from_slice(&digest.as_bytes()[..16]);
        ResultFingerprint {
            hash,
            encoded_len: self.encoded_len,
        }
    }
}

impl Default for FingerprintWriter {
    fn default() -> Self {
        Self {
            hasher: blake3::Hasher::new(),
            encoded_len: 0,
            writes: 0,
        }
    }
}

impl Write for FingerprintWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.hasher.update(bytes);
        self.encoded_len = self.encoded_len.saturating_add(bytes.len());
        self.writes = self.writes.saturating_add(1);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Streams the stable CBOR encoding of `value` into a fingerprint.
pub(crate) fn fingerprint_value(value: &CborValue) -> ResultFingerprint {
    let mut writer = FingerprintWriter::default();
    ciborium::into_writer(value, &mut writer)
        .expect("CborValue from tau_proto should always serialize back to CBOR");
    writer.finish()
}

/// Returns streaming work counters for allocation-independent regression tests.
#[cfg(test)]
pub(crate) fn fingerprint_value_work(value: &CborValue) -> (ResultFingerprint, usize) {
    let mut writer = FingerprintWriter::default();
    ciborium::into_writer(value, &mut writer)
        .expect("CborValue from tau_proto should always serialize back to CBOR");
    let writes = writer.writes;
    (writer.finish(), writes)
}

/// Streams the disjoint error prefix, message, and optional raw details.
pub(crate) fn fingerprint_error(message: &str, details: Option<&CborValue>) -> ResultFingerprint {
    let mut writer = FingerprintWriter::default();
    writer
        .write_all(b"err\x00")
        .expect("hash writer is infallible");
    writer
        .write_all(message.as_bytes())
        .expect("hash writer is infallible");
    writer.write_all(&[0]).expect("hash writer is infallible");
    if let Some(details) = details {
        ciborium::into_writer(details, &mut writer)
            .expect("CborValue from tau_proto should always serialize back to CBOR");
    }
    writer.finish()
}

/// Materializes canonical bytes only for compatibility-oracle tests.
#[cfg(test)]
pub(crate) fn encode_for_hash(value: &CborValue) -> Vec<u8> {
    let mut bytes = Vec::new();
    ciborium::into_writer(value, &mut bytes).expect("CborValue should serialize");
    bytes
}

/// Materializes canonical error bytes only for compatibility-oracle tests.
#[cfg(test)]
pub(crate) fn encode_error_for_hash(message: &str, details: Option<&CborValue>) -> Vec<u8> {
    let mut bytes = b"err\x00".to_vec();
    bytes.extend_from_slice(message.as_bytes());
    bytes.push(0);
    if let Some(details) = details {
        ciborium::into_writer(details, &mut bytes).expect("CborValue should serialize");
    }
    bytes
}

/// Hashes already materialized oracle bytes for compatibility tests.
#[cfg(test)]
pub(crate) fn hash_truncated(bytes: &[u8]) -> ResultHash {
    let digest = blake3::hash(bytes);
    digest.as_bytes()[..16]
        .try_into()
        .expect("fixed digest prefix")
}

/// Materializes one stored response only for compatibility-oracle tests.
#[cfg(test)]
pub(crate) fn encode_tool_response_for_hash(response: &tau_proto::ToolResponse) -> Vec<u8> {
    encode_for_hash(&response.raw)
}

/// Materializes one stored error only for compatibility-oracle tests.
#[cfg(test)]
pub(crate) fn encode_error_response_for_hash(
    message: &str,
    response: &tau_proto::ToolResponse,
) -> Vec<u8> {
    encode_error_for_hash(message, non_null_details(&response.raw))
}

/// Compares CBOR values by the bytes their stable serializer observes.
pub(crate) fn canonical_value_eq(left: &CborValue, right: &CborValue) -> bool {
    match (left, right) {
        (CborValue::Float(left), CborValue::Float(right)) => left.to_bits() == right.to_bits(),
        (CborValue::Array(left), CborValue::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| canonical_value_eq(left, right))
        }
        (CborValue::Map(left), CborValue::Map(right)) => {
            left.len() == right.len()
                && left.iter().zip(right).all(|((lk, lv), (rk, rv))| {
                    canonical_value_eq(lk, rk) && canonical_value_eq(lv, rv)
                })
        }
        (CborValue::Tag(lt, lv), CborValue::Tag(rt, rv)) => lt == rt && canonical_value_eq(lv, rv),
        _ => left == right,
    }
}

/// Build the CBOR value that replaces a duplicate tool result.
/// Encodes as `CborValue::Text`; the harness stamps the accompanying typed
/// presentation discriminator before provider projection frames this body.
///
/// Format includes both the tool name and original call id so the model can
/// find the previous output without mistaking the pointer for a pending or
/// cached result.
pub(crate) fn build_pointer_value(
    original_call_id: &ToolCallId,
    tool_name: &tau_proto::ToolName,
) -> CborValue {
    CborValue::Text(format!(
        "{} tool output identical to previous tool call: {}",
        tool_name.as_str(),
        original_call_id
    ))
}

/// Build the error-message string that replaces a duplicate tool
/// error. The raw pointer body goes into the `message` field; `details` is
/// dropped because it is what made the original distinct and the pointer's job
/// is to refer back, not to reproduce it. The wrapping `function_call_output`
/// is rendered with an "ERROR:" prefix downstream. The pointer still names the
/// original tool and call id so the model can locate the full error earlier in
/// context.
pub(crate) fn build_pointer_error_message(
    original_call_id: &ToolCallId,
    tool_name: &tau_proto::ToolName,
) -> String {
    format!(
        "{} tool output identical to previous tool call: {}",
        tool_name.as_str(),
        original_call_id
    )
}

#[cfg(test)]
mod tests;
