use super::*;
use crate::event_log as path_crate_event_log;

fn quota_model() -> tau_proto::ProviderModelInfo {
    tau_proto::ProviderModelInfo {
        id: "chatgpt/gpt-5.6-sol".into(),
        display_name: None,
        tags: Vec::new(),
        supported_tool_types: Vec::new(),
        input_modalities: Vec::new(),
        tool_result_modalities: Vec::new(),
        supports_parallel_tool_calls: true,
        default_affinity: 0,
        context_window: tau_proto::TokenCount::new(100_000),
        efforts: vec![tau_proto::Effort::Off],
        verbosities: vec![tau_proto::Verbosity::Low],
        thinking_summaries: vec![tau_proto::ThinkingSummary::Off],
        supports_compaction: false,
        supports_standalone_compaction: false,
        standalone_compaction_generation_negative: false,
        standalone_compaction_threshold: None,
        standalone_compaction_prefix_budget: None,
        cache_policy: None,
        est_uncached_input_cost_1m_usd: Default::default(),
        est_cached_input_cost_1m_usd: Default::default(),
        est_cache_write_input_cost_1m_usd: Default::default(),
        est_output_cost_1m_usd: Default::default(),
        est_cache_storage_cost_1m_token_hour_usd: None,
    }
}

fn quota_window(used_basis_points: u16) -> tau_proto::ProviderQuotaWindow {
    quota_window_for("codex", used_basis_points)
}

fn quota_window_for(limit_id: &str, used_basis_points: u16) -> tau_proto::ProviderQuotaWindow {
    tau_proto::ProviderQuotaWindow {
        key: tau_proto::ProviderQuotaWindowKey {
            limit_id: tau_proto::ProviderQuotaLimitId::parse(limit_id).expect("quota pool"),
            window_id: tau_proto::ProviderQuotaWindowId::parse("secondary").expect("window id"),
        },
        used_basis_points,
        usage_observed_at_unix_ms: tau_proto::UnixMillis::new(123_000),
        window_seconds: tau_proto::QuotaWindowSeconds::new(604_800),
        reset_at_unix_seconds: Some(tau_proto::UnixSeconds::new(700_000)),
        remaining_seconds_at_timing_anchor: Some(tau_proto::SignedSeconds::new(300_000)),
        timing_anchor_observed_at_unix_ms: Some(tau_proto::UnixMillis::new(123_000)),
        server_offset_ms: Some(tau_proto::ServerOffsetMillis::new(0)),
        server_offset_observed_at_unix_ms: Some(tau_proto::UnixMillis::new(123_000)),
    }
}

fn quota_binding() -> tau_proto::ProviderQuotaRouteBinding {
    tau_proto::ProviderQuotaRouteBinding {
        model: "chatgpt/gpt-5.6-sol".into(),
        limit_ids: vec![tau_proto::ProviderQuotaLimitId::parse("codex").expect("quota pool")],
        observed_at_unix_ms: tau_proto::UnixMillis::new(123_000),
        provenance: tau_proto::ProviderQuotaBindingProvenance::TurnEvent,
    }
}

fn quota_replace_report(epoch: &str, used_basis_points: u16) -> Event {
    Event::ProviderQuotaReplaceReported(tau_proto::ProviderQuotaReplace {
        provider: tau_proto::ProviderName::new("chatgpt"),
        profile_epoch: tau_proto::ProviderQuotaEpoch::parse(epoch).expect("epoch"),
        sequence: tau_proto::ProviderQuotaSequence::new(1),
        establishes_new_epoch: true,
        windows: vec![quota_window(used_basis_points)],
        route_bindings: vec![quota_binding()],
    })
}

fn committed_quota_events(harness: &Harness) -> Vec<(Option<tau_proto::ConnectionId>, Event)> {
    let mut events = Vec::new();
    let mut seq = path_crate_event_log::EventLogSeq::new(0);
    while let Some(entry) = harness.runtime_io.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if matches!(
            entry.event,
            Event::ProviderQuotaReplaceReported(_)
                | Event::ProviderQuotaPatchReported(_)
                | Event::ProviderQuotaClearReported(_)
                | Event::HarnessProviderQuotaChanged(_)
        ) {
            events.push((entry.source, entry.event));
        }
    }
    events
}

/// A configured Provider's report commits before downstream validation and the
/// separate harness-sourced canonical current-state snapshot.
#[test]
fn provider_quota_report_commits_before_canonical_snapshot() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    let debug_path = harness
        .enable_debug_log(&temp.path().join("debug"))
        .expect("enable debug log");
    connect_ready_configured_extension(
        &mut harness,
        "quota-provider",
        "quota-provider",
        tau_proto::ClientKind::Provider,
    );
    harness.set_provider_models(
        &crate::test_connection_id("quota-provider"),
        vec![quota_model()],
    );

    harness
        .handle_extension_event_inner_with_persist(
            &crate::test_connection_id("quota-provider"),
            quota_replace_report("epoch-commit-order", 1_000),
            Some(false),
        )
        .expect("submit quota report");

    let events = committed_quota_events(&harness);
    assert!(matches!(
        events.as_slice(),
        [
            (Some(report_source), Event::ProviderQuotaReplaceReported(_)),
            (
                Some(canonical_source),
                Event::HarnessProviderQuotaChanged(_)
            )
        ] if report_source.as_str() == "quota-provider"
            && canonical_source.as_str() == HARNESS_CONNECTION_ID
    ));
    assert_eq!(
        harness.provider_runtime.quota[&tau_proto::ProviderName::new("chatgpt")]
            .snapshot
            .windows[0]
            .used_basis_points,
        1_000
    );
    let debug_lines = std::fs::read_to_string(debug_path).expect("read debug log");
    let quota_debug = debug_lines
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("debug JSON"))
        .filter(|line| {
            matches!(
                line["event_name"].as_str(),
                Some("provider.quota_replace_reported" | "harness.provider_quota_changed")
            )
        })
        .map(|line| {
            (
                line["source"].as_str().map(str::to_owned),
                line["event_name"]
                    .as_str()
                    .expect("quota debug event name")
                    .to_owned(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        quota_debug,
        [
            (
                Some("quota-provider".to_owned()),
                "provider.quota_replace_reported".to_owned(),
            ),
            (
                Some(HARNESS_CONNECTION_ID.to_owned()),
                "harness.provider_quota_changed".to_owned(),
            ),
        ]
    );
}

/// A quota report deferred behind another intercepted publication updates
/// process-global provider/account state across rollover for the same live
/// provider generation.
#[test]
fn rollover_applies_deferred_provider_quota_for_current_generation() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "quota-provider",
        "quota-provider",
        tau_proto::ClientKind::Provider,
    );
    harness.set_provider_models(
        &crate::test_connection_id("quota-provider"),
        vec![quota_model()],
    );
    connect_test_tool(&mut harness, "rollover-blocker");
    harness
        .handle_extension_event(
            "rollover-blocker",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register rollover blocker");
    harness.publish_event(None, draft_event("block provider quota"));
    harness
        .handle_extension_event_inner_with_persist(
            &crate::test_connection_id("quota-provider"),
            quota_replace_report("epoch-rollover", 2_500),
            Some(false),
        )
        .expect("defer quota report");
    assert!(harness.provider_runtime.quota.is_empty());

    harness
        .switch_session(
            "replacement"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            tau_proto::SessionStartReason::New,
        )
        .expect("switch session");

    assert_eq!(
        harness.provider_runtime.quota[&tau_proto::ProviderName::new("chatgpt")]
            .snapshot
            .windows[0]
            .used_basis_points,
        2_500
    );
    assert!(matches!(
        committed_quota_events(&harness).as_slice(),
        [
            (Some(report_source), Event::ProviderQuotaReplaceReported(_)),
            (
                Some(canonical_source),
                Event::HarnessProviderQuotaChanged(_)
            ),
        ] if report_source == "quota-provider"
            && canonical_source == HARNESS_CONNECTION_ID
    ));
}

/// Replace, patch, and clear all traverse generic report commit before their
/// respective downstream canonical current-state transitions.
#[test]
fn provider_quota_report_family_drives_state_end_to_end() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "quota-provider",
        "quota-provider",
        tau_proto::ClientKind::Provider,
    );
    harness.set_provider_models(
        &crate::test_connection_id("quota-provider"),
        vec![quota_model()],
    );
    let epoch = tau_proto::ProviderQuotaEpoch::parse("epoch-family").expect("epoch");
    let reports = [
        Event::ProviderQuotaReplaceReported(tau_proto::ProviderQuotaReplace {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: epoch.clone(),
            sequence: tau_proto::ProviderQuotaSequence::new(1),
            establishes_new_epoch: true,
            windows: vec![quota_window(1_000)],
            route_bindings: vec![quota_binding()],
        }),
        Event::ProviderQuotaPatchReported(tau_proto::ProviderQuotaPatch {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: epoch.clone(),
            sequence: tau_proto::ProviderQuotaSequence::new(2),
            windows: vec![quota_window(2_000)],
            removed_window_keys: Vec::new(),
            route_bindings: vec![quota_binding()],
        }),
        Event::ProviderQuotaClearReported(tau_proto::ProviderQuotaClear {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: epoch,
            sequence: tau_proto::ProviderQuotaSequence::new(3),
        }),
    ];
    for report in reports {
        harness
            .handle_extension_event_inner_with_persist(
                &crate::test_connection_id("quota-provider"),
                report,
                Some(false),
            )
            .expect("submit quota report");
    }

    assert!(harness.provider_runtime.quota.is_empty());
    let events = committed_quota_events(&harness);
    assert_eq!(events.len(), 6);
    for pair in events.as_chunks::<2>().0 {
        assert!(matches!(
            pair,
            [
                (Some(report_source), report),
                (
                    Some(canonical_source),
                    Event::HarnessProviderQuotaChanged(_)
                )
            ] if report_source.as_str() == "quota-provider"
                && canonical_source.as_str() == HARNESS_CONNECTION_ID
                && matches!(
                    report,
                    Event::ProviderQuotaReplaceReported(_)
                        | Event::ProviderQuotaPatchReported(_)
                        | Event::ProviderQuotaClearReported(_)
                )
        ));
    }
}

/// Payload ownership is downstream domain validation: a configured Provider's
/// unowned report remains committed but cannot mutate state or derive a fact.
#[test]
fn provider_quota_unowned_report_commits_without_canonical_state() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "quota-provider",
        "quota-provider",
        tau_proto::ClientKind::Provider,
    );

    harness
        .handle_extension_event_inner_with_persist(
            &crate::test_connection_id("quota-provider"),
            quota_replace_report("epoch-unowned", 1_000),
            Some(true),
        )
        .expect("submit unowned quota report");

    assert!(harness.provider_runtime.quota.is_empty());
    assert!(matches!(
        committed_quota_events(&harness).as_slice(),
        [(Some(source), Event::ProviderQuotaReplaceReported(_))]
            if source.as_str() == "quota-provider"
    ));
}

/// Even a quota `Emit` with `persist=true` remains runtime-only: cold resume
/// restores neither raw reports nor canonical quota current state.
#[test]
fn provider_quota_reports_and_state_do_not_cold_replay() {
    let temp = TempDir::new().expect("temp dir");
    let state = temp.path().join("state");
    {
        let mut harness = quiet_provider_harness(&state).expect("harness");
        connect_ready_configured_extension(
            &mut harness,
            "quota-provider",
            "quota-provider",
            tau_proto::ClientKind::Provider,
        );
        harness.set_provider_models(
            &crate::test_connection_id("quota-provider"),
            vec![quota_model()],
        );
        harness
            .handle_extension_event_inner_with_persist(
                &crate::test_connection_id("quota-provider"),
                quota_replace_report("epoch-no-replay", 1_000),
                Some(true),
            )
            .expect("submit persist=true quota report");
        assert!(!harness.provider_runtime.quota.is_empty());
        harness.shutdown().expect("shutdown harness");
    }

    let resumed =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("resume harness");
    assert!(resumed.provider_runtime.quota.is_empty());
    assert!(
        event_log_events(&resumed)
            .into_iter()
            .all(|event| !matches!(
                event,
                Event::ProviderQuotaReplaceReported(_)
                    | Event::ProviderQuotaPatchReported(_)
                    | Event::ProviderQuotaClearReported(_)
                    | Event::HarnessProviderQuotaChanged(_)
            ))
    );
}

/// Quota report names are default-deny authority: configured wrong-kind,
/// unconfigured kind-claiming, UI, and external socket peers cannot commit.
#[test]
fn provider_quota_report_rejects_non_provider_authority() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    for (source, kind) in [
        ("tool-peer", tau_proto::ClientKind::Tool),
        ("core-peer", tau_proto::ClientKind::Core),
        ("action-peer", tau_proto::ClientKind::Action),
    ] {
        connect_ready_configured_extension(&mut harness, source, source, kind);
        harness
            .handle_extension_event_inner_with_persist(
                &crate::test_connection_id(source),
                quota_replace_report("epoch-forbidden", 1_000),
                Some(false),
            )
            .expect("reject configured wrong-kind quota report");
    }
    connect_test_client(
        &mut harness,
        "unconfigured-provider",
        tau_proto::ClientKind::Provider,
    );
    harness
        .handle_extension_event_inner_with_persist(
            &crate::test_connection_id("unconfigured-provider"),
            quota_replace_report("epoch-forbidden", 1_000),
            Some(false),
        )
        .expect("reject unconfigured provider claim");
    for (source, kind) in [
        ("ui-peer", tau_proto::ClientKind::Ui),
        ("external-peer", tau_proto::ClientKind::External),
    ] {
        connect_test_client(&mut harness, source, kind);
        harness
            .handle_client_event_inner_with_persist(
                &crate::test_connection_id(source),
                quota_replace_report("epoch-forbidden", 1_000),
                Some(false),
            )
            .expect("reject client-path quota report");
    }

    assert!(committed_quota_events(&harness).is_empty());
    assert!(harness.provider_runtime.quota.is_empty());
}

/// Exact interception may replace a mutable report; only the committed
/// replacement drives downstream quota validation and state.
#[test]
fn provider_quota_report_replacement_drives_canonical_state() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "quota-provider",
        "quota-provider",
        tau_proto::ClientKind::Provider,
    );
    harness.set_provider_models(
        &crate::test_connection_id("quota-provider"),
        vec![quota_model()],
    );
    connect_test_tool(&mut harness, "interceptor");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::PROVIDER_QUOTA_REPLACE_REPORTED,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register interceptor");

    harness
        .handle_extension_event_inner_with_persist(
            &crate::test_connection_id("quota-provider"),
            quota_replace_report("epoch-replaced", 1_000),
            Some(false),
        )
        .expect("submit quota report");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(Some(Box::new(quota_replace_report(
                    "epoch-replaced",
                    4_000,
                )))),
            })),
        )
        .expect("replace quota report");

    assert_eq!(
        harness.provider_runtime.quota[&tau_proto::ProviderName::new("chatgpt")]
            .snapshot
            .windows[0]
            .used_basis_points,
        4_000
    );

    harness
        .handle_extension_event_inner_with_persist(
            &crate::test_connection_id("quota-provider"),
            quota_replace_report("epoch-invalid-replacement", 5_000),
            Some(false),
        )
        .expect("submit second quota report");
    let mut invalid_replacement = quota_replace_report("epoch-invalid-replacement", 6_000);
    let Event::ProviderQuotaReplaceReported(replace) = &mut invalid_replacement else {
        unreachable!("quota helper returns replacement report");
    };
    replace.provider = tau_proto::ProviderName::new("unowned");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(Some(Box::new(invalid_replacement))),
            })),
        )
        .expect("replace with invalid quota report");

    assert_eq!(
        harness.provider_runtime.quota[&tau_proto::ProviderName::new("chatgpt")]
            .snapshot
            .windows[0]
            .used_basis_points,
        4_000,
        "the committed replacement must be revalidated downstream"
    );
    assert_eq!(
        committed_quota_events(&harness)
            .iter()
            .filter(|(_, event)| matches!(event, Event::HarnessProviderQuotaChanged(_)))
            .count(),
        1
    );
}

/// Dropping an intercepted quota report prevents both its commit and every
/// downstream current-state effect.
#[test]
fn dropping_provider_quota_report_prevents_canonical_state() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "quota-provider",
        "quota-provider",
        tau_proto::ClientKind::Provider,
    );
    harness.set_provider_models(
        &crate::test_connection_id("quota-provider"),
        vec![quota_model()],
    );
    connect_test_tool(&mut harness, "interceptor");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::PROVIDER_QUOTA_REPLACE_REPORTED,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register interceptor");

    harness
        .handle_extension_event_inner_with_persist(
            &crate::test_connection_id("quota-provider"),
            quota_replace_report("epoch-dropped", 1_000),
            Some(false),
        )
        .expect("submit quota report");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Drop,
            })),
        )
        .expect("drop quota report");

    assert!(committed_quota_events(&harness).is_empty());
    assert!(harness.provider_runtime.quota.is_empty());
}

/// A report parked before same-source provider replacement still commits with
/// its captured source, but captured-generation revalidation blocks mutation.
#[test]
fn parked_stale_provider_quota_report_cannot_mutate_state() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "quota-provider",
        "quota-provider",
        tau_proto::ClientKind::Provider,
    );
    harness.set_provider_models(
        &crate::test_connection_id("quota-provider"),
        vec![quota_model()],
    );
    connect_test_tool(&mut harness, "interceptor");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::PROVIDER_QUOTA_REPLACE_REPORTED,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register interceptor");
    harness
        .handle_extension_event_inner_with_persist(
            &crate::test_connection_id("quota-provider"),
            quota_replace_report("epoch-stale", 1_000),
            Some(false),
        )
        .expect("submit quota report");

    harness.handle_disconnect(&crate::test_connection_id("quota-provider"));
    connect_ready_configured_extension(
        &mut harness,
        "quota-provider",
        "quota-provider",
        tau_proto::ClientKind::Provider,
    );
    harness
        .extensions
        .entries
        .get_mut("quota-provider")
        .expect("replacement provider")
        .instance_id = 43.into();
    harness.set_provider_models(
        &crate::test_connection_id("quota-provider"),
        vec![quota_model()],
    );
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("pass stale report");

    assert!(harness.provider_runtime.quota.is_empty());
    assert!(matches!(
        committed_quota_events(&harness).as_slice(),
        [(Some(source), Event::ProviderQuotaReplaceReported(_))]
            if source.as_str() == "quota-provider"
    ));
}

/// Replace/Patch enforce source ownership and strict upstream sequencing while
/// applying complete records by stable key.
#[test]
fn provider_quota_replace_patch_and_spoofing_are_validated() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    harness.set_provider_models(&crate::test_connection_id("owner"), vec![quota_model()]);
    let epoch = tau_proto::ProviderQuotaEpoch::parse("epoch-1").expect("epoch");
    harness.handle_provider_quota_replace(
        &crate::test_connection_id("spoof"),
        tau_proto::ProviderQuotaReplace {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: epoch.clone(),
            sequence: tau_proto::ProviderQuotaSequence::new(1),
            establishes_new_epoch: true,
            windows: vec![quota_window(1_000)],
            route_bindings: vec![quota_binding()],
        },
    );
    assert!(harness.provider_runtime.quota.is_empty());
    harness.handle_provider_quota_replace(
        &crate::test_connection_id("owner"),
        tau_proto::ProviderQuotaReplace {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: epoch.clone(),
            sequence: tau_proto::ProviderQuotaSequence::new(1),
            establishes_new_epoch: true,
            windows: vec![quota_window(1_000)],
            route_bindings: vec![quota_binding()],
        },
    );
    assert_eq!(
        harness.provider_runtime.quota[&tau_proto::ProviderName::new("chatgpt")]
            .snapshot
            .sequence,
        tau_proto::ProviderQuotaSequence::new(1)
    );
    harness.handle_provider_quota_patch(
        &crate::test_connection_id("owner"),
        tau_proto::ProviderQuotaPatch {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: epoch,
            sequence: tau_proto::ProviderQuotaSequence::new(2),
            windows: vec![quota_window(2_000)],
            removed_window_keys: Vec::new(),
            route_bindings: Vec::new(),
        },
    );
    let snapshot =
        &harness.provider_runtime.quota[&tau_proto::ProviderName::new("chatgpt")].snapshot;
    assert_eq!(snapshot.sequence, tau_proto::ProviderQuotaSequence::new(2));
    assert_eq!(snapshot.windows[0].used_basis_points, 2_000);
}

/// A full two-pool account snapshot followed by the exact-model default-pool
/// turn binding projects both pool facts while preserving only `codex` as the
/// applicable route, matching the provider extension's real event sequence.
#[test]
fn provider_quota_two_pool_default_binding_projects_without_ambiguity() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    harness.set_provider_models(&crate::test_connection_id("owner"), vec![quota_model()]);
    let epoch = tau_proto::ProviderQuotaEpoch::parse("epoch-1").expect("epoch");
    harness.handle_provider_quota_replace(
        &crate::test_connection_id("owner"),
        tau_proto::ProviderQuotaReplace {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: epoch.clone(),
            sequence: tau_proto::ProviderQuotaSequence::new(1),
            establishes_new_epoch: true,
            windows: vec![
                quota_window_for("codex", 4_400),
                quota_window_for("codex_bengalfox", 0),
            ],
            route_bindings: Vec::new(),
        },
    );
    harness.handle_provider_quota_patch(
        &crate::test_connection_id("owner"),
        tau_proto::ProviderQuotaPatch {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: epoch,
            sequence: tau_proto::ProviderQuotaSequence::new(2),
            windows: Vec::new(),
            removed_window_keys: Vec::new(),
            route_bindings: vec![quota_binding()],
        },
    );

    let snapshot =
        &harness.provider_runtime.quota[&tau_proto::ProviderName::new("chatgpt")].snapshot;
    assert_eq!(snapshot.windows.len(), 2);
    assert_eq!(snapshot.route_bindings.len(), 1);
    assert_eq!(snapshot.route_bindings[0].model, quota_binding().model);
    assert_eq!(
        snapshot.route_bindings[0]
            .limit_ids
            .iter()
            .map(tau_proto::ProviderQuotaLimitId::as_str)
            .collect::<Vec<_>>(),
        vec!["codex"]
    );
}

/// When duplicate publishers advertise one model, only the effective
/// deterministic route winner may publish quota for that exact binding.
#[test]
fn duplicate_provider_namespace_cannot_spoof_effective_route() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    harness.set_provider_models(&crate::test_connection_id("a-owner"), vec![quota_model()]);
    harness.set_provider_models(&crate::test_connection_id("z-winner"), vec![quota_model()]);
    let replace = |sequence| tau_proto::ProviderQuotaReplace {
        provider: tau_proto::ProviderName::new("chatgpt"),
        profile_epoch: tau_proto::ProviderQuotaEpoch::parse("epoch-1").expect("epoch"),
        sequence,
        establishes_new_epoch: true,
        windows: vec![quota_window(1_000)],
        route_bindings: vec![quota_binding()],
    };
    harness.handle_provider_quota_replace(
        &crate::test_connection_id("a-owner"),
        replace(tau_proto::ProviderQuotaSequence::new(1)),
    );
    assert!(harness.provider_runtime.quota.is_empty());
    harness.handle_provider_quota_replace(
        &crate::test_connection_id("z-winner"),
        replace(tau_proto::ProviderQuotaSequence::new(1)),
    );
    assert_eq!(
        harness.provider_runtime.quota[&tau_proto::ProviderName::new("chatgpt")]
            .source_id
            .as_str(),
        "z-winner"
    );
}

/// Distinct effective routes split across two sources make the whole account
/// namespace ambiguous and therefore suppress all quota authority.
#[test]
fn split_namespace_ownership_fails_closed() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    let mut second = quota_model();
    second.id = "chatgpt/gpt-other".into();
    harness.set_provider_models(&crate::test_connection_id("owner-a"), vec![quota_model()]);
    harness.set_provider_models(&crate::test_connection_id("owner-b"), vec![second]);
    harness.handle_provider_quota_replace(
        &crate::test_connection_id("owner-a"),
        tau_proto::ProviderQuotaReplace {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: tau_proto::ProviderQuotaEpoch::parse("epoch-1").expect("epoch"),
            sequence: tau_proto::ProviderQuotaSequence::new(1),
            establishes_new_epoch: true,
            windows: vec![quota_window(1_000)],
            route_bindings: vec![quota_binding()],
        },
    );
    assert!(harness.provider_runtime.quota.is_empty());
}

/// Harness-local model metadata invalidation does not consume provider sequence
/// space, so the provider's immediately following patch remains acceptable.
#[test]
fn model_change_preserves_provider_sequence_space() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    harness
        .apply_provider_models_snapshot(&crate::test_connection_id("owner"), vec![quota_model()]);
    let epoch = tau_proto::ProviderQuotaEpoch::parse("epoch-1").expect("epoch");
    harness.handle_provider_quota_replace(
        &crate::test_connection_id("owner"),
        tau_proto::ProviderQuotaReplace {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: epoch.clone(),
            sequence: tau_proto::ProviderQuotaSequence::new(1),
            establishes_new_epoch: true,
            windows: vec![quota_window(1_000)],
            route_bindings: vec![quota_binding()],
        },
    );
    let mut changed = quota_model();
    changed.context_window = tau_proto::TokenCount::new(200_000);
    harness.apply_provider_models_snapshot(&crate::test_connection_id("owner"), vec![changed]);
    assert_eq!(
        harness.provider_runtime.quota[&tau_proto::ProviderName::new("chatgpt")]
            .snapshot
            .sequence,
        tau_proto::ProviderQuotaSequence::new(1)
    );
    harness.handle_provider_quota_patch(
        &crate::test_connection_id("owner"),
        tau_proto::ProviderQuotaPatch {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: epoch,
            sequence: tau_proto::ProviderQuotaSequence::new(2),
            windows: vec![quota_window(2_000)],
            removed_window_keys: Vec::new(),
            route_bindings: Vec::new(),
        },
    );
    assert_eq!(
        harness.provider_runtime.quota[&tau_proto::ProviderName::new("chatgpt")]
            .snapshot
            .sequence,
        tau_proto::ProviderQuotaSequence::new(2)
    );
}

/// Withdrawing the last effective model clears sensitive account state while
/// retaining an empty running-harness capability for late subscribers; a later
/// accepted replacement restores current state and supersedes that capability.
#[test]
fn quota_catch_up_preserves_clocks_and_model_withdrawal_clears() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    harness
        .apply_provider_models_snapshot(&crate::test_connection_id("owner"), vec![quota_model()]);
    harness.handle_provider_quota_replace(
        &crate::test_connection_id("owner"),
        tau_proto::ProviderQuotaReplace {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: tau_proto::ProviderQuotaEpoch::parse("epoch-1").expect("epoch"),
            sequence: tau_proto::ProviderQuotaSequence::new(1),
            establishes_new_epoch: true,
            windows: vec![quota_window(1_000)],
            route_bindings: vec![quota_binding()],
        },
    );
    let events = connect_test_client(&mut harness, "late-quota-ui", tau_proto::ClientKind::Ui);
    let selectors = vec![EventSelector::Exact(
        tau_proto::EventName::HARNESS_PROVIDER_QUOTA_CHANGED,
    )];
    harness
        .complete_subscription(
            &crate::test_connection_id("late-quota-ui"),
            selectors.clone(),
            selectors,
        )
        .expect("subscribe to quota current state");
    let routed_events = events.lock().expect("events");
    assert!(
        routed_events.iter().all(|routed| !matches!(
            &routed.frame,
            HarnessOutputMessage::Deliver(delivery)
                if matches!(
                    delivery.event.as_ref(),
                    Event::ProviderQuotaReplaceReported(_)
                        | Event::ProviderQuotaPatchReported(_)
                        | Event::ProviderQuotaClearReported(_)
                )
        )),
        "late subscribers must never receive raw quota reports"
    );
    let observed = routed_events.iter().find_map(|routed| match &routed.frame {
        HarnessOutputMessage::Deliver(delivery) => match delivery.event.as_ref() {
            Event::HarnessProviderQuotaChanged(changed) => Some((
                routed.source_id.clone(),
                changed.windows[0].usage_observed_at_unix_ms,
            )),
            _ => None,
        },
        _ => None,
    });
    assert_eq!(
        observed,
        Some((
            Some(crate::test_connection_id(HARNESS_CONNECTION_ID)),
            tau_proto::UnixMillis::new(123_000)
        ))
    );
    drop(routed_events);
    harness.apply_provider_models_snapshot(&crate::test_connection_id("owner"), Vec::new());
    assert!(harness.provider_runtime.quota.is_empty());
    let cleared_events = connect_test_client(
        &mut harness,
        "post-clear-quota-ui",
        tau_proto::ClientKind::Ui,
    );
    let selectors = vec![EventSelector::Exact(
        tau_proto::EventName::HARNESS_PROVIDER_QUOTA_CHANGED,
    )];
    harness
        .complete_subscription(
            &crate::test_connection_id("post-clear-quota-ui"),
            selectors.clone(),
            selectors,
        )
        .expect("subscribe after quota clear");
    let cleared = cleared_events
        .lock()
        .expect("events")
        .iter()
        .find_map(|routed| match &routed.frame {
            HarnessOutputMessage::Deliver(delivery) => match delivery.event.as_ref() {
                Event::HarnessProviderQuotaChanged(changed) => Some((
                    changed.provider.clone(),
                    changed.windows.len(),
                    changed.route_bindings.len(),
                )),
                _ => None,
            },
            _ => None,
        });
    assert_eq!(
        cleared,
        Some((tau_proto::ProviderName::new("chatgpt"), 0, 0)),
        "late subscribers must retain the same running-harness capability as live clients"
    );
    harness
        .apply_provider_models_snapshot(&crate::test_connection_id("owner"), vec![quota_model()]);
    harness.handle_provider_quota_replace(
        &crate::test_connection_id("owner"),
        tau_proto::ProviderQuotaReplace {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: tau_proto::ProviderQuotaEpoch::parse("epoch-1").expect("epoch"),
            sequence: tau_proto::ProviderQuotaSequence::new(2),
            establishes_new_epoch: false,
            windows: vec![quota_window(2_000)],
            route_bindings: vec![quota_binding()],
        },
    );
    assert_eq!(
        harness.provider_runtime.quota[&tau_proto::ProviderName::new("chatgpt")]
            .snapshot
            .sequence,
        tau_proto::ProviderQuotaSequence::new(2)
    );
    assert!(
        harness.provider_runtime.quota_capabilities.is_empty(),
        "new current state supersedes the cleared capability snapshot"
    );
}

/// Peers cannot author canonical quota state, while actual canonical
/// publication overrides interceptor replacement and Drop.
#[test]
fn validated_quota_projection_is_must_pass_and_immutable() {
    let original = Event::HarnessProviderQuotaChanged(tau_proto::HarnessProviderQuotaChanged {
        provider: tau_proto::ProviderName::new("chatgpt"),
        profile_epoch: tau_proto::ProviderQuotaEpoch::parse("epoch-1").expect("epoch"),
        sequence: tau_proto::ProviderQuotaSequence::new(1),
        windows: vec![quota_window(1_000)],
        route_bindings: vec![quota_binding()],
    });
    let replacement = Event::HarnessProviderQuotaChanged(tau_proto::HarnessProviderQuotaChanged {
        provider: tau_proto::ProviderName::new("chatgpt"),
        profile_epoch: tau_proto::ProviderQuotaEpoch::parse("epoch-1").expect("epoch"),
        sequence: tau_proto::ProviderQuotaSequence::new(1),
        windows: vec![quota_window(9_000)],
        route_bindings: vec![quota_binding()],
    });
    assert!(super::super::interception::event_must_pass_by_default(
        &tau_proto::EventName::HARNESS_PROVIDER_QUOTA_CHANGED
    ));
    assert!(
        super::super::interception::immutable_protected_fact_was_modified(&original, &replacement)
    );
    assert!(Harness::is_peer_forbidden_harness_fact(&original));

    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "quota-provider",
        "quota-provider",
        tau_proto::ClientKind::Provider,
    );
    harness
        .handle_extension_event_inner_with_persist(
            &crate::test_connection_id("quota-provider"),
            original.clone(),
            Some(false),
        )
        .expect("reject peer-authored canonical state");
    assert!(committed_quota_events(&harness).is_empty());

    connect_test_tool(&mut harness, "interceptor");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::HARNESS_PROVIDER_QUOTA_CHANGED,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register interceptor");

    harness.publish_event(
        Some(&crate::test_connection_id(HARNESS_CONNECTION_ID)),
        original.clone(),
    );
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(Some(Box::new(replacement))),
            })),
        )
        .expect("attempt canonical replacement");
    harness.publish_event(
        Some(&crate::test_connection_id(HARNESS_CONNECTION_ID)),
        original.clone(),
    );
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Drop,
            })),
        )
        .expect("attempt canonical drop");

    let canonical = committed_quota_events(&harness);
    assert_eq!(canonical.len(), 2);
    assert!(canonical.iter().all(|(source, event)| {
        source.as_deref() == Some(HARNESS_CONNECTION_ID) && event == &original
    }));
}

/// Explicit Clear retires its epoch at the provider's actual sequence and
/// prevents stale same-epoch or previously replaced epochs from resurrecting.
#[test]
fn provider_quota_clear_orders_and_retires_epochs() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    harness.set_provider_models(&crate::test_connection_id("owner"), vec![quota_model()]);
    let epoch_a = tau_proto::ProviderQuotaEpoch::parse("epoch-a").expect("epoch");
    harness.handle_provider_quota_replace(
        &crate::test_connection_id("owner"),
        tau_proto::ProviderQuotaReplace {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: epoch_a.clone(),
            sequence: tau_proto::ProviderQuotaSequence::new(5),
            establishes_new_epoch: true,
            windows: vec![quota_window(1_000)],
            route_bindings: vec![quota_binding()],
        },
    );
    harness.handle_provider_quota_clear(
        &crate::test_connection_id("owner"),
        tau_proto::ProviderQuotaClear {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: epoch_a.clone(),
            sequence: tau_proto::ProviderQuotaSequence::new(6),
        },
    );
    assert!(harness.provider_runtime.quota.is_empty());
    assert!(
        harness.provider_runtime.quota_retired_epochs[&tau_proto::ProviderName::new("chatgpt")]
            .contains(&epoch_a)
    );
    harness.handle_provider_quota_replace(
        &crate::test_connection_id("owner"),
        tau_proto::ProviderQuotaReplace {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: epoch_a,
            sequence: tau_proto::ProviderQuotaSequence::new(7),
            establishes_new_epoch: true,
            windows: vec![quota_window(2_000)],
            route_bindings: vec![quota_binding()],
        },
    );
    assert!(harness.provider_runtime.quota.is_empty());
    harness.handle_provider_quota_replace(
        &crate::test_connection_id("owner"),
        tau_proto::ProviderQuotaReplace {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: tau_proto::ProviderQuotaEpoch::parse("epoch-b").expect("epoch"),
            sequence: tau_proto::ProviderQuotaSequence::new(1),
            establishes_new_epoch: true,
            windows: vec![quota_window(2_000)],
            route_bindings: vec![quota_binding()],
        },
    );
    assert_eq!(
        harness.provider_runtime.quota[&tau_proto::ProviderName::new("chatgpt")]
            .snapshot
            .profile_epoch
            .as_str(),
        "epoch-b"
    );
}

/// A profile rotation while route authority is absent can recover from the
/// restored sole owner with its next authoritative full replacement.
#[test]
fn restored_owner_accepts_unretired_rotated_full_epoch() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    harness
        .apply_provider_models_snapshot(&crate::test_connection_id("owner"), vec![quota_model()]);
    harness.handle_provider_quota_replace(
        &crate::test_connection_id("owner"),
        tau_proto::ProviderQuotaReplace {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: tau_proto::ProviderQuotaEpoch::parse("epoch-e").expect("epoch"),
            sequence: tau_proto::ProviderQuotaSequence::new(1),
            establishes_new_epoch: true,
            windows: vec![quota_window(1_000)],
            route_bindings: vec![quota_binding()],
        },
    );
    harness.apply_provider_models_snapshot(&crate::test_connection_id("owner"), Vec::new());
    harness
        .apply_provider_models_snapshot(&crate::test_connection_id("owner"), vec![quota_model()]);
    harness.handle_provider_quota_replace(
        &crate::test_connection_id("owner"),
        tau_proto::ProviderQuotaReplace {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: tau_proto::ProviderQuotaEpoch::parse("epoch-f").expect("epoch"),
            sequence: tau_proto::ProviderQuotaSequence::new(2),
            establishes_new_epoch: false,
            windows: vec![quota_window(2_000)],
            route_bindings: vec![quota_binding()],
        },
    );
    assert_eq!(
        harness.provider_runtime.quota[&tau_proto::ProviderName::new("chatgpt")]
            .snapshot
            .profile_epoch
            .as_str(),
        "epoch-f"
    );
}

/// An explicit higher-sequence clear consumes a route-loss tombstone and
/// permanently retires that epoch instead of allowing later recovery.
#[test]
fn clear_consumes_matching_route_loss_tombstone() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    harness
        .apply_provider_models_snapshot(&crate::test_connection_id("owner"), vec![quota_model()]);
    let epoch = tau_proto::ProviderQuotaEpoch::parse("epoch-e").expect("epoch");
    harness.handle_provider_quota_replace(
        &crate::test_connection_id("owner"),
        tau_proto::ProviderQuotaReplace {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: epoch.clone(),
            sequence: tau_proto::ProviderQuotaSequence::new(1),
            establishes_new_epoch: true,
            windows: vec![quota_window(1_000)],
            route_bindings: vec![quota_binding()],
        },
    );
    harness.apply_provider_models_snapshot(&crate::test_connection_id("owner"), Vec::new());
    assert!(
        harness
            .provider_runtime
            .quota_tombstones
            .contains_key(&tau_proto::ProviderName::new("chatgpt"))
    );
    harness.handle_provider_quota_clear(
        &crate::test_connection_id("owner"),
        tau_proto::ProviderQuotaClear {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: epoch.clone(),
            sequence: tau_proto::ProviderQuotaSequence::new(2),
        },
    );
    assert!(
        !harness
            .provider_runtime
            .quota_tombstones
            .contains_key(&tau_proto::ProviderName::new("chatgpt"))
    );
    assert!(
        harness.provider_runtime.quota_retired_epochs[&tau_proto::ProviderName::new("chatgpt")]
            .contains(&epoch)
    );
}
