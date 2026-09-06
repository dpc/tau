use std::fmt::Write as _;

use serde_json::{Value, json};

use super::*;
use crate::cache_diagnostic::tests::collect;

/// Collect exact captures without initializing the process-wide writer.
fn exact<T>(run: impl FnOnce() -> T) -> (T, Vec<Value>) {
    /// Restore the exact sink even when an assertion unwinds.
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_DEBUG_CAPTURES.with(|captures| *captures.borrow_mut() = None);
        }
    }
    TEST_DEBUG_CAPTURES.with(|captures| {
        assert!(captures.borrow().is_none());
        *captures.borrow_mut() = Some(Vec::new());
    });
    let _reset = Reset;
    let result = run();
    let captures = TEST_DEBUG_CAPTURES.with(|captures| {
        captures
            .borrow_mut()
            .take()
            .expect("exact sink")
            .into_iter()
            .map(|capture| serde_json::from_slice(capture.json()).expect("exact JSON"))
            .collect()
    });
    (result, captures)
}

/// Exercise the real lowering/send/parser path and both private capture
/// classes.
fn run(
    events: String,
    compat: CacheUsageCompat,
    policy: CacheDiagnostics,
    persistable: bool,
    operation: tau_proto::PromptOperation,
) -> (AttemptOutcome, Vec<Value>, Vec<Value>) {
    let server = spawn_sse_server(events);
    let mut config = provider();
    config.base_url = format!("http://{}/v1", server.address());
    config.compat.cache_usage = compat;
    let mut prompt = prompt();
    prompt.operation = operation;
    let mut config_resolved = resolved_provider(&config);
    if operation == tau_proto::PromptOperation::StandaloneCompaction {
        config_resolved.local_summary_compaction =
            LocalSummaryCompactionConfig::default_for(128_000);
        prompt
            .context
            .blocks
            .push(tau_proto::ContextBlock::UserInput(
                tau_proto::UserInputBlock {
                    items: vec![ContextItem::CompactionTrigger],
                },
            ));
    }
    let ((outcome, rows), exact) = exact(|| {
        collect(|| {
            run_attempt_with_diagnostics(
                tau_proto::ProviderAttempt::new(7).expect("attempt"),
                &prompt,
                &config_resolved,
                &config.models[0],
                persistable,
                policy,
                &mut |_| {},
                &mut || false,
                &tau_provider::OutboundNetworkPolicy::from_environment(BTreeMap::new(), None),
            )
        })
    });
    server.finish();
    (outcome, rows, exact)
}

/// Build one semantic success while retaining caller-selected usage events.
fn success_events(usage: &[Value]) -> String {
    let mut events = String::from(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"kept\"},\"finish_reason\":\"stop\"}]}\n\n",
    );
    for value in usage {
        let _ = write!(&mut events, "data: {}\n\n", json!({"usage": value}));
    }
    events.push_str("data: [DONE]\n\n");
    events
}

/// Actual sends and exact captures share identity; raw reads remain unclamped
/// while existing normalized accounting keeps its own cap.
#[test]
fn successful_attempt_correlates_exact_captures_and_raw_usage() {
    let (outcome, rows, exact) = run(
        success_events(&[json!({
            "prompt_tokens": 10, "completion_tokens": 2,
            "prompt_tokens_details": {"cached_tokens": 99, "cache_write_tokens": 88},
        })]),
        CacheUsageCompat::OpenAi,
        CacheDiagnostics::Metadata,
        true,
        tau_proto::PromptOperation::Inference,
    );
    let AttemptOutcome::Completed(success) = outcome else {
        panic!("success")
    };
    assert_eq!(
        success.usage.expect("canonical usage").prompt_cached_tokens,
        10
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["record_kind"], "dispatch");
    assert_eq!(rows[0]["wire_dispatch_index"], 1);
    assert_eq!(rows[0]["logical_attempt"], 7);
    assert_eq!(rows[0]["harness_provider_attempt"], 7);
    assert_eq!(rows[0]["request_form"], "full");
    assert_eq!(rows[1]["outcome"], "success");
    assert_eq!(rows[1]["dispatch_count"], 1);
    assert_eq!(rows[1]["successful_dispatch_index"], 1);
    assert_eq!(rows[1]["reported_usage"]["read_tokens"], 99);
    assert_eq!(rows[1]["reported_usage"]["write_tokens"], 88);
    assert_eq!(exact.len(), 2);
    for capture in exact.iter().chain(rows.iter()) {
        assert_eq!(capture["attempt_id"], rows[0]["attempt_id"]);
    }
    assert!(exact.iter().all(|v| v["wire_dispatch_index"] == 1));
    assert_eq!(rows[1]["attribution_status"], "unsupported_shape");
}

/// Repeated members replace the whole diagnostic snapshot. A later malformed
/// stream must retain the last observed snapshot, not an earlier merged record.
#[test]
fn latest_usage_snapshot_survives_later_failure_without_merging() {
    let events = concat!(
        "data: {\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2,\"prompt_cache_hit_tokens\":99,\"prompt_cache_miss_tokens\":7}}\n\n",
        "data: {\"usage\":{\"completion_tokens\":3,\"prompt_cache_miss_tokens\":88}}\n\n",
        "data: not-json\n\n",
    ).to_owned();
    let (outcome, rows, exact) = run(
        events,
        CacheUsageCompat::DeepSeek,
        CacheDiagnostics::Metadata,
        true,
        tau_proto::PromptOperation::Inference,
    );
    assert!(!matches!(outcome, AttemptOutcome::Completed(_)));
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["outcome"], "error");
    assert_eq!(rows[1]["dispatch_count"], 1);
    assert!(rows[1]["successful_dispatch_index"].is_null());
    assert_eq!(rows[1]["reported_usage"]["output_tokens"], 3);
    assert_eq!(rows[1]["reported_usage"]["miss_tokens"], 88);
    assert!(rows[1]["reported_usage"]["input_tokens"].is_null());
    assert!(rows[1]["reported_usage"]["read_tokens"].is_null());
    assert_eq!(
        exact.len(),
        1,
        "existing stream failures have no exact failure capture"
    );
}

/// An explicit null usage member replaces prior evidence; an absent member does
/// not. Missing counters must not become a stale snapshot or fabricated zero.
#[test]
fn null_usage_replaces_snapshot_but_absent_usage_does_not() {
    for (usage, expected) in [
        (vec![json!({"prompt_tokens": 10})], json!(10)),
        (vec![json!({"prompt_tokens": 10}), Value::Null], Value::Null),
    ] {
        let (_, rows, _) = run(
            success_events(&usage),
            CacheUsageCompat::OpenAi,
            CacheDiagnostics::Metadata,
            true,
            tau_proto::PromptOperation::Inference,
        );
        assert_eq!(rows[1]["reported_usage"]["input_tokens"], expected);
    }
}

/// Metadata opt-out leaves exact capture untouched, memory-only activity stays
/// excluded, and local summaries use the same backend-scoped correlation.
#[test]
fn independent_policy_preserves_exact_capture_and_operation_eligibility() {
    for (policy, persistable, operation, scalar_count, exact_count) in [
        (
            CacheDiagnostics::Off,
            true,
            tau_proto::PromptOperation::Inference,
            0,
            2,
        ),
        (
            CacheDiagnostics::Metadata,
            false,
            tau_proto::PromptOperation::Inference,
            0,
            0,
        ),
        (
            CacheDiagnostics::Off,
            true,
            tau_proto::PromptOperation::StandaloneCompaction,
            0,
            2,
        ),
        (
            CacheDiagnostics::Metadata,
            true,
            tau_proto::PromptOperation::StandaloneCompaction,
            2,
            2,
        ),
    ] {
        let (outcome, rows, exact) = run(
            success_events(&[]),
            CacheUsageCompat::None,
            policy,
            persistable,
            operation,
        );
        assert!(matches!(outcome, AttemptOutcome::Completed(_)));
        assert_eq!(rows.len(), scalar_count);
        assert_eq!(exact.len(), exact_count);
        if policy == CacheDiagnostics::Off {
            assert!(exact[0]["attempt_id"].is_string());
            assert_eq!(exact[0]["attempt_id"], exact[1]["attempt_id"]);
        }
        if operation == tau_proto::PromptOperation::StandaloneCompaction {
            let attempt_id = exact[0]["attempt_id"].clone();
            assert!(attempt_id.is_string());
            assert!(exact.iter().all(|v| v["attempt_id"] == attempt_id));
            assert!(exact.iter().all(|v| v["wire_dispatch_index"] == 1));
            if policy == CacheDiagnostics::Metadata {
                assert!(
                    rows.iter()
                        .all(|v| v["operation"] == "standalone_compaction")
                );
                assert!(rows.iter().all(|v| v["logical_attempt"] == 7));
                assert!(rows.iter().all(|v| v["harness_provider_attempt"] == 7));
                assert!(rows.iter().all(|v| v["attempt_id"] == attempt_id));
            }
        }
    }
}

/// Cancellation at the final pre-send check must record zero actual dispatches
/// and preserve the existing absence of any exact request capture.
#[test]
fn final_pre_dispatch_cancellation_has_no_candidate_index_or_capture() {
    let configured = provider();
    let checks = Cell::new(0);
    let ((outcome, rows), exact) = exact(|| {
        collect(|| {
            run_attempt_with_diagnostics(
                tau_proto::ProviderAttempt::ONE,
                &prompt(),
                &resolved_provider(&configured),
                &configured.models[0],
                true,
                CacheDiagnostics::Metadata,
                &mut |_| {},
                &mut || {
                    checks.set(checks.get() + 1);
                    checks.get() == 2
                },
                &tau_provider::OutboundNetworkPolicy::from_environment(BTreeMap::new(), None),
            )
        })
    });
    assert!(matches!(outcome, AttemptOutcome::Canceled { .. }));
    assert_eq!(checks.get(), 2);
    assert!(exact.is_empty());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["record_kind"], "attempt_end");
    assert_eq!(rows[0]["dispatch_count"], 0);
    assert!(rows[0]["successful_dispatch_index"].is_null());
    assert_eq!(rows[0]["outcome"], "canceled");
}

/// Local lowering failures never acquire an actual dispatch or exact capture.
#[test]
fn local_failure_has_only_pre_dispatch_end() {
    let mut configured = provider();
    configured
        .extra_body
        .insert("model".into(), json!("collision"));
    let ((outcome, rows), exact) = exact(|| {
        collect(|| {
            run_attempt_with_diagnostics(
                tau_proto::ProviderAttempt::ONE,
                &prompt(),
                &resolved_provider(&configured),
                &configured.models[0],
                true,
                CacheDiagnostics::Metadata,
                &mut |_| {},
                &mut || false,
                &tau_provider::OutboundNetworkPolicy::from_environment(BTreeMap::new(), None),
            )
        })
    });
    assert!(matches!(outcome, AttemptOutcome::Terminal(_)));
    assert!(exact.is_empty());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["dispatch_count"], 0);
    assert_eq!(rows[0]["outcome"], "pre_dispatch_failure");
    assert!(rows[0]["successful_dispatch_index"].is_null());
}

/// Existing HTTP-error captures retain their timing and share the real send's
/// correlation; diagnostic records never copy error bodies.
#[test]
fn http_failure_correlates_existing_exact_capture_without_copying_prose() {
    let server = ScriptedTcpServer::spawn(|mut socket| {
        let mut request = [0_u8; 4096];
        let _ = path_std_io::Read::read(&mut socket, &mut request).expect("request");
        socket.write_all(b"HTTP/1.1 429 Too Many Requests\r\ncontent-length: 13\r\nconnection: close\r\n\r\nsecret-canary").expect("response");
    });
    let mut configured = provider();
    configured.base_url = format!("http://{}/v1", server.address());
    let ((outcome, rows), exact) = exact(|| {
        collect(|| {
            run_attempt_with_diagnostics(
                tau_proto::ProviderAttempt::ONE,
                &prompt(),
                &resolved_provider(&configured),
                &configured.models[0],
                true,
                CacheDiagnostics::Metadata,
                &mut |_| {},
                &mut || false,
                &tau_provider::OutboundNetworkPolicy::from_environment(BTreeMap::new(), None),
            )
        })
    });
    server.finish();
    assert!(matches!(outcome, AttemptOutcome::Retryable { .. }));
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["retry_class"], "throttle");
    assert_eq!(exact.len(), 2);
    assert_eq!(exact[1]["http_status"], 429);
    assert_eq!(exact[1]["attempt_id"], rows[0]["attempt_id"]);
    assert_eq!(exact[1]["wire_dispatch_index"], 1);
    assert!(
        !serde_json::to_string(&rows)
            .expect("rows")
            .contains("secret-canary")
    );
}

/// Raw-event retention overflow must omit only the existing oversized exact
/// response, never scalar counters or semantic completion.
#[test]
fn raw_event_ineligibility_does_not_disable_scalar_usage() {
    let mut events = over_bound_sse_events();
    events.push_str(&success_events(&[json!({
        "prompt_tokens": 10, "prompt_tokens_details": {"cached_tokens": 99},
    })]));
    let (outcome, rows, exact) = run(
        events,
        CacheUsageCompat::OpenAi,
        CacheDiagnostics::Metadata,
        true,
        tau_proto::PromptOperation::Inference,
    );
    assert!(matches!(outcome, AttemptOutcome::Completed(_)));
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["reported_usage"]["read_tokens"], 99);
    assert_eq!(exact.len(), 1);
    assert!(exact[0].get("body").is_some());
}

/// Cancellation after semantic acceptance keeps the last raw usage snapshot and
/// sticky progress, without fabricating a successful dispatch or failure
/// capture.
#[test]
fn canceled_stream_retains_usage_and_semantic_progress() {
    let events = concat!(
        "data: {\"usage\":{\"prompt_tokens\":10,\"prompt_cache_hit_tokens\":99},",
        "\"choices\":[{\"delta\":{\"content\":\"kept\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    )
    .to_owned();
    let server = spawn_sse_server(events);
    let mut configured = provider();
    configured.base_url = format!("http://{}/v1", server.address());
    configured.compat.cache_usage = CacheUsageCompat::DeepSeek;
    let cancel = Cell::new(false);
    let ((outcome, rows), exact) = exact(|| {
        collect(|| {
            run_attempt_with_diagnostics(
                tau_proto::ProviderAttempt::ONE,
                &prompt(),
                &resolved_provider(&configured),
                &configured.models[0],
                true,
                CacheDiagnostics::Metadata,
                &mut |update| {
                    if let AttemptUpdate::Progress(progress) = update
                        && progress.semantic_progress() == SemanticProgress::Parsed
                    {
                        cancel.set(true);
                    }
                },
                &mut || cancel.get(),
                &tau_provider::OutboundNetworkPolicy::from_environment(BTreeMap::new(), None),
            )
        })
    });
    server.finish();
    assert!(matches!(outcome, AttemptOutcome::Canceled { .. }));
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["outcome"], "canceled");
    assert_eq!(rows[1]["semantic_progress"], true);
    assert_eq!(rows[1]["reported_usage"]["read_tokens"], 99);
    assert!(rows[1]["successful_dispatch_index"].is_null());
    assert_eq!(exact.len(), 1);
}
