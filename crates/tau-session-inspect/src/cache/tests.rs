//! Synthetic cache evidence fixtures; never provider traffic or private state.

use std::fs::File;
use std::io::Write as _;

use super::exact_geometry::{ExactRequest, FingerprintKey};
use super::index::IndexState;
use super::*;

/// Operation recognition must not reorder existing prompt-attribution failures,
/// even when the same old-format capture also names another session.
#[test]
fn prompt_attribution_error_precedence_is_unchanged() {
    for (body, reason) in [
        (
            br#"{"session_id":"other"}"#.as_slice(),
            "capture_attribution_unavailable",
        ),
        (
            br#"{"session_id":"other","agent_prompt_id":"../invalid"}"#.as_slice(),
            "capture_attribution_malformed",
        ),
        (
            br#"{"session_id":"other","agent_prompt_id":"prompt"}"#.as_slice(),
            "capture_session_mismatch",
        ),
    ] {
        let root = tempfile::tempdir().expect("fixture root");
        capture(root.path(), "1.json.zst", body);
        let mut inventory = Inventory::default();
        inventory.scan(
            root.path(),
            &"session".parse().expect("session"),
            &CacheScanLimits::default(),
        );
        assert_eq!(inventory.gaps.len(), 1);
        assert_eq!(inventory.gaps[reason], 1);
    }
}

/// Operation captures are recognized without inventing a prompt join, while
/// owner lifecycle remains explicitly outside backend continuity.
#[test]
fn operation_capture_has_backend_continuity_without_prompt_join() {
    let root = tempfile::tempdir().expect("fixture root");
    capture(
        root.path(),
        "1.cache-operation.0123456789abcdef0123456789abcdef.cache-diagnostic.json.zst",
        br#"{"schema":"tau.cache_diagnostic","schema_version":0,"record_kind":"dispatch",
        "session_id":"session","agent_id":"agent","agent_prompt_id":null,"record_seq":1,
        "operation":"cache_refresh","operation_id":"local-operation",
        "logical_attempt":null,"harness_provider_attempt":null,
        "producer_run_id":"0123456789abcdef0123456789abcdef",
        "attempt_id":"0123456789abcdef0123456789abcdef"}"#,
    );
    let mut inventory = Inventory::default();
    inventory.scan(
        root.path(),
        &"session".parse().expect("session"),
        &CacheScanLimits::default(),
    );
    assert!(inventory.prompts.is_empty());
    assert_eq!(
        inventory.diagnostic_count(),
        1,
        "{:?}",
        inventory.diagnostic_sequences()
    );
    assert!(inventory.gaps.is_empty());
    assert!(
        !inventory
            .gaps
            .contains_key("capture_attribution_unavailable")
    );
}

/// The current capture class is inventoried and admitted for explicit
/// Stage-3 projections without misclassifying its schema as stale.
#[test]
fn scalar_cache_capture_is_recognized_for_analysis() {
    let root = tempfile::tempdir().expect("fixture root");
    capture(
        root.path(),
        "1-prompt-cache-diagnostic.json.zst",
        br#"{
        "schema":"tau.cache_diagnostic","schema_version":0,"record_kind":"dispatch",
        "session_id":"session","agent_id":"agent","agent_prompt_id":"prompt","record_seq":1,
        "operation":"standalone_compaction",
        "producer_run_id":"0123456789abcdef0123456789abcdef",
        "attempt_id":"fedcba9876543210fedcba9876543210"
    }"#,
    );
    let mut inventory = Inventory::default();
    inventory.scan(
        root.path(),
        &"session".parse().expect("session identity"),
        &CacheScanLimits::default(),
    );
    assert_eq!(
        inventory
            .prompts
            .values()
            .next()
            .expect("observed prompt")
            .diagnostic_files,
        1
    );
    assert!(inventory.gaps.is_empty());
    assert!(!inventory.gaps.contains_key("unsupported_capture_schema"));
    assert!(!inventory.gaps.contains_key("legacy_partial"));
}

/// Exact duplicate scalar records are idempotent while a conflicting duplicate
/// remains explicit corruption rather than last-write-wins.
#[test]
fn scalar_record_identity_deduplicates_exact_values_and_rejects_conflicts() {
    let root = tempfile::tempdir().expect("fixture root");
    let mut row = diagnostic("dispatch", 1);
    capture_json(root.path(), "1.json.zst", &row);
    capture_json(root.path(), "2.json.zst", &row);
    row["wire_dispatch_index"] = 2.into();
    capture_json(root.path(), "3.json.zst", &row);
    let mut inventory = Inventory::default();
    inventory.scan(
        root.path(),
        &"session".parse().expect("session"),
        &CacheScanLimits::default(),
    );
    assert_eq!(inventory.diagnostic_count(), 1);
    assert!(inventory.gaps["conflicting_cache_diagnostic_record"] > 0);
}

/// Continuity uses only shared capture-local identities and reports missing
/// terminal evidence instead of joining by file order or timestamps.
#[test]
fn continuity_projection_preserves_capture_local_unknowns() {
    let root = tempfile::tempdir().expect("fixture root");
    let mut dispatch = diagnostic("dispatch", 1);
    dispatch["wire_dispatch_index"] = 1.into();
    dispatch["request_form"] = "anchored_suffix".into();
    dispatch["anchor_validation"] = "matched".into();
    dispatch["connection_state"] = "reused".into();
    capture_json(root.path(), "1.json.zst", &dispatch);
    let options = cache_options(root.path(), CacheView::Continuity);
    let mut inventory = Inventory::default();
    inventory.scan(
        root.path(),
        &"session".parse().expect("session"),
        &options.limits,
    );
    let mut report = empty_report();
    let mut remaining = 1_000_000;
    inventory
        .project(
            &options,
            &BTreeSet::from(["agent".parse().expect("agent")]),
            &mut report,
            &mut remaining,
        )
        .expect("project");
    let row = report
        .lines
        .iter()
        .map(|line| serde_json::from_str::<Value>(line).expect("jsonl row"))
        .find(|row| row["record_kind"] == "comparison")
        .expect("comparison");
    assert_eq!(row["derived"]["coverage"], "partial_missing_attempt_end");
    assert_eq!(row["derived"]["visible_prefix_equality"], "unknown");
    assert_eq!(row["reported"]["dispatch_count_observed"], 1);
}

/// Geometry groups only identical closed scalar regimes and labels GCD as an
/// empirical statistic rather than provider cache geometry.
#[test]
fn geometry_projection_labels_empirical_reported_distribution() {
    let root = tempfile::tempdir().expect("fixture root");
    for (sequence, read) in [(1, 384), (3, 640)] {
        let mut dispatch = diagnostic("dispatch", sequence);
        dispatch["effective_model"] = "model".into();
        dispatch["backend"] = "responses".into();
        dispatch["transport"] = "websocket".into();
        dispatch["attempt_id"] = format!("{sequence:032x}").into();
        capture_json(root.path(), &format!("{sequence}.json.zst"), &dispatch);
        let mut end = diagnostic("attempt_end", sequence + 1);
        end["attempt_id"] = format!("{sequence:032x}").into();
        end["reported_usage"] = json!({"read_tokens": read});
        capture_json(root.path(), &format!("{}.json.zst", sequence + 1), &end);
    }
    let options = cache_options(root.path(), CacheView::Geometry);
    let mut inventory = Inventory::default();
    inventory.scan(
        root.path(),
        &"session".parse().expect("session"),
        &options.limits,
    );
    let mut report = empty_report();
    let mut remaining = 1_000_000;
    inventory
        .project(
            &options,
            &BTreeSet::from(["agent".parse().expect("agent")]),
            &mut report,
            &mut remaining,
        )
        .expect("project");
    let row = report
        .lines
        .iter()
        .map(|line| serde_json::from_str::<Value>(line).expect("jsonl row"))
        .find(|row| row["record_kind"] == "comparison")
        .expect("comparison");
    assert_eq!(row["reported"]["read_tokens"], json!([384, 640]));
    assert_eq!(row["derived"]["empirical_gcd_read_tokens"], 128);
    assert_eq!(row["derived"]["exact_cache_geometry"], "unknown");
}

/// Attribution forwards the producer's explicit unsupported state and never
/// transforms raw usage into invented per-item evidence.
#[test]
fn attribution_projection_preserves_unsupported_shape() {
    let root = tempfile::tempdir().expect("fixture root");
    let mut end = diagnostic("attempt_end", 1);
    end["reported_usage"] = json!({"input_tokens": 100, "read_tokens": 40});
    end["attribution_status"] = "unsupported_shape".into();
    end["attribution_total_check"] = "not_checkable".into();
    end["attribution"] = json!([]);
    end["omitted_entries"] = 0.into();
    capture_json(root.path(), "1.json.zst", &end);
    let options = cache_options(root.path(), CacheView::Attribution);
    let mut inventory = Inventory::default();
    inventory.scan(
        root.path(),
        &"session".parse().expect("session"),
        &options.limits,
    );
    let mut report = empty_report();
    let mut remaining = 1_000_000;
    inventory
        .project(
            &options,
            &BTreeSet::from(["agent".parse().expect("agent")]),
            &mut report,
            &mut remaining,
        )
        .expect("project");
    let row = report
        .lines
        .iter()
        .map(|line| serde_json::from_str::<Value>(line).expect("jsonl row"))
        .find(|row| row["record_kind"] == "attribution")
        .expect("attribution");
    assert_eq!(row["reported"]["status"], "unsupported_shape");
    assert_eq!(row["reported"]["entries"], json!([]));
    assert_eq!(row["derived"]["coverage"], "reported_unsupported");
}

/// Agent-scoped projection must not expose another agent's diagnostics merely
/// because both captures live under the same session directory.
#[test]
fn diagnostic_projection_isolates_selected_agents_in_one_session() {
    let root = tempfile::tempdir().expect("fixture root");
    for (sequence, agent, prompt) in [(1, "agent", "prompt"), (2, "other", "other-prompt")] {
        let mut row = diagnostic("dispatch", sequence);
        row["agent_id"] = agent.into();
        row["agent_prompt_id"] = prompt.into();
        row["attempt_id"] = format!("{sequence:032x}").into();
        capture_json(root.path(), &format!("{sequence}.json.zst"), &row);
    }
    let options = cache_options(root.path(), CacheView::Continuity);
    let mut inventory = Inventory::default();
    inventory.scan(
        root.path(),
        &"session".parse().expect("session"),
        &options.limits,
    );
    let mut report = empty_report();
    let mut remaining = 1_000_000;
    inventory
        .project(
            &options,
            &BTreeSet::from(["agent".parse().expect("agent")]),
            &mut report,
            &mut remaining,
        )
        .expect("project");
    let text = report.lines.join("\n");
    assert!(text.contains("\"agent_prompt_id\":\"prompt\""));
    assert!(!text.contains("other-prompt"));
}

/// Retained scalar trees use a conservative allocation charge and stop with an
/// explicit gap instead of consuming the cumulative decode allowance.
#[test]
fn diagnostic_retention_obeys_working_memory_admission() {
    let root = tempfile::tempdir().expect("fixture root");
    for sequence in 1..=4 {
        let mut row = diagnostic("dispatch", sequence);
        row["attempt_id"] = format!("{sequence:032x}").into();
        row["unknown_padding"] = "x".repeat(100_000).into();
        capture_json(root.path(), &format!("{sequence}.json.zst"), &row);
    }
    let limits = CacheScanLimits {
        working_memory_bytes: 64 * 1024 * 1024,
        ..Default::default()
    };
    let mut inventory = Inventory::default();
    inventory.scan(root.path(), &"session".parse().expect("session"), &limits);
    assert!(inventory.diagnostic_count() < 4);
    assert!(
        inventory
            .gaps
            .get("diagnostic_memory_limit")
            .is_some_and(|count| *count > 0),
        "{:?}",
        inventory.gaps
    );
}

/// Conflicting duplicate identities are excluded from evidence regardless of
/// which payload happens to be encountered first.
#[test]
fn conflicting_record_identity_is_never_first_observed_evidence() {
    for reversed in [false, true] {
        let root = tempfile::tempdir().expect("fixture root");
        let first = diagnostic("dispatch", 1);
        let mut second = first.clone();
        second["wire_dispatch_index"] = 2.into();
        let values = if reversed {
            [second, first]
        } else {
            [first, second]
        };
        for (index, value) in values.iter().enumerate() {
            capture_json(root.path(), &format!("{index}.json.zst"), value);
        }
        let options = cache_options(root.path(), CacheView::Continuity);
        let mut inventory = Inventory::default();
        inventory.scan(
            root.path(),
            &"session".parse().expect("session"),
            &options.limits,
        );
        let mut report = empty_report();
        let mut remaining = 1_000_000;
        inventory
            .project(
                &options,
                &BTreeSet::from(["agent".parse().expect("agent")]),
                &mut report,
                &mut remaining,
            )
            .expect("project");
        assert!(report.lines.is_empty());
        assert_eq!(inventory.gaps["conflicting_cache_diagnostic_record"], 1);
    }
}

/// Repair attempts with different dispatch regimes are excluded from geometry
/// rather than selecting one filesystem-order-dependent dispatch.
#[test]
fn geometry_rejects_ambiguous_multi_dispatch_regimes() {
    let root = tempfile::tempdir().expect("fixture root");
    for (sequence, model) in [(1, "model-a"), (2, "model-b")] {
        let mut dispatch = diagnostic("dispatch", sequence);
        dispatch["effective_model"] = model.into();
        dispatch["wire_dispatch_index"] = sequence.into();
        capture_json(root.path(), &format!("{sequence}.json.zst"), &dispatch);
    }
    let mut end = diagnostic("attempt_end", 3);
    end["reported_usage"] = json!({"read_tokens": 384});
    end["dispatch_count"] = 2.into();
    capture_json(root.path(), "3.json.zst", &end);
    let options = cache_options(root.path(), CacheView::Geometry);
    let mut inventory = Inventory::default();
    inventory.scan(
        root.path(),
        &"session".parse().expect("session"),
        &options.limits,
    );
    let mut report = empty_report();
    let mut remaining = 1_000_000;
    inventory
        .project(
            &options,
            &BTreeSet::from(["agent".parse().expect("agent")]),
            &mut report,
            &mut remaining,
        )
        .expect("project");
    assert_eq!(report.gaps["geometry_attempt_regime_ambiguous"], 1);
    assert!(!report.lines.iter().any(|line| {
        serde_json::from_str::<Value>(line).expect("row")["record_kind"] == "comparison"
    }));
}

/// Continuity requires unique, gap-free dispatch indices matching the
/// attempt-end count; lossy subsets and duplicates remain partial.
#[test]
fn continuity_rejects_missing_duplicate_and_holey_dispatch_evidence() {
    for (name, indices, count, reason) in [
        (
            "missing",
            vec![1],
            2,
            "attempt_dispatch_evidence_incomplete",
        ),
        (
            "duplicate",
            vec![1, 1],
            2,
            "attempt_dispatch_index_duplicate",
        ),
        (
            "hole",
            vec![1, 3],
            3,
            "attempt_dispatch_evidence_incomplete",
        ),
    ] {
        let root = tempfile::tempdir().expect("fixture root");
        for (offset, index) in indices.into_iter().enumerate() {
            let mut dispatch = diagnostic("dispatch", offset as u64 + 1);
            dispatch["wire_dispatch_index"] = index.into();
            capture_json(root.path(), &format!("{offset}.json.zst"), &dispatch);
        }
        let mut end = diagnostic("attempt_end", 10);
        end["dispatch_count"] = count.into();
        capture_json(root.path(), "end.json.zst", &end);
        let options = cache_options(root.path(), CacheView::Continuity);
        let mut inventory = Inventory::default();
        inventory.scan(
            root.path(),
            &"session".parse().expect("session"),
            &options.limits,
        );
        let mut report = empty_report();
        let mut remaining = 1_000_000;
        inventory
            .project(
                &options,
                &BTreeSet::from(["agent".parse().expect("agent")]),
                &mut report,
                &mut remaining,
            )
            .expect("project");
        let row = report
            .lines
            .iter()
            .map(|line| serde_json::from_str::<Value>(line).expect("row"))
            .find(|row| row["record_kind"] == "comparison")
            .expect("comparison");
        assert_eq!(row["derived"]["coverage"], reason, "{name}");
        assert_eq!(report.gaps["attempt_continuity_incomplete"], 1, "{name}");
    }
}

/// Geometry does not derive from a retained subset when attempt-end reports a
/// dropped dispatch.
#[test]
fn geometry_requires_complete_dispatch_continuity() {
    let root = tempfile::tempdir().expect("fixture root");
    capture_json(root.path(), "1.json.zst", &diagnostic("dispatch", 1));
    let mut end = diagnostic("attempt_end", 2);
    end["dispatch_count"] = 2.into();
    end["reported_usage"] = json!({"read_tokens": 384});
    capture_json(root.path(), "2.json.zst", &end);
    let options = cache_options(root.path(), CacheView::Geometry);
    let mut inventory = Inventory::default();
    inventory.scan(
        root.path(),
        &"session".parse().expect("session"),
        &options.limits,
    );
    let mut report = empty_report();
    let mut remaining = 1_000_000;
    inventory
        .project(
            &options,
            &BTreeSet::from(["agent".parse().expect("agent")]),
            &mut report,
            &mut remaining,
        )
        .expect("project");
    assert_eq!(report.gaps["attempt_dispatch_evidence_incomplete"], 1);
    assert!(!report.lines.iter().any(|line| {
        serde_json::from_str::<Value>(line).expect("row")["record_kind"] == "comparison"
    }));
}

/// Missing and overlong model identities remain unavailable dimensions rather
/// than one shared null-model geometry group.
#[test]
fn geometry_never_groups_unobserved_models() {
    let root = tempfile::tempdir().expect("fixture root");
    for (base, model) in [(1, None), (3, Some("x".repeat(129)))] {
        let mut dispatch = diagnostic("dispatch", base);
        dispatch["attempt_id"] = format!("{base:032x}").into();
        dispatch["effective_model"] = model.into();
        capture_json(root.path(), &format!("{base}.json.zst"), &dispatch);
        let mut end = diagnostic("attempt_end", base + 1);
        end["attempt_id"] = format!("{base:032x}").into();
        end["reported_usage"] = json!({"read_tokens": 384});
        capture_json(root.path(), &format!("{}.json.zst", base + 1), &end);
    }
    let options = cache_options(root.path(), CacheView::Geometry);
    let mut inventory = Inventory::default();
    inventory.scan(
        root.path(),
        &"session".parse().expect("session"),
        &options.limits,
    );
    let mut report = empty_report();
    let mut remaining = 1_000_000;
    inventory
        .project(
            &options,
            &BTreeSet::from(["agent".parse().expect("agent")]),
            &mut report,
            &mut remaining,
        )
        .expect("project");
    assert_eq!(report.gaps["geometry_model_unavailable"], 2);
    assert!(!report.lines.iter().any(|line| {
        serde_json::from_str::<Value>(line).expect("row")["record_kind"] == "comparison"
    }));
}

/// Configurable grouping omits unselected dimensions, so backend-only analysis
/// can combine model changes without printing or requiring model identities.
#[test]
fn geometry_groups_only_by_requested_dimensions() {
    let root = tempfile::tempdir().expect("fixture root");
    for (base, model, read) in [(1, "model-a", 128), (3, "model-b", 256)] {
        let attempt = format!("{base:032x}");
        let mut dispatch = diagnostic("dispatch", base);
        dispatch["attempt_id"] = attempt.clone().into();
        dispatch["effective_model"] = model.into();
        capture_json(root.path(), &format!("{base}.json.zst"), &dispatch);
        let mut end = diagnostic("attempt_end", base + 1);
        end["attempt_id"] = attempt.into();
        end["reported_usage"] = json!({"read_tokens": read});
        capture_json(root.path(), &format!("{}.json.zst", base + 1), &end);
    }
    let mut options = cache_options(root.path(), CacheView::Geometry);
    options.selection.group_by = vec![CacheGroup::Backend];
    let mut inventory = Inventory::default();
    inventory.scan(
        root.path(),
        &"session".parse().expect("session"),
        &options.limits,
    );
    let mut report = empty_report();
    inventory
        .project(
            &options,
            &BTreeSet::from(["agent".parse().expect("agent")]),
            &mut report,
            &mut 1_000_000,
        )
        .expect("project");
    let comparisons = report
        .lines
        .iter()
        .map(|line| serde_json::from_str::<Value>(line).expect("row"))
        .filter(|row| row["derived"]["method"] == "empirical_reported_read_distribution_v0")
        .collect::<Vec<_>>();
    assert_eq!(comparisons.len(), 1);
    assert_eq!(comparisons[0]["reported"]["read_tokens"], json!([128, 256]));
    assert!(comparisons[0]["derived"]["regime"].get("model").is_none());
    assert!(
        comparisons[0]["derived"]["regime"]
            .get("controls")
            .is_none()
    );
}

/// Shared scalar filters use observed closed fields and count ordinary
/// mismatches without turning them into invented evidence gaps.
#[test]
fn scalar_filters_select_observed_time_model_operation_and_attempt() {
    let root = tempfile::tempdir().expect("fixture root");
    for (sequence, model, attempt) in [(10, "keep", 2), (20, "drop", 3)] {
        let mut dispatch = diagnostic("dispatch", sequence);
        dispatch["attempt_id"] = format!("{sequence:032x}").into();
        dispatch["recorded_at_unix_micros"] = sequence.into();
        dispatch["effective_model"] = model.into();
        dispatch["logical_attempt"] = attempt.into();
        capture_json(root.path(), &format!("{sequence}.json.zst"), &dispatch);
    }
    let mut options = cache_options(root.path(), CacheView::Continuity);
    options.selection.since_unix_micros = Some(5);
    options.selection.until_unix_micros = Some(15);
    options.selection.model = Some("keep".into());
    options.selection.operation = Some(CacheOperation::Inference);
    options.selection.attempt = Some(2);
    let mut inventory = Inventory::default();
    inventory.scan(
        root.path(),
        &"session".parse().expect("session"),
        &options.limits,
    );
    let mut report = empty_report();
    inventory
        .project(
            &options,
            &BTreeSet::from(["agent".parse().expect("agent")]),
            &mut report,
            &mut 1_000_000,
        )
        .expect("project");
    assert_eq!(report.exclusions["scalar_filter_mismatch"], 1);
    assert!(report.lines.iter().all(|line| !line.contains("\"drop\"")));
}

/// Malformed logical attempts and conflicted peer models remain unavailable;
/// neither may fall through to apparently valid lower-authority fields.
#[test]
fn scalar_filters_reject_malformed_attempts_and_conflicted_models() {
    let root = tempfile::tempdir().expect("fixture root");
    let mut malformed = diagnostic("dispatch", 1);
    malformed["logical_attempt"] = "bad".into();
    malformed["harness_provider_attempt"] = 2.into();
    capture_json(root.path(), "malformed.json.zst", &malformed);

    let mut first = diagnostic("dispatch", 2);
    first["record_seq"] = 2.into();
    first["effective_model"] = "model-a".into();
    capture_json(root.path(), "conflict-a.json.zst", &first);
    first["effective_model"] = "model-b".into();
    capture_json(root.path(), "conflict-b.json.zst", &first);
    let mut end = diagnostic("attempt_end", 3);
    end["recorded_at_unix_micros"] = 3.into();
    capture_json(root.path(), "end.json.zst", &end);

    let mut inventory = Inventory::default();
    let mut options = cache_options(root.path(), CacheView::Continuity);
    inventory.scan(
        root.path(),
        &"session".parse().expect("session"),
        &options.limits,
    );

    options.selection.attempt = Some(2);
    let mut report = empty_report();
    inventory
        .project(
            &options,
            &BTreeSet::from(["agent".parse().expect("agent")]),
            &mut report,
            &mut 1_000_000,
        )
        .expect("attempt filter");
    assert!(report.exclusions.contains_key("scalar_attempt_unavailable"));

    options.selection.attempt = None;
    options.selection.model = Some("model-a".into());
    let mut report = empty_report();
    inventory
        .project(
            &options,
            &BTreeSet::from(["agent".parse().expect("agent")]),
            &mut report,
            &mut 1_000_000,
        )
        .expect("model filter");
    assert!(report.exclusions.contains_key("scalar_model_conflicted"));
}

/// Exact request selection metadata stays attached to the full keyed dispatch
/// identity even when two dispatches share prompt attribution and timestamp.
#[test]
fn exact_selection_metadata_uses_full_dispatch_identity() {
    let root = tempfile::tempdir().expect("fixture root");
    for (sequence, attempt, model) in [
        (1, "11111111111111111111111111111111", "model-a"),
        (2, "22222222222222222222222222222222", "model-b"),
    ] {
        let mut scalar = diagnostic("dispatch", sequence);
        scalar["attempt_id"] = attempt.into();
        scalar["recorded_at_unix_micros"] = 10.into();
        scalar["effective_model"] = model.into();
        capture_json(root.path(), &format!("{sequence}-scalar.json.zst"), &scalar);
        capture_json(
            root.path(),
            &format!("{sequence}-request.json.zst"),
            &json!({
                "session_id":"session","agent_prompt_id":"prompt",
                "backend":"responses","transport":"websocket","model":model,
                "attempt_id":attempt,"wire_dispatch_index":1,
                "body":{"model":model,"input":[],"tools":[]}
            }),
        );
    }
    let options = cache_options(root.path(), CacheView::Geometry);
    let mut inventory = Inventory::new(FingerprintKey([8; 32]));
    inventory.scan(
        root.path(),
        &"session".parse().expect("session"),
        &options.limits,
    );
    let mut report = empty_report();
    inventory
        .project(
            &options,
            &BTreeSet::from(["agent".parse().expect("agent")]),
            &mut report,
            &mut 1_000_000,
        )
        .expect("qualify requests");
    let mut models = inventory
        .indexable_exact_requests()
        .into_iter()
        .map(|request| request.model.expect("bounded model"))
        .collect::<Vec<_>>();
    models.sort();
    assert_eq!(models, ["model-a", "model-b"]);
}

/// Conflicted scalar identities and duplicate exact dispatch keys cannot
/// authorize current or indexed exact-request selection metadata.
#[test]
fn exact_selection_rejects_conflicted_and_ambiguous_dispatch_authority() {
    for (name, records, expected_gap) in [
        (
            "conflicted",
            vec![(1, "model-a"), (1, "model-b")],
            "conflicting_cache_diagnostic_record",
        ),
        (
            "ambiguous",
            vec![(1, "model-a"), (2, "model-b")],
            "exact_request_dispatch_ambiguous",
        ),
    ] {
        let root = tempfile::tempdir().expect("fixture root");
        let index_path = root.path().join("private.index");
        let existing =
            IndexState::open(&index_path, "fixture", 1024 * 1024).expect("fresh existing index");
        existing
            .commit("fixture", &[stored_request("old-session")], &[])
            .expect("seed existing index");
        let existing_bytes = std::fs::read(&index_path).expect("read seeded index");
        let attempt = "11111111111111111111111111111111";
        for (sequence, model) in records {
            let mut scalar = diagnostic("dispatch", sequence);
            scalar["record_seq"] = sequence.into();
            scalar["attempt_id"] = attempt.into();
            scalar["effective_model"] = model.into();
            capture_json(
                root.path(),
                &format!("{name}-{sequence}-{model}.json.zst"),
                &scalar,
            );
        }
        capture_json(
            root.path(),
            "request.json.zst",
            &json!({
                "session_id":"session","agent_prompt_id":"prompt",
                "backend":"responses","transport":"websocket","model":"model-a",
                "attempt_id":attempt,"wire_dispatch_index":1,
                "body":{"model":"model-a","input":[],"tools":[]}
            }),
        );
        let options = cache_options(root.path(), CacheView::Geometry);
        let mut inventory = Inventory::new(FingerprintKey([9; 32]));
        inventory.scan(
            root.path(),
            &"session".parse().expect("session"),
            &options.limits,
        );
        let mut report = empty_report();
        inventory
            .project(
                &options,
                &BTreeSet::from(["agent".parse().expect("agent")]),
                &mut report,
                &mut 1_000_000,
            )
            .expect("qualify requests");
        assert!(
            report.gaps.contains_key(expected_gap) || inventory.gaps.contains_key(expected_gap),
            "{name}"
        );
        assert!(
            inventory.indexable_exact_requests().is_empty(),
            "{name} retained ambiguous authority"
        );
        if inventory.index_input_complete() {
            existing
                .commit(
                    "fixture",
                    &inventory.indexable_exact_requests(),
                    &inventory.indexable_exact_responses(),
                )
                .expect("replacement");
        }
        assert_eq!(
            std::fs::read(&index_path).expect("read preserved index"),
            existing_bytes,
            "{name} replaced an index from incomplete input"
        );
    }
}

/// Geometry comparison references must resolve to the emitted attempt-end row,
/// not a dispatch row or a filtered ordinal.
#[test]
fn geometry_input_reference_resolves_to_attempt_end_source() {
    let root = tempfile::tempdir().expect("fixture root");
    capture_json(root.path(), "1.json.zst", &diagnostic("dispatch", 1));
    let mut end = diagnostic("attempt_end", 2);
    end["reported_usage"] = json!({"read_tokens": 384});
    capture_json(root.path(), "2.json.zst", &end);
    let options = cache_options(root.path(), CacheView::Geometry);
    let mut inventory = Inventory::default();
    inventory.scan(
        root.path(),
        &"session".parse().expect("session"),
        &options.limits,
    );
    let mut report = empty_report();
    let mut remaining = 1_000_000;
    inventory
        .project(
            &options,
            &BTreeSet::from(["agent".parse().expect("agent")]),
            &mut report,
            &mut remaining,
        )
        .expect("project");
    let rows = report
        .lines
        .iter()
        .map(|line| serde_json::from_str::<Value>(line).expect("row"))
        .collect::<Vec<_>>();
    let end_label = rows
        .iter()
        .find(|row| row["record_kind"] == "attempt_end")
        .and_then(|row| row["derived"]["record_label"].as_str())
        .expect("attempt-end label");
    let comparison = rows
        .iter()
        .find(|row| row["record_kind"] == "comparison")
        .expect("comparison");
    assert_eq!(comparison["derived"]["input_records"], json!([end_label]));
}

/// Invalid attribution entries cannot retain a producer's complete claim after
/// sanitization drops them.
#[test]
fn malformed_attribution_downgrades_complete_claim() {
    let root = tempfile::tempdir().expect("fixture root");
    let mut end = diagnostic("attempt_end", 1);
    end["attribution_status"] = "complete".into();
    end["attribution_total_check"] = "matches".into();
    end["attribution"] = json!([{"scope":"SECRET","read_tokens":10}]);
    capture_json(root.path(), "1.json.zst", &end);
    let options = cache_options(root.path(), CacheView::Attribution);
    let mut inventory = Inventory::default();
    inventory.scan(
        root.path(),
        &"session".parse().expect("session"),
        &options.limits,
    );
    let mut report = empty_report();
    let mut remaining = 1_000_000;
    inventory
        .project(
            &options,
            &BTreeSet::from(["agent".parse().expect("agent")]),
            &mut report,
            &mut remaining,
        )
        .expect("project");
    let row = report
        .lines
        .iter()
        .map(|line| serde_json::from_str::<Value>(line).expect("row"))
        .find(|row| row["record_kind"] == "attribution")
        .expect("attribution");
    assert_eq!(row["derived"]["coverage"], "sanitized_partial");
    assert_eq!(row["reported"]["entries"], json!([]));
    assert_eq!(
        report.gaps["attribution_evidence_unavailable_or_partial"],
        1
    );
    assert!(!report.lines.join("\n").contains("SECRET"));
}

/// Attribution requests with only dispatch evidence, or with no selected
/// diagnostics, return explicit partial gaps rather than empty success.
#[test]
fn attribution_reports_missing_attempt_end_and_missing_selection() {
    let root = tempfile::tempdir().expect("fixture root");
    capture_json(root.path(), "1.json.zst", &diagnostic("dispatch", 1));
    let options = cache_options(root.path(), CacheView::Attribution);
    let mut inventory = Inventory::default();
    inventory.scan(
        root.path(),
        &"session".parse().expect("session"),
        &options.limits,
    );
    for (agent, reason) in [
        ("agent", "attribution_attempt_end_missing"),
        ("other", "selected_cache_diagnostic_missing"),
    ] {
        let mut report = empty_report();
        let mut remaining = 1_000_000;
        inventory
            .project(
                &options,
                &BTreeSet::from([agent.parse().expect("agent")]),
                &mut report,
                &mut remaining,
            )
            .expect("project");
        assert_eq!(report.gaps[reason], 1);
    }
}

/// Realized reads, including perfect hits, never invent an eligible ceiling.
#[test]
fn reads_do_not_synthesize_eligibility() {
    let perfect = metrics(100, 100, None);
    assert_eq!(perfect["share_of_input"], 1.0);
    assert_eq!(perfect["eligibility_evidence"], "unknown");
    assert!(perfect["eligibility_utilization"].is_null());
    assert!(metrics(0, 0, Some(0))["share_of_input"].is_null());
    assert!(metrics(0, 0, Some(0))["eligibility_utilization"].is_null());
}

/// Invalid counters stay invalid instead of being capped into credible
/// evidence.
#[test]
fn invalid_read_and_ceiling_evidence_is_not_repaired() {
    assert!(metrics(10, 11, None)["non_read_input"].is_null());
    assert_eq!(metrics(10, 11, None)["input_read_evidence"], "invalid");
    assert_eq!(metrics(10, 5, Some(4))["eligibility_evidence"], "invalid");
    assert_eq!(metrics(10, 5, Some(11))["eligibility_evidence"], "invalid");
    assert_eq!(metrics(10, 5, Some(10))["eligibility_utilization"], 0.5);
}

/// Writes one synthetic private zstd capture under the managed directory shape.
fn capture(root: &std::path::Path, name: &str, body: &[u8]) {
    std::fs::create_dir_all(root.join("provider")).expect("capture directory");
    let bytes = zstd::stream::encode_all(body, 3).expect("compress fixture");
    std::fs::write(root.join("provider").join(name), bytes).expect("write fixture");
}

/// Writes one strict JSON fixture through the existing compressed capture
/// helper.
fn capture_json(root: &std::path::Path, name: &str, value: &Value) {
    capture(
        root,
        name,
        &serde_json::to_vec(value).expect("encode capture"),
    );
}

/// Builds one current prompt-scoped scalar record with stable typed
/// attribution.
fn diagnostic(kind: &str, sequence: u64) -> Value {
    let mut value = json!({
        "schema": "tau.cache_diagnostic",
        "schema_version": 0,
        "record_kind": kind,
        "producer_run_id": "0123456789abcdef0123456789abcdef",
        "record_seq": sequence,
        "attempt_id": "fedcba9876543210fedcba9876543210",
        "session_id": "session",
        "agent_id": "agent",
        "agent_prompt_id": "prompt",
        "operation": "inference",
        "operation_id": "prompt",
        "logical_attempt": 1,
        "harness_provider_attempt": 1,
        "backend": "responses",
        "transport": "websocket",
        "effective_model": "fixture-model"
    });
    if kind == "dispatch" {
        value["wire_dispatch_index"] = 1.into();
        value["request_form"] = "full".into();
        value["anchor_validation"] = "not_applicable".into();
        value["connection_state"] = "new".into();
    } else {
        value["dispatch_count"] = 1.into();
        value["successful_dispatch_index"] = 1.into();
        value["outcome"] = "success".into();
        value["attribution_status"] = "absent".into();
        value["attribution_total_check"] = "not_checkable".into();
        value["attribution"] = json!([]);
        value["omitted_entries"] = 0.into();
    }
    value
}

/// Creates options for a direct inventory projection fixture.
fn cache_options(root: &std::path::Path, view: CacheView) -> CacheOptions {
    CacheOptions {
        state_dir: root.into(),
        scope: CacheScope::Agent {
            agent_id: "agent".parse().expect("agent"),
            include_descendants: false,
        },
        prompt: None,
        selection: Default::default(),
        view,
        limits: CacheScanLimits::default(),
        producer_build: "fixture".into(),
        index: None,
    }
}

/// Creates an empty report sink for direct projection tests.
fn empty_report() -> CacheReport {
    CacheReport {
        lines: Vec::new(),
        partial: false,
        responses: 0,
        gaps: BTreeMap::new(),
        exclusions: BTreeMap::new(),
        inspected: true,
        index_written: false,
    }
}

/// Multiple same-prompt files remain file counts, never inferred attempt joins.
#[test]
fn legacy_files_have_explicit_partial_coverage_without_payload_export() {
    let root = tempfile::tempdir().expect("fixture root");
    let body = br#"{"session_id":"session","agent_prompt_id":"prompt","body":{"secret":"CREDENTIAL","previous_response_id":"PRIVATE_RESPONSE"}}"#;
    capture(root.path(), "1.json.zst", body);
    capture(root.path(), "2.json.zst", body);
    let mut inventory = Inventory::default();
    inventory.scan(
        root.path(),
        &"session".parse().expect("session id"),
        &CacheScanLimits::default(),
    );
    assert_eq!(inventory.gaps["legacy_partial"], 2);
    let counts = inventory.prompts.values().next().expect("capture counts");
    assert_eq!(counts.request_files, 2);
    let output = serde_json::to_string(counts).expect("encode counts");
    assert!(!output.contains("CREDENTIAL"));
    assert!(!output.contains("PRIVATE_RESPONSE"));
}

/// Torn compression and compressed limits cannot silently turn into empty
/// success.
#[test]
fn torn_and_bounded_capture_files_are_counted_gaps() {
    let root = tempfile::tempdir().expect("fixture root");
    capture(root.path(), "1.json.zst", br#"{"body":{}}"#);
    let path = root.path().join("provider/1.json.zst");
    let mut bytes = std::fs::read(&path).expect("read fixture");
    bytes.truncate(bytes.len() - 2);
    std::fs::write(&path, bytes).expect("truncate fixture");
    let mut inventory = Inventory::default();
    inventory.scan(
        root.path(),
        &"session".parse().expect("session id"),
        &CacheScanLimits::default(),
    );
    assert_eq!(inventory.gaps["truncated_or_malformed_compression"], 1);
    let limits = CacheScanLimits {
        compressed_file_bytes: 1,
        ..Default::default()
    };
    let mut inventory = Inventory::default();
    inventory.scan(
        root.path(),
        &"session".parse().expect("session id"),
        &limits,
    );
    assert_eq!(inventory.gaps["compressed_capture_limit"], 1);
}

/// Unknown schemas are explicitly unsupported, never treated as legacy records.
#[test]
fn unsupported_schema_is_not_legacy_success() {
    let root = tempfile::tempdir().expect("fixture root");
    capture(
        root.path(),
        "1.json.zst",
        br#"{"schema":"tau.cache_diagnostic","schema_version":99}"#,
    );
    let mut inventory = Inventory::default();
    inventory.scan(
        root.path(),
        &"session".parse().expect("session id"),
        &CacheScanLimits::default(),
    );
    assert_eq!(inventory.gaps["unsupported_capture_schema"], 1);
    assert!(inventory.prompts.is_empty());
}

/// Duplicate fields cannot overwrite typed attribution or hidden nested
/// evidence.
#[test]
fn duplicate_json_keys_are_rejected_recursively() {
    for bytes in [
        br#"{"session_id":"one","session_id":"two"}"#.as_slice(),
        br#"{"body":{"a":1,"a":2}}"#.as_slice(),
    ] {
        assert!(serde_json::from_slice::<strict_json::StrictJson>(bytes).is_err());
    }
}

/// Decompression admission and cumulative budgets both expose partial results.
#[test]
fn decoded_and_total_limits_are_explicit() {
    let root = tempfile::tempdir().expect("fixture root");
    capture(root.path(), "1.json.zst", &[b' '; 1000]);
    capture(root.path(), "2.json.zst", &[b' '; 1000]);
    let limits = CacheScanLimits {
        decompressed_file_bytes: 10,
        total_decompressed_bytes: 10,
        ..Default::default()
    };
    let mut inventory = Inventory::default();
    inventory.scan(
        root.path(),
        &"session".parse().expect("session id"),
        &limits,
    );
    assert_eq!(inventory.gaps["decoded_or_memory_capture_limit"], 1);
    assert_eq!(inventory.gaps["cumulative_capture_limit"], 1);
}

/// Preflight rejects over-budget journals without attempting strict replay or
/// repair.
#[test]
fn journal_preflight_produces_partial_without_inspection_or_mutation() {
    let root = tempfile::tempdir().expect("fixture root");
    let dir = root.path().join("agents/agent");
    std::fs::create_dir_all(&dir).expect("journal directory");
    let path = dir.join("events.cbor");
    let mut file = File::create(&path).expect("journal fixture");
    file.write_all(&[0; 129]).expect("journal bytes");
    let options = CacheOptions {
        state_dir: root.path().into(),
        scope: CacheScope::Agent {
            agent_id: "agent".parse().expect("agent id"),
            include_descendants: false,
        },
        prompt: None,
        selection: Default::default(),
        view: CacheView::Summary,
        limits: CacheScanLimits {
            working_memory_bytes: 65536,
            ..Default::default()
        },
        producer_build: "synthetic-build".into(),
        index: None,
    };
    let report = read_cache_report(&options).expect("partial report");
    assert!(report.is_partial());
    assert!(!report.inspected);
    assert_eq!(report.gaps["journal_memory_preflight_limit"], 1);
    assert_eq!(
        std::fs::read(path).expect("unchanged journal"),
        vec![0; 129]
    );
    assert!(!dir.join("lock").exists());
}

/// Exact admission arithmetic is inclusive and saturates both multiplication
/// and sum.
#[test]
fn journal_preflight_boundary_and_overflow_cannot_wrap() {
    assert_eq!(journal_memory_charge(0, 128), 65536 / 4);
    assert!(journal_memory_charge(0, 129) > 65536 / 4);
    assert_eq!(journal_memory_charge(0, u64::MAX), u64::MAX);
    assert_eq!(journal_memory_charge(u64::MAX - 1, 1), u64::MAX);
}

/// Exact request geometry joins only through attempt/dispatch identity, reports
/// ordered structural prefix evidence, and never emits private values or
/// hashes.
#[test]
fn exact_request_geometry_is_content_free_and_does_not_claim_tokens_or_residency() {
    let root = tempfile::tempdir().expect("fixture root");
    for (sequence, attempt, prompt, input) in [
        (
            1,
            "11111111111111111111111111111111",
            "prompt-1",
            json!([{"role":"user","content":"PRIVATE-A"}]),
        ),
        (
            2,
            "22222222222222222222222222222222",
            "prompt-2",
            json!([
                {"role":"user","content":"PRIVATE-A"},
                {"role":"assistant","content":"PRIVATE-B"}
            ]),
        ),
    ] {
        let mut scalar = diagnostic("dispatch", sequence);
        scalar["attempt_id"] = attempt.into();
        scalar["agent_prompt_id"] = prompt.into();
        scalar["operation_id"] = prompt.into();
        scalar["recorded_at_unix_micros"] = sequence.into();
        capture_json(root.path(), &format!("{sequence}-scalar.json.zst"), &scalar);
        capture_json(
            root.path(),
            &format!("{sequence}-request.json.zst"),
            &json!({
                "session_id":"session","agent_prompt_id":prompt,
                "backend":"responses","transport":"websocket","model":"model",
                "attempt_id":attempt,"wire_dispatch_index":1,
                "body":{"model":"model","input":input,"tools":[],
                    "reasoning":{"effort":"high"},"unknown":{"value":"PRIVATE-C"}}
            }),
        );
    }
    let options = cache_options(root.path(), CacheView::Geometry);
    let mut inventory = Inventory::new(FingerprintKey([4; 32]));
    inventory.scan(
        root.path(),
        &"session".parse().expect("session"),
        &options.limits,
    );
    let mut report = empty_report();
    let mut remaining = 1_000_000;
    inventory
        .project(
            &options,
            &BTreeSet::from(["agent".parse().expect("agent")]),
            &mut report,
            &mut remaining,
        )
        .expect("geometry");
    let output = report.lines.join("\n");
    assert!(output.contains("exact_captured_request_comparison_v0"));
    assert!(output.contains("\"common_captured_input_items\":1"));
    assert!(output.contains("\"provider_tokenization\":\"unknown\""));
    assert!(output.contains("\"provider_residency\":\"unknown\""));
    assert!(output.contains("\"raw_wire_byte_equality\":\"unavailable\""));
    for private in [
        "PRIVATE-A",
        "PRIVATE-B",
        "PRIVATE-C",
        "11111111",
        "22222222",
    ] {
        assert!(!output.contains(private), "{private} leaked: {output}");
    }

    let mut exact_only = options;
    exact_only.selection.require_exact_chain = true;
    let mut report = empty_report();
    inventory
        .project(
            &exact_only,
            &BTreeSet::from(["agent".parse().expect("agent")]),
            &mut report,
            &mut 1_000_000,
        )
        .expect("exact-chain projection");
    assert!(
        !report
            .lines
            .join("\n")
            .contains("exact_captured_request_comparison_v0")
    );
    assert_eq!(report.exclusions["exact_chain_not_equal"], 1);
}

/// Request qualification is independent of the selected report view so an
/// explicit summary-view index first-write and reopen retain current evidence.
#[test]
fn non_geometry_view_qualifies_first_index_write_and_same_path_reuse() {
    let root = tempfile::tempdir().expect("fixture root");
    let mut scalar = diagnostic("dispatch", 1);
    scalar["attempt_id"] = "11111111111111111111111111111111".into();
    scalar["recorded_at_unix_micros"] = 1.into();
    capture_json(root.path(), "1-scalar.json.zst", &scalar);
    capture_json(
        root.path(),
        "2-request.json.zst",
        &json!({
            "session_id":"session","agent_prompt_id":"prompt",
            "backend":"responses","transport":"websocket","model":"model",
            "attempt_id":"11111111111111111111111111111111",
            "wire_dispatch_index":1,
            "body":{"model":"model","input":[],"tools":[]}
        }),
    );
    let index_path = root.path().join("private.index");
    let mut options = cache_options(root.path(), CacheView::Summary);
    options.index = Some(index_path.clone());
    let mut inventory = Inventory::new(FingerprintKey([5; 32]));
    inventory.scan(
        root.path(),
        &"session".parse().expect("session"),
        &options.limits,
    );
    let mut report = empty_report();
    inventory
        .project(
            &options,
            &BTreeSet::from(["agent".parse().expect("agent")]),
            &mut report,
            &mut 1_000_000,
        )
        .expect("summary projection");
    let requests = inventory.indexable_exact_requests();
    assert_eq!(requests.len(), 1);
    let state = IndexState::open(&index_path, "fixture", 1024 * 1024).expect("new index");
    state
        .commit("fixture", &requests, &[])
        .expect("first index write");
    let reopened = IndexState::open(&index_path, "fixture", 1024 * 1024).expect("reuse index");
    assert_eq!(reopened.requests.len(), 1);
    assert!(reopened.requests[0].indexed);
}

/// Any malformed candidate capture blocks replacement admission, preserving an
/// existing index instead of silently forgetting skipped evidence.
#[test]
fn malformed_capture_blocks_index_replacement_admission() {
    let root = tempfile::tempdir().expect("fixture root");
    capture(
        root.path(),
        "malformed.json.zst",
        br#"{"body":{"a":1,"a":2}}"#,
    );
    let mut inventory = Inventory::new(FingerprintKey([6; 32]));
    inventory.scan(
        root.path(),
        &"session".parse().expect("session"),
        &CacheScanLimits::default(),
    );
    assert_eq!(inventory.gaps["malformed_or_ambiguous_capture_json"], 1);
    assert!(!inventory.index_input_complete());
}

/// A reused index never projects one session's structural evidence into another
/// session merely because the same agent participates in both.
#[test]
fn indexed_exact_evidence_isolated_by_selected_session() {
    let root = tempfile::tempdir().expect("fixture root");
    let mut inventory = Inventory::new(FingerprintKey([7; 32]));
    inventory.extend_index(vec![stored_request("session-a")], Vec::new());
    let options = CacheOptions {
        state_dir: root.path().into(),
        scope: CacheScope::Session("session-b".parse().expect("session")),
        prompt: None,
        selection: Default::default(),
        view: CacheView::Geometry,
        limits: CacheScanLimits::default(),
        producer_build: "fixture".into(),
        index: Some(root.path().join("private.index")),
    };
    let mut report = empty_report();
    inventory
        .project(
            &options,
            &BTreeSet::from(["agent".parse().expect("agent")]),
            &mut report,
            &mut 1_000_000,
        )
        .expect("geometry projection");
    assert!(
        !report
            .lines
            .join("\n")
            .contains("exact_captured_request_shape_v0")
    );
}

/// Builds one body-free indexed request fixture.
fn stored_request(session: &str) -> ExactRequest {
    ExactRequest {
        session: session.into(),
        agent: "agent".into(),
        prompt: "prompt".into(),
        instance: "i".repeat(64),
        attempt: Some("a".repeat(64)),
        dispatch: Some(1),
        adapter: "responses".into(),
        body: "b".repeat(64),
        instructions: None,
        tools: "t".repeat(64),
        controls: "c".repeat(64),
        other: "o".repeat(64),
        route: "r".repeat(64),
        cache_key: None,
        previous_response: None,
        items: Vec::new(),
        prefixes: Vec::new(),
        complete: true,
        indexed: true,
        recorded_at_unix_micros: Some(1),
        request_form: Some("full".into()),
        model: Some("fixture-model".into()),
        operation: Some("inference".into()),
        attempt_ordinal: Some(1),
    }
}

/// Current producer envelopes remain recognized without exposing their raw
/// payloads.
#[test]
fn current_provider_capture_envelopes_are_inventory_not_unsupported_schema() {
    let root = tempfile::tempdir().expect("fixture root");
    // Shapes mirror current producer serializers, including nullable usage and
    // the separately existing versioned failure records (not new migrations).
    let envelopes = [
        json!({"backend":"chat_completions","transport":"http-sse","model":"model",
            "operation":"inference","logical_attempt":1,"wire_dispatch_index":1,
            "body":{"messages":[{"content":"PRIVATE"}]}}),
        json!({"backend":"responses","transport":"websocket","model":"model",
            "body":{"input":[]}}),
        json!({"backend":"chat_completions","transport":"http-sse","model":"model",
            "operation":"inference","logical_attempt":1,"wire_dispatch_index":1,
            "usage":null,"stop_reason":"end_turn","output_items":[],"raw_events":[]}),
        json!({"backend":"responses","transport":"http-sse","model":"model",
            "provider_response_id":"PRIVATE_ID","usage":null,"stop_reason":"end_turn",
            "response_bytes_received":10,"raw_events":[],"raw_events_truncated":false}),
        json!({"backend":{"kind":"responses","transport":"websocket"},
            "provider_response_id":"PRIVATE_ID","usage":null,
            "provider_response_finished":{},"provider_terminal_event":null}),
        json!({"backend":"chat_completions","transport":"http-sse","model":"model",
            "operation":"inference","logical_attempt":1,"wire_dispatch_index":1,
            "http_status":503,"body":"PRIVATE_ERROR"}),
        json!({"backend":"responses","transport":"http-sse","model":"model",
            "response_bytes_received":10,"error":{"kind":"http","body":"PRIVATE_ERROR"}}),
        current_attempt_failure(),
        current_compact_failure(),
    ];
    for (index, mut envelope) in envelopes.into_iter().enumerate() {
        envelope["session_id"] = "session".into();
        envelope["agent_prompt_id"] = "prompt".into();
        capture(
            root.path(),
            &format!("{index}.json.zst"),
            &serde_json::to_vec(&envelope).expect("encode envelope"),
        );
    }
    let mut inventory = Inventory::default();
    inventory.scan(
        root.path(),
        &"session".parse().expect("session"),
        &CacheScanLimits::default(),
    );
    assert_eq!(inventory.gaps, BTreeMap::from([("legacy_partial", 9)]));
    let counts = inventory.prompts.values().next().expect("prompt inventory");
    assert_eq!(counts.request_files, 2);
    assert_eq!(counts.response_files, 3);
    assert_eq!(counts.failure_files, 4);
    assert!(
        !serde_json::to_string(counts)
            .expect("counts")
            .contains("PRIVATE")
    );
}

/// A current producer-exact finite attempt with no established transport
/// evidence.
fn current_attempt_failure() -> Value {
    json!({
        "schema_version":1,"capture_kind":"provider_attempt_failure",
        "session_id":"session","agent_prompt_id":"prompt",
        "operation":"inference","logical_attempt":1,"wire_dispatch_index":null,
        "backend":{"kind":"responses","transport_intent":"websocket","transport_established":false},
        "outcome":"retry_scheduled",
        "classification":{"category":"transport","retry_after_secs":null},
        "wire":{"wire_dispatches":0,"repair_used":false,"response_bytes_received":0,"semantic_progress":"none"},
        "provider":null,"transport":null,
        "truncation":{"total":false,"shape":false,"identifiers":false}
    })
}

/// A current producer-exact compact failure with complete empty decoded body
/// and no headers.
fn current_compact_failure() -> Value {
    json!({
        "schema_version":0,"capture_kind":"compact_http_failure",
        "session_id":"session","agent_prompt_id":"prompt",
        "operation":"compact","backend":{"kind":"responses","transport":"unary_http"},
        "http":{"status":503,"headers":{"content_type":null,"retry_after":null,
            "request_id":null,"openai_request_id":null,"x_request_id":null}},
        "body":{"decoded_bytes_received":0,"retained_bytes":0,"complete":true,
            "truncated":false,"redacted_prefix_truncated":false,
            "sha256_decoded_received":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "sha256_coverage":"complete_decoded_body","redacted_decoded_prefix_base64":""}
    })
}

/// Recognized discriminators do not make missing or wrong-type evidence
/// credible.
#[test]
fn malformed_current_failure_fields_are_partial_not_valid_inventory() {
    let root = tempfile::tempdir().expect("fixture root");
    let mut malformed = Vec::new();
    for base in [current_attempt_failure(), current_compact_failure()] {
        for name in base.as_object().expect("fixture object").keys() {
            if [
                "schema_version",
                "capture_kind",
                "session_id",
                "agent_prompt_id",
            ]
            .contains(&name.as_str())
            {
                continue;
            }
            let mut missing = base.clone();
            missing.as_object_mut().expect("object").remove(name);
            malformed.push(missing);
        }
    }
    for (mut value, pointer) in [
        (current_compact_failure(), "/body/decoded_bytes_received"),
        (current_compact_failure(), "/body/complete"),
        (current_compact_failure(), "/body/sha256_decoded_received"),
        (current_compact_failure(), "/http/headers"),
        (current_attempt_failure(), "/wire/response_bytes_received"),
        (current_attempt_failure(), "/wire/semantic_progress"),
        (current_attempt_failure(), "/backend/transport_established"),
        (
            current_attempt_failure(),
            "/classification/retry_after_secs",
        ),
        (current_attempt_failure(), "/truncation/total"),
    ] {
        *value.pointer_mut(pointer).expect("existing field") = json!([]);
        malformed.push(value);
    }
    let expected = malformed.len() as u64;
    for (index, value) in malformed.into_iter().enumerate() {
        capture(
            root.path(),
            &format!("{index}.json.zst"),
            &serde_json::to_vec(&value).expect("encode malformed"),
        );
    }
    let mut inventory = Inventory::default();
    inventory.scan(
        root.path(),
        &"session".parse().expect("session"),
        &CacheScanLimits::default(),
    );
    assert_eq!(
        inventory.gaps,
        BTreeMap::from([("malformed_current_failure_capture", expected)])
    );
    assert!(inventory.prompts.is_empty());
}

/// Nullable provider/transport evidence is type-checked recursively, not
/// discarded unchecked.
#[test]
fn current_failure_nested_optional_shapes_are_validated() {
    let mut attempt = current_attempt_failure();
    attempt["provider"] = json!({"terminal_event_type":null,"canonical_error_code":null,
        "provider_request_id":"request","provider_response_id":null,
        "message":{"present":false,"utf8_bytes":0,"unicode_scalars":0},
        "terminal_event_shape":null});
    attempt["transport"] = json!({"phase":"response_stream","kind":"websocket_close",
        "ws_close_code":1000,"ws_close_reason":{"present":false,"utf8_bytes":0,"unicode_scalars":0},
        "clean_eof":false,"frame_bytes":null});
    assert!(failure_shape::attempt(&attempt));
    attempt["provider"]["message"]["present"] = "false".into();
    assert!(!failure_shape::attempt(&attempt));

    let bytes = json!({"original_bytes":4,"retained_bytes":4,"truncated":false,
        "base64":"b29wcw==","utf8":"oops","original_unicode_scalars":4,"retained_unicode_scalars":4});
    let mut compact = current_compact_failure();
    compact["http"]["headers"]["request_id"] = bytes.clone();
    compact["body"]["parsed_error"] = json!({"code":bytes});
    assert!(failure_shape::compact(&compact));
    compact["body"]["parsed_error"]["code"]["retained_bytes"] = (-1).into();
    assert!(!failure_shape::compact(&compact));
}

/// Known discriminators at future versions remain unsupported rather than
/// malformed-current.
#[test]
fn future_failure_versions_are_not_current_shape_fallbacks() {
    let root = tempfile::tempdir().expect("fixture root");
    for (index, mut value) in [current_attempt_failure(), current_compact_failure()]
        .into_iter()
        .enumerate()
    {
        value["schema_version"] = 99.into();
        capture(
            root.path(),
            &format!("{index}.json.zst"),
            &serde_json::to_vec(&value).expect("future fixture"),
        );
    }
    let mut inventory = Inventory::default();
    inventory.scan(
        root.path(),
        &"session".parse().expect("session"),
        &CacheScanLimits::default(),
    );
    assert_eq!(
        inventory.gaps,
        BTreeMap::from([("unsupported_capture_schema", 2)])
    );
    assert!(inventory.prompts.is_empty());
}
