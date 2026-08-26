use tau_proto::{CborValue, ToolCallId, ToolName, ToolResultItem, ToolResultStatus, ToolType};

use super::*;

fn cbor_text(s: &str) -> CborValue {
    CborValue::Text(s.to_owned())
}

fn result_entry(call_id: &str, content: &str) -> AgentEntry {
    result_entry_with_status(call_id, ToolResultStatus::Success, content)
}

fn result_entry_with_status(call_id: &str, status: ToolResultStatus, content: &str) -> AgentEntry {
    AgentEntry::ToolResults {
        items: vec![ToolResultItem {
            presentation: Default::default(),
            call_id: ToolCallId::from(call_id),
            tool_type: ToolType::Function,
            status,
            output: tau_proto::ToolResponse::from_cbor(&cbor_text(content)),
            provider_content: Vec::new(),
        }],
    }
}

#[test]
fn rebuild_records_only_above_threshold() {
    let small = "x".repeat(50);
    let big = "y".repeat(1024);
    let entries = [
        result_entry("call_small", &small),
        result_entry("call_big", &big),
    ];
    let mut map = ResultDedupMap::new();
    map.rebuild_from_branch(
        entries
            .iter()
            .enumerate()
            .rev()
            .map(|(index, entry)| (NodeId::new(index as u64), entry)),
        Some(NodeId::new(1)),
        DEFAULT_THRESHOLD_BYTES,
    );
    // Only the big entry was over the threshold.
    assert_eq!(map.len(), 1);
    let big_hash = hash_truncated(&encode_tool_response_for_hash(
        &tau_proto::ToolResponse::from_cbor(&cbor_text(&big)),
    ));
    assert_eq!(map.lookup(&big_hash).map(|c| c.as_str()), Some("call_big"),);
}

#[test]
fn rebuild_does_not_record_short_dedup_pointer() {
    let big = "z".repeat(1024);
    let pointer = "read tool output identical to previous tool call: call_x";
    let entries = [
        result_entry("call_a", &big),
        // Dedup pointers are below the normal threshold and therefore do not
        // enter the map. This is a size rule, not marker recognition.
        result_entry("call_b", pointer),
    ];
    let mut map = ResultDedupMap::new();
    map.rebuild_from_branch(
        entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (NodeId::new(index as u64), entry)),
        Some(NodeId::new(2)),
        DEFAULT_THRESHOLD_BYTES,
    );
    // call_a entered, while call_b's short pointer did not.
    assert_eq!(map.len(), 1);
    let big_hash = hash_truncated(&encode_for_hash(&cbor_text(&big)));
    assert_eq!(map.lookup(&big_hash).map(|c| c.as_str()), Some("call_a"),);
}

/// Raw tool payloads that happen to use the internal envelope spelling remain
/// ordinary dedup candidates. Only typed harness producers may stamp a prompt.
#[test]
fn rebuild_records_large_internal_envelope_spelling_for_every_terminal_status() {
    let success_payload = format!("<tau_internal>success-{}</tau_internal>", "x".repeat(1024));
    let error_payload = format!("<tau_internal>error-{}</tau_internal>", "x".repeat(1024));
    let cancelled_payload = format!(
        "<tau_internal>cancelled-{}</tau_internal>",
        "x".repeat(1024)
    );
    let success =
        result_entry_with_status("call_success", ToolResultStatus::Success, &success_payload);
    let error = result_entry_with_status(
        "call_error",
        ToolResultStatus::Error {
            message: error_payload.clone(),
        },
        &error_payload,
    );
    let cancelled = result_entry_with_status(
        "call_cancelled",
        ToolResultStatus::Cancelled {
            reason: cancelled_payload.clone(),
        },
        &cancelled_payload,
    );
    let mut map = ResultDedupMap::new();
    map.rebuild_from_branch(
        [&success, &error, &cancelled]
            .into_iter()
            .enumerate()
            .map(|(index, entry)| (NodeId::new(index as u64), entry)),
        Some(NodeId::new(3)),
        DEFAULT_THRESHOLD_BYTES,
    );

    let success_response = tau_proto::ToolResponse::from_cbor(&cbor_text(&success_payload));
    let error_response = tau_proto::ToolResponse::from_cbor(&cbor_text(&error_payload));
    let cancelled_response = tau_proto::ToolResponse::from_cbor(&cbor_text(&cancelled_payload));
    let success_hash = hash_truncated(&encode_tool_response_for_hash(&success_response));
    let error_hash = hash_truncated(&encode_error_response_for_hash(
        &error_payload,
        &error_response,
    ));
    let cancelled_hash = hash_truncated(&encode_error_response_for_hash(
        &cancelled_payload,
        &cancelled_response,
    ));
    assert_eq!(map.len(), 3);
    assert_eq!(
        map.lookup(&success_hash).map(|call_id| call_id.as_str()),
        Some("call_success")
    );
    assert_eq!(
        map.lookup(&error_hash).map(|call_id| call_id.as_str()),
        Some("call_error")
    );
    assert_eq!(
        map.lookup(&cancelled_hash).map(|call_id| call_id.as_str()),
        Some("call_cancelled")
    );
}

#[test]
fn rebuild_keeps_first_call_id_on_duplicate() {
    let big = "q".repeat(1024);
    let entries = [
        result_entry("call_first", &big),
        result_entry("call_second", &big),
    ];
    let mut map = ResultDedupMap::new();
    map.rebuild_from_branch(
        entries
            .iter()
            .enumerate()
            .rev()
            .map(|(index, entry)| (NodeId::new(index as u64), entry)),
        Some(NodeId::new(2)),
        DEFAULT_THRESHOLD_BYTES,
    );
    let h = hash_truncated(&encode_for_hash(&cbor_text(&big)));
    assert_eq!(
        map.lookup(&h).map(|c| c.as_str()),
        Some("call_first"),
        "earliest occurrence on the branch must own the slot"
    );
}

/// A single aggregate node keeps model call order authoritative when several
/// full legacy results share one fingerprint.
#[test]
fn rebuild_keeps_first_item_within_one_tool_results_node() {
    let big = "aggregate".repeat(200);
    let entry = AgentEntry::ToolResults {
        items: ["call_first", "call_second"]
            .map(|call_id| {
                let AgentEntry::ToolResults { mut items } = result_entry(call_id, &big) else {
                    unreachable!()
                };
                items.remove(0)
            })
            .into(),
    };
    let mut map = ResultDedupMap::new();
    map.rebuild_from_branch(
        [(NodeId::new(1), &entry)],
        Some(NodeId::new(1)),
        DEFAULT_THRESHOLD_BYTES,
    );

    let hash = fingerprint_value(&cbor_text(&big)).hash;
    assert_eq!(
        map.lookup(&hash).map(ToolCallId::as_str),
        Some("call_first")
    );
}

#[test]
fn needs_rebuild_detects_head_jump() {
    let mut map = ResultDedupMap::new();
    map.rebuild_from_branch(
        std::iter::empty::<(NodeId, &AgentEntry)>(),
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
        "read tool output identical to previous tool call: call_xyz"
    );
}

#[test]
fn pointer_error_message_describes_original_tool_call() {
    let m = build_pointer_error_message(&ToolCallId::from("call_xyz"), &ToolName::new("shell"));
    assert_eq!(
        m,
        "shell tool output identical to previous tool call: call_xyz"
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
    let entries = [result_entry("call_a", &big)];
    let mut map = ResultDedupMap::new();
    map.rebuild_from_branch(
        entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (NodeId::new(index as u64), entry)),
        Some(NodeId::new(1)),
        DEFAULT_THRESHOLD_BYTES,
    );
    assert_eq!(map.len(), 1);
    map.note_head_advanced_to(NodeId::new(2));
    assert!(!map.needs_rebuild(Some(NodeId::new(2))));
    assert_eq!(map.len(), 1);
}

/// A multi-megabyte result must produce the former canonical fingerprint while
/// the production path retains only the fixed-size hasher and work counters.
#[test]
fn large_result_streaming_fingerprint_matches_materialized_oracle() {
    let value = CborValue::Bytes(vec![0x5a; 4 * 1024 * 1024]);
    let oracle = encode_for_hash(&value);
    let (streamed, writes) = fingerprint_value_work(&value);

    assert_eq!(streamed.hash, hash_truncated(&oracle));
    assert_eq!(streamed.encoded_len, oracle.len());
    assert!(writes > 0);
}

/// Rebuilding a long branch must consume the supplied borrowed iterator once,
/// retain only compact anchors, and keep the oldest exact result authoritative.
#[test]
fn long_branch_rebuild_is_single_pass_and_preserves_oldest_anchor() {
    let payload = "long branch payload ".repeat(80);
    let entries = (0..10_000)
        .map(|index| result_entry(&format!("call_{index}"), &payload))
        .collect::<Vec<_>>();
    let mut visited = 0;
    let branch_from_tip = entries
        .iter()
        .enumerate()
        .rev()
        .map(|(index, entry)| (NodeId::new(index as u64), entry))
        .inspect(|_| visited += 1);
    let mut map = ResultDedupMap::new();

    map.rebuild_from_branch(
        branch_from_tip,
        Some(NodeId::new(9_999)),
        DEFAULT_THRESHOLD_BYTES,
    );

    let fingerprint = fingerprint_value(&cbor_text(&payload));
    assert_eq!(visited, entries.len());
    assert_eq!(map.len(), entries.len());
    assert_eq!(
        map.lookup(&fingerprint.hash).map(ToolCallId::as_str),
        Some("call_0")
    );
    let mut probes = 0;
    assert_eq!(
        map.find_matching(&fingerprint.hash, |node_id, call_id| {
            probes += 1;
            node_id == Some(NodeId::new(0)) && call_id.as_str() == "call_0"
        })
        .map(ToolCallId::as_str),
        Some("call_0")
    );
    assert_eq!(probes, 1, "exact oldest anchor lookup is constant work");
}

/// Hash buckets may contain unrelated canonical results. Lookup must ask the
/// borrowed canonical comparator and never treat truncated-hash equality alone
/// as proof of duplicate content.
#[test]
fn hash_collision_candidates_require_exact_canonical_match() {
    let hash = [7; 16];
    let mut map = ResultDedupMap::new();
    map.built_for = Some(NodeId::new(2));
    map.map.insert(
        hash,
        vec![
            DedupAnchor {
                call_id: "newer_collision".into(),
                node_id: Some(NodeId::new(2)),
            },
            DedupAnchor {
                call_id: "older_match".into(),
                node_id: Some(NodeId::new(1)),
            },
        ],
    );

    assert_eq!(
        map.find_matching(&hash, |_, call_id| call_id.as_str() == "older_match")
            .map(ToolCallId::as_str),
        Some("older_match")
    );
    assert!(
        map.find_matching(&hash, |_, _| false).is_none(),
        "hash equality without canonical equality must not dedup"
    );
}

/// Pending anchors are promoted through the call-id index once an aggregate
/// node materializes, so later lookups retain exact O(1) node identity.
#[test]
fn pending_anchor_promotes_to_exact_materialized_node() {
    let hash = [9; 16];
    let mut map = ResultDedupMap::new();
    map.insert(hash, "pending_call".into());
    let mut resolutions = 0;
    map.resolve_pending(|call_id| {
        resolutions += 1;
        (call_id.as_str() == "pending_call").then(|| Some(NodeId::new(42)))
    });

    assert_eq!(resolutions, 1);
    assert_eq!(map.pending_len(), 0);
    assert_eq!(
        map.find_matching(&hash, |node_id, call_id| {
            node_id == Some(NodeId::new(42)) && call_id.as_str() == "pending_call"
        })
        .map(ToolCallId::as_str),
        Some("pending_call")
    );
}

/// Once pending anchors resolve, repeated hot-path promotion checks perform no
/// per-anchor work regardless of the number of retained materialized anchors.
#[test]
fn resolved_anchor_promotion_check_is_constant_work() {
    let mut map = ResultDedupMap::new();
    for index in 0..10_000 {
        map.insert([index as u8; 16], format!("call_{index}").into());
    }
    map.resolve_pending(|call_id| {
        let index = call_id
            .as_str()
            .strip_prefix("call_")
            .expect("call prefix")
            .parse::<u64>()
            .expect("numeric call");
        Some(Some(NodeId::new(index)))
    });
    assert_eq!(map.pending_len(), 0);

    let mut probes = 0;
    for _ in 0..100 {
        map.resolve_pending(|_| {
            probes += 1;
            None
        });
    }
    assert_eq!(probes, 0);
}

/// A pending result that materializes outside the map's selected branch is
/// removed rather than becoming a pointer anchor invisible to the model.
#[test]
fn off_branch_pending_anchor_is_pruned() {
    let hash = [11; 16];
    let mut map = ResultDedupMap::new();
    map.insert(hash, "off_branch".into());
    map.resolve_pending(|_| Some(None));

    assert_eq!(map.pending_len(), 0);
    assert!(map.find_matching(&hash, |_, _| true).is_none());
}
