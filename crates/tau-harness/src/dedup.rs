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
/// pair, which is fine — a collision would only mean two unrelated
/// outputs get aliased, and the model would see a pointer to the
/// "wrong" call. In practice the failure mode is so rare that
/// chasing it isn't worth the wider hash.
pub(crate) type ResultHash = [u8; 16];

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
    map: HashMap<ResultHash, ToolCallId>,
    built_for: Option<NodeId>,
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
    /// session resume / session switch.
    pub(crate) fn rebuild_from_branch<'a>(
        &mut self,
        branch: impl IntoIterator<Item = &'a AgentEntry>,
        new_head: Option<NodeId>,
        threshold: usize,
    ) {
        self.map.clear();
        for entry in branch {
            let AgentEntry::ToolResults { items } = entry else {
                continue;
            };
            for item in items {
                let (content_hash, content_bytes) = match &item.status {
                    ToolResultStatus::Success => {
                        let bytes = encode_tool_response_for_hash(&item.output);
                        (hash_truncated(&bytes), bytes.len())
                    }
                    ToolResultStatus::Error { message } => {
                        let bytes = encode_error_response_for_hash(message, &item.output);
                        (hash_truncated(&bytes), bytes.len())
                    }
                    ToolResultStatus::Cancelled { reason } => {
                        let bytes = encode_error_response_for_hash(reason, &item.output);
                        (hash_truncated(&bytes), bytes.len())
                    }
                };
                if content_bytes < threshold {
                    continue;
                }
                self.map
                    .entry(content_hash)
                    .or_insert_with(|| item.call_id.clone());
            }
        }
        self.built_for = new_head;
    }

    /// Look up `hash`. Returns the first `call_id` that produced this
    /// content on the current branch, or `None` if it's new.
    pub(crate) fn lookup(&self, hash: &ResultHash) -> Option<&ToolCallId> {
        self.map.get(hash)
    }

    /// Record a fresh `(hash, call_id)` pair. Caller must have
    /// confirmed `lookup(&hash).is_none()` first; an
    /// already-present hash is a programming error and triggers a
    /// debug-assertion-only panic so production sessions just keep
    /// the original mapping.
    pub(crate) fn insert(&mut self, hash: ResultHash, call_id: ToolCallId) {
        debug_assert!(
            !self.map.contains_key(&hash),
            "dedup map insert called on existing hash; lookup-before-insert was skipped"
        );
        self.map.insert(hash, call_id);
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
        self.map.len()
    }
}

/// CBOR-encode `value` for hashing. Encoding (rather than e.g. a
/// printable repr) keeps the hash stable across renderer changes
/// (`cbor_to_text` is for human / LLM consumption only and may evolve
/// formatting).
pub(crate) fn encode_for_hash(value: &CborValue) -> Vec<u8> {
    let mut buf = Vec::new();
    // ciborium's writer is infallible into a `Vec<u8>`; a serialize
    // failure here would mean a non-encodable CBOR value reached us,
    // which `tau_proto` rules out at the type level.
    ciborium::into_writer(value, &mut buf)
        .expect("CborValue from tau_proto should always serialize back to CBOR");
    buf
}

/// Companion to [`encode_for_hash`] for `ToolError` outcomes. Keys on
/// the message string and the optional details payload jointly so a
/// repeated error with the same message and same details collapses,
/// while two errors that share a message but carry different details
/// stay distinct.
pub(crate) fn encode_error_for_hash(message: &str, details: Option<&CborValue>) -> Vec<u8> {
    let mut buf = Vec::new();
    // Tag-prefix to keep the error keyspace disjoint from the result
    // keyspace — without this, an error message text would collide
    // with a result whose CBOR-encoded form was the same byte sequence.
    buf.extend_from_slice(b"err\x00");
    buf.extend_from_slice(message.as_bytes());
    buf.push(0);
    if let Some(details) = details {
        ciborium::into_writer(details, &mut buf)
            .expect("CborValue from tau_proto should always serialize back to CBOR");
    }
    buf
}

pub(crate) fn encode_tool_response_for_hash(response: &tau_proto::ToolResponse) -> Vec<u8> {
    encode_for_hash(&response.raw)
}

pub(crate) fn encode_error_response_for_hash(
    message: &str,
    response: &tau_proto::ToolResponse,
) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"err\x00");
    buf.extend_from_slice(message.as_bytes());
    buf.push(0);
    ciborium::into_writer(&response.raw, &mut buf)
        .expect("CborValue from tau_proto should always serialize back to CBOR");
    buf
}

/// BLAKE3 of `bytes`, truncated to 16 bytes. See [`ResultHash`].
pub(crate) fn hash_truncated(bytes: &[u8]) -> ResultHash {
    let digest = blake3::hash(bytes);
    let mut out = [0_u8; 16];
    out.copy_from_slice(&digest.as_bytes()[..16]);
    out
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
