use tau_proto::{CborValue, ToolCallId, ToolName, ToolResultItem, ToolResultStatus, ToolType};

use super::*;

fn cbor_text(s: &str) -> CborValue {
    CborValue::Text(s.to_owned())
}

fn result_entry(call_id: &str, content: &str) -> AgentEntry {
    AgentEntry::ToolResults {
        items: vec![ToolResultItem {
            call_id: ToolCallId::from(call_id),
            tool_type: ToolType::Function,
            status: ToolResultStatus::Success,
            output: tau_proto::ToolResponse::from_cbor(&cbor_text(content)),
            provider_content: Vec::new(),
        }],
    }
}

#[test]
fn rebuild_records_only_above_threshold() {
    let small = "x".repeat(50);
    let big = "y".repeat(1024);
    let entries = vec![
        result_entry("call_small", &small),
        result_entry("call_big", &big),
    ];
    let mut map = ResultDedupMap::new();
    map.rebuild_from_branch(&entries, Some(NodeId::new(1)), DEFAULT_THRESHOLD_BYTES);
    // Only the big entry was over the threshold.
    assert_eq!(map.len(), 1);
    let big_hash = hash_truncated(&encode_tool_response_for_hash(
        &tau_proto::ToolResponse::from_cbor(&cbor_text(&big)),
    ));
    assert_eq!(map.lookup(&big_hash).map(|c| c.as_str()), Some("call_big"),);
}

#[test]
fn rebuild_skips_dedup_pointers() {
    let big = "z".repeat(1024);
    let pointer =
        format!("{INTERNAL_MARKER} read tool output identical to previous tool call: call_x");
    let entries = vec![
        result_entry("call_a", &big),
        // A previously-recorded dedup pointer that was already
        // serving as a stand-in for `call_a`'s content. On
        // rebuild it must NOT enter the map — otherwise a future
        // result whose CBOR happened to match the pointer text
        // would dedup against the wrong (pointer's) call_id.
        result_entry("call_b", &pointer),
    ];
    let mut map = ResultDedupMap::new();
    map.rebuild_from_branch(&entries, Some(NodeId::new(2)), DEFAULT_THRESHOLD_BYTES);
    // call_a entered, call_b's pointer text did not.
    assert_eq!(map.len(), 1);
    let big_hash = hash_truncated(&encode_for_hash(&cbor_text(&big)));
    assert_eq!(map.lookup(&big_hash).map(|c| c.as_str()), Some("call_a"),);
}

#[test]
fn rebuild_keeps_first_call_id_on_duplicate() {
    let big = "q".repeat(1024);
    let entries = vec![
        result_entry("call_first", &big),
        result_entry("call_second", &big),
    ];
    let mut map = ResultDedupMap::new();
    map.rebuild_from_branch(&entries, Some(NodeId::new(2)), DEFAULT_THRESHOLD_BYTES);
    let h = hash_truncated(&encode_for_hash(&cbor_text(&big)));
    assert_eq!(
        map.lookup(&h).map(|c| c.as_str()),
        Some("call_first"),
        "earliest occurrence on the branch must own the slot"
    );
}

#[test]
fn needs_rebuild_detects_head_jump() {
    let mut map = ResultDedupMap::new();
    map.rebuild_from_branch(
        std::iter::empty(),
        Some(NodeId::new(5)),
        DEFAULT_THRESHOLD_BYTES,
    );
    assert!(!map.needs_rebuild(Some(NodeId::new(5))));
    // Linear advance still counts as a rebuild trigger from this
    // helper's POV — the harness handles linear advance via
    // `note_head_advanced_to`, not a rebuild.
    assert!(map.needs_rebuild(Some(NodeId::new(6))));
    assert!(map.needs_rebuild(None));
}

#[test]
fn pointer_value_describes_original_tool_call() {
    let v = build_pointer_value(&ToolCallId::from("call_xyz"), &ToolName::new("read"));
    let CborValue::Text(s) = v else {
        panic!("pointer should always be CborValue::Text");
    };
    assert_eq!(
        s,
        "[tau-internal] read tool output identical to previous tool call: call_xyz"
    );
    assert!(is_dedup_pointer_value(&tau_proto::ToolResponse::from_cbor(
        &CborValue::Text(s),
    )));
}

#[test]
fn pointer_error_message_describes_original_tool_call() {
    let m = build_pointer_error_message(&ToolCallId::from("call_xyz"), &ToolName::new("shell"));
    assert_eq!(
        m,
        "[tau-internal] shell tool output identical to previous tool call: call_xyz"
    );
}
#[test]
fn error_hash_keyspace_is_disjoint_from_result_keyspace() {
    // An error message and a tool result whose CBOR-encoded form
    // is the same string must not collide. The "err\0" prefix on
    // error encoding guarantees this.
    let s = "abc".repeat(200);
    let result_bytes = encode_for_hash(&cbor_text(&s));
    let error_bytes = encode_error_for_hash(&s, None);
    assert_ne!(hash_truncated(&result_bytes), hash_truncated(&error_bytes),);
}

#[test]
fn error_details_distinguish_otherwise_identical_messages() {
    let msg = "compile failed".to_owned();
    let h1 = hash_truncated(&encode_error_for_hash(&msg, None));
    let h2 = hash_truncated(&encode_error_for_hash(
        &msg,
        Some(&cbor_text("error: missing semicolon")),
    ));
    assert_ne!(h1, h2);
}

/// Regression guard: `note_head_advanced_to` must skip the
/// advance when `built_for` is `None`. The harness calls this hook
/// on *every* fold (including ones that don't pass through dedup
/// intake — user messages from session re-init, message projections,
/// `ToolRequest`). On a freshly resumed session the map starts
/// empty with `built_for == None`; if such a fold advanced the
/// cursor unconditionally, `needs_rebuild(new_head)` would return
/// `false` on the next dedup intake and the lazy rebuild would
/// never run, silently losing every historical entry on the
/// branch. A naive "just always set built_for" simplification
/// would re-introduce that bug, which is the exact regression the
/// `dedup_map_rebuilds_on_session_restore` integration test
/// caught during development.
#[test]
fn note_head_advanced_skips_when_built_for_is_none() {
    let mut map = ResultDedupMap::new();
    assert!(map.needs_rebuild(Some(NodeId::new(7))));
    map.note_head_advanced_to(NodeId::new(7));
    assert!(
        map.needs_rebuild(Some(NodeId::new(7))),
        "advancing built_for from None would mark the map as in-sync \
             with a head it has never been populated for, masking the lazy \
             rebuild on the next intake",
    );
}

#[test]
fn note_head_advanced_does_not_clear() {
    let big = "p".repeat(1024);
    let entries = vec![result_entry("call_a", &big)];
    let mut map = ResultDedupMap::new();
    map.rebuild_from_branch(&entries, Some(NodeId::new(1)), DEFAULT_THRESHOLD_BYTES);
    assert_eq!(map.len(), 1);
    map.note_head_advanced_to(NodeId::new(2));
    assert!(!map.needs_rebuild(Some(NodeId::new(2))));
    assert_eq!(map.len(), 1);
}
