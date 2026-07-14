use super::*;

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
        context_window: 100_000,
        efforts: vec![tau_proto::Effort::Off],
        verbosities: vec![tau_proto::Verbosity::Low],
        thinking_summaries: vec![tau_proto::ThinkingSummary::Off],
        supports_compaction: false,
        supports_standalone_compaction: false,
        standalone_compaction_threshold: None,
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
        usage_observed_at_unix_ms: 123_000,
        window_seconds: 604_800,
        reset_at_unix_seconds: Some(700_000),
        remaining_seconds_at_timing_anchor: Some(300_000),
        timing_anchor_observed_at_unix_ms: Some(123_000),
        server_offset_ms: Some(0),
        server_offset_observed_at_unix_ms: Some(123_000),
    }
}

fn quota_binding() -> tau_proto::ProviderQuotaRouteBinding {
    tau_proto::ProviderQuotaRouteBinding {
        model: "chatgpt/gpt-5.6-sol".into(),
        limit_ids: vec![tau_proto::ProviderQuotaLimitId::parse("codex").expect("quota pool")],
        observed_at_unix_ms: 123_000,
        provenance: tau_proto::ProviderQuotaBindingProvenance::TurnEvent,
    }
}

/// Replace/Patch enforce source ownership and strict upstream sequencing while
/// applying complete records by stable key.
#[test]
fn provider_quota_replace_patch_and_spoofing_are_validated() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    harness.set_provider_models("owner", vec![quota_model()]);
    let epoch = tau_proto::ProviderQuotaEpoch::parse("epoch-1").expect("epoch");
    harness.handle_provider_quota_replace(
        "spoof",
        tau_proto::ProviderQuotaReplace {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: epoch.clone(),
            sequence: 1,
            establishes_new_epoch: true,
            windows: vec![quota_window(1_000)],
            route_bindings: vec![quota_binding()],
        },
    );
    assert!(harness.provider_quota.is_empty());
    harness.handle_provider_quota_replace(
        "owner",
        tau_proto::ProviderQuotaReplace {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: epoch.clone(),
            sequence: 1,
            establishes_new_epoch: true,
            windows: vec![quota_window(1_000)],
            route_bindings: vec![quota_binding()],
        },
    );
    assert_eq!(
        harness.provider_quota[&tau_proto::ProviderName::new("chatgpt")]
            .snapshot
            .sequence,
        1
    );
    harness.handle_provider_quota_patch(
        "owner",
        tau_proto::ProviderQuotaPatch {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: epoch,
            sequence: 2,
            windows: vec![quota_window(2_000)],
            removed_window_keys: Vec::new(),
            route_bindings: Vec::new(),
        },
    );
    let snapshot = &harness.provider_quota[&tau_proto::ProviderName::new("chatgpt")].snapshot;
    assert_eq!(snapshot.sequence, 2);
    assert_eq!(snapshot.windows[0].used_basis_points, 2_000);
}

/// A full two-pool account snapshot followed by the exact-model default-pool
/// turn binding projects both pool facts while preserving only `codex` as the
/// applicable route, matching the provider extension's real event sequence.
#[test]
fn provider_quota_two_pool_default_binding_projects_without_ambiguity() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    harness.set_provider_models("owner", vec![quota_model()]);
    let epoch = tau_proto::ProviderQuotaEpoch::parse("epoch-1").expect("epoch");
    harness.handle_provider_quota_replace(
        "owner",
        tau_proto::ProviderQuotaReplace {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: epoch.clone(),
            sequence: 1,
            establishes_new_epoch: true,
            windows: vec![
                quota_window_for("codex", 4_400),
                quota_window_for("codex_bengalfox", 0),
            ],
            route_bindings: Vec::new(),
        },
    );
    harness.handle_provider_quota_patch(
        "owner",
        tau_proto::ProviderQuotaPatch {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: epoch,
            sequence: 2,
            windows: Vec::new(),
            removed_window_keys: Vec::new(),
            route_bindings: vec![quota_binding()],
        },
    );

    let snapshot = &harness.provider_quota[&tau_proto::ProviderName::new("chatgpt")].snapshot;
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
    harness.set_provider_models("a-owner", vec![quota_model()]);
    harness.set_provider_models("z-winner", vec![quota_model()]);
    let replace = |sequence| tau_proto::ProviderQuotaReplace {
        provider: tau_proto::ProviderName::new("chatgpt"),
        profile_epoch: tau_proto::ProviderQuotaEpoch::parse("epoch-1").expect("epoch"),
        sequence,
        establishes_new_epoch: true,
        windows: vec![quota_window(1_000)],
        route_bindings: vec![quota_binding()],
    };
    harness.handle_provider_quota_replace("a-owner", replace(1));
    assert!(harness.provider_quota.is_empty());
    harness.handle_provider_quota_replace("z-winner", replace(1));
    assert_eq!(
        harness.provider_quota[&tau_proto::ProviderName::new("chatgpt")]
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
    harness.set_provider_models("owner-a", vec![quota_model()]);
    harness.set_provider_models("owner-b", vec![second]);
    harness.handle_provider_quota_replace(
        "owner-a",
        tau_proto::ProviderQuotaReplace {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: tau_proto::ProviderQuotaEpoch::parse("epoch-1").expect("epoch"),
            sequence: 1,
            establishes_new_epoch: true,
            windows: vec![quota_window(1_000)],
            route_bindings: vec![quota_binding()],
        },
    );
    assert!(harness.provider_quota.is_empty());
}

/// Harness-local model metadata invalidation does not consume provider sequence
/// space, so the provider's immediately following patch remains acceptable.
#[test]
fn model_change_preserves_provider_sequence_space() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    harness.apply_provider_models_snapshot("owner", vec![quota_model()]);
    let epoch = tau_proto::ProviderQuotaEpoch::parse("epoch-1").expect("epoch");
    harness.handle_provider_quota_replace(
        "owner",
        tau_proto::ProviderQuotaReplace {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: epoch.clone(),
            sequence: 1,
            establishes_new_epoch: true,
            windows: vec![quota_window(1_000)],
            route_bindings: vec![quota_binding()],
        },
    );
    let mut changed = quota_model();
    changed.context_window = 200_000;
    harness.apply_provider_models_snapshot("owner", vec![changed]);
    assert_eq!(
        harness.provider_quota[&tau_proto::ProviderName::new("chatgpt")]
            .snapshot
            .sequence,
        1
    );
    harness.handle_provider_quota_patch(
        "owner",
        tau_proto::ProviderQuotaPatch {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: epoch,
            sequence: 2,
            windows: vec![quota_window(2_000)],
            removed_window_keys: Vec::new(),
            route_bindings: Vec::new(),
        },
    );
    assert_eq!(
        harness.provider_quota[&tau_proto::ProviderName::new("chatgpt")]
            .snapshot
            .sequence,
        2
    );
}

/// Withdrawing the last effective model clears sensitive account state, and a
/// late subscriber receives original observation clocks only while state lives.
#[test]
fn quota_catch_up_preserves_clocks_and_model_withdrawal_clears() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    harness.apply_provider_models_snapshot("owner", vec![quota_model()]);
    harness.handle_provider_quota_replace(
        "owner",
        tau_proto::ProviderQuotaReplace {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: tau_proto::ProviderQuotaEpoch::parse("epoch-1").expect("epoch"),
            sequence: 1,
            establishes_new_epoch: true,
            windows: vec![quota_window(1_000)],
            route_bindings: vec![quota_binding()],
        },
    );
    let events = connect_test_client(&mut harness, "late-quota-ui", tau_proto::ClientKind::Ui);
    harness.replay_harness_notice(
        "late-quota-ui",
        &[EventSelector::Exact(
            tau_proto::EventName::HARNESS_PROVIDER_QUOTA_CHANGED,
        )],
    );
    let observed = events
        .lock()
        .expect("events")
        .iter()
        .find_map(|routed| match &routed.frame {
            HarnessOutputMessage::Deliver(delivery) => match delivery.event.as_ref() {
                Event::HarnessProviderQuotaChanged(changed) => {
                    Some(changed.windows[0].usage_observed_at_unix_ms)
                }
                _ => None,
            },
            _ => None,
        });
    assert_eq!(observed, Some(123_000));
    harness.apply_provider_models_snapshot("owner", Vec::new());
    assert!(harness.provider_quota.is_empty());
    harness.apply_provider_models_snapshot("owner", vec![quota_model()]);
    harness.handle_provider_quota_replace(
        "owner",
        tau_proto::ProviderQuotaReplace {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: tau_proto::ProviderQuotaEpoch::parse("epoch-1").expect("epoch"),
            sequence: 2,
            establishes_new_epoch: false,
            windows: vec![quota_window(2_000)],
            route_bindings: vec![quota_binding()],
        },
    );
    assert_eq!(
        harness.provider_quota[&tau_proto::ProviderName::new("chatgpt")]
            .snapshot
            .sequence,
        2
    );
}

/// Validated quota current state cannot be dropped or rewritten by an
/// interceptor, keeping live delivery consistent with catch-up cache truth.
#[test]
fn validated_quota_projection_is_must_pass_and_immutable() {
    let original = Event::HarnessProviderQuotaChanged(tau_proto::HarnessProviderQuotaChanged {
        provider: tau_proto::ProviderName::new("chatgpt"),
        profile_epoch: tau_proto::ProviderQuotaEpoch::parse("epoch-1").expect("epoch"),
        sequence: 1,
        windows: vec![quota_window(1_000)],
        route_bindings: vec![quota_binding()],
    });
    let replacement = Event::HarnessProviderQuotaChanged(tau_proto::HarnessProviderQuotaChanged {
        provider: tau_proto::ProviderName::new("chatgpt"),
        profile_epoch: tau_proto::ProviderQuotaEpoch::parse("epoch-1").expect("epoch"),
        sequence: 1,
        windows: vec![quota_window(9_000)],
        route_bindings: vec![quota_binding()],
    });
    assert!(super::super::interception::event_must_pass_by_default(
        &tau_proto::EventName::HARNESS_PROVIDER_QUOTA_CHANGED
    ));
    assert!(
        super::super::interception::immutable_protected_fact_was_modified(&original, &replacement)
    );
}

/// Explicit Clear retires its epoch at the provider's actual sequence and
/// prevents stale same-epoch or previously replaced epochs from resurrecting.
#[test]
fn provider_quota_clear_orders_and_retires_epochs() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    harness.set_provider_models("owner", vec![quota_model()]);
    let epoch_a = tau_proto::ProviderQuotaEpoch::parse("epoch-a").expect("epoch");
    harness.handle_provider_quota_replace(
        "owner",
        tau_proto::ProviderQuotaReplace {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: epoch_a.clone(),
            sequence: 5,
            establishes_new_epoch: true,
            windows: vec![quota_window(1_000)],
            route_bindings: vec![quota_binding()],
        },
    );
    harness.handle_provider_quota_clear(
        "owner",
        tau_proto::ProviderQuotaClear {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: epoch_a.clone(),
            sequence: 6,
        },
    );
    assert!(harness.provider_quota.is_empty());
    assert!(
        harness.provider_quota_retired_epochs[&tau_proto::ProviderName::new("chatgpt")]
            .contains(&epoch_a)
    );
    harness.handle_provider_quota_replace(
        "owner",
        tau_proto::ProviderQuotaReplace {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: epoch_a,
            sequence: 7,
            establishes_new_epoch: true,
            windows: vec![quota_window(2_000)],
            route_bindings: vec![quota_binding()],
        },
    );
    assert!(harness.provider_quota.is_empty());
    harness.handle_provider_quota_replace(
        "owner",
        tau_proto::ProviderQuotaReplace {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: tau_proto::ProviderQuotaEpoch::parse("epoch-b").expect("epoch"),
            sequence: 1,
            establishes_new_epoch: true,
            windows: vec![quota_window(2_000)],
            route_bindings: vec![quota_binding()],
        },
    );
    assert_eq!(
        harness.provider_quota[&tau_proto::ProviderName::new("chatgpt")]
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
    harness.apply_provider_models_snapshot("owner", vec![quota_model()]);
    harness.handle_provider_quota_replace(
        "owner",
        tau_proto::ProviderQuotaReplace {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: tau_proto::ProviderQuotaEpoch::parse("epoch-e").expect("epoch"),
            sequence: 1,
            establishes_new_epoch: true,
            windows: vec![quota_window(1_000)],
            route_bindings: vec![quota_binding()],
        },
    );
    harness.apply_provider_models_snapshot("owner", Vec::new());
    harness.apply_provider_models_snapshot("owner", vec![quota_model()]);
    harness.handle_provider_quota_replace(
        "owner",
        tau_proto::ProviderQuotaReplace {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: tau_proto::ProviderQuotaEpoch::parse("epoch-f").expect("epoch"),
            sequence: 2,
            establishes_new_epoch: false,
            windows: vec![quota_window(2_000)],
            route_bindings: vec![quota_binding()],
        },
    );
    assert_eq!(
        harness.provider_quota[&tau_proto::ProviderName::new("chatgpt")]
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
    harness.apply_provider_models_snapshot("owner", vec![quota_model()]);
    let epoch = tau_proto::ProviderQuotaEpoch::parse("epoch-e").expect("epoch");
    harness.handle_provider_quota_replace(
        "owner",
        tau_proto::ProviderQuotaReplace {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: epoch.clone(),
            sequence: 1,
            establishes_new_epoch: true,
            windows: vec![quota_window(1_000)],
            route_bindings: vec![quota_binding()],
        },
    );
    harness.apply_provider_models_snapshot("owner", Vec::new());
    assert!(
        harness
            .provider_quota_tombstones
            .contains_key(&tau_proto::ProviderName::new("chatgpt"))
    );
    harness.handle_provider_quota_clear(
        "owner",
        tau_proto::ProviderQuotaClear {
            provider: tau_proto::ProviderName::new("chatgpt"),
            profile_epoch: epoch.clone(),
            sequence: 2,
        },
    );
    assert!(
        !harness
            .provider_quota_tombstones
            .contains_key(&tau_proto::ProviderName::new("chatgpt"))
    );
    assert!(
        harness.provider_quota_retired_epochs[&tau_proto::ProviderName::new("chatgpt")]
            .contains(&epoch)
    );
}
