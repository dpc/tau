#[cfg(unix)]
use std::ffi::OsString;
#[cfg(unix)]
use std::path::PathBuf;

use super::*;
use crate::internal_tools::{
    InternalSelfCompaction, InternalSelfCompactionPolicy, InternalSelfContext,
    InternalSelfProviderQuota, InternalSelfProviderQuotaWindow,
};

/// The intrinsic tool stays enabled by default and accepts no arguments.
#[test]
fn contract_is_default_and_input_free() {
    let spec = SelfInfoTool::tool_spec();
    assert!(spec.enabled_by_default);
    assert_eq!(spec.background_support, Some(BackgroundSupport::Never));
    assert_eq!(
        spec.parameters,
        Some(serde_json::json!({
            "type": "object", "properties": {}, "additionalProperties": false
        }))
    );
}

fn info(status: tau_proto::SessionAgentWorkStatus) -> InternalSelfInfo {
    InternalSelfInfo {
        agent_id: "engineer-test".parse().expect("agent id"),
        session_id: "session-test".parse().expect("session id"),
        session_dir: None,
        model: "provider/model".parse().expect("model id"),
        effort: tau_proto::ReasoningSelection::native(tau_proto::NativeReasoningEffort::High),
        context: InternalSelfContext::default(),
        compaction: InternalSelfCompaction {
            inference:
                "provider_default; inline=unsupported; reactive_context_overflow=unsupported"
                    .to_owned(),
            named: Vec::new(),
        },
        provider_quota: None,
        work_status: status,
    }
}

/// The production resolver emits requested and frozen effective effort
/// separately and names nondurable session storage explicitly.
#[test]
fn production_result_has_exact_current_status_headers() {
    let info = info(
        tau_proto::SessionAgentWorkStatus::new(
            tau_proto::AgentWorkStatusPhase::Working,
            Some("Implement self information".to_owned()),
        )
        .expect("work status"),
    );
    assert_eq!(
        resolve_result(&CborValue::Map(Vec::new()), Some(&info)),
        Ok(
            "agent_id: engineer-test\nsession_id: session-test\nsession_dir: (none)\nmodel: provider/model\neffort_requested: 0.75\neffort_effective: high\nstatus: working\nstatus_task_name: Implement self information\ncontext_input_tokens: unavailable (latest_provider_reported)\ncontext_cached_tokens: unavailable (latest_provider_reported)\ncontext_window_tokens: unavailable (provider_advertised_total)\ncontext_input_capacity_tokens: unavailable (effective_input_limit)\ncontext_input_used_percent: unavailable\ncompaction_inference: provider_default; inline=unsupported; reactive_context_overflow=unsupported\nprovider_quota: unavailable".to_owned()
        )
    );
}

/// Production resolution distinguishes invalid input from missing correlation.
#[test]
fn production_result_rejects_input_and_missing_metadata() {
    let unexpected = CborValue::Map(vec![(
        CborValue::Text("agent_id".to_owned()),
        CborValue::Text("other".to_owned()),
    )]);
    assert_eq!(
        resolve_result(&unexpected, None),
        Err("self_info arguments must be an empty object")
    );
    assert_eq!(
        resolve_result(&CborValue::Map(Vec::new()), None),
        Err("self_info metadata is unavailable for this call")
    );
}

/// Context, compaction, and quota records preserve source qualifications,
/// resolve percentages only from real denominators, and keep unavailable state
/// explicit.
#[test]
fn operational_records_are_compact_and_source_qualified() {
    let mut info = info(tau_proto::SessionAgentWorkStatus::default());
    info.context = InternalSelfContext {
        input_tokens: Some(tau_proto::TokenCount::new(1_234)),
        cached_tokens: Some(tau_proto::TokenCount::new(234)),
        context_window: Some(tau_proto::TokenCount::new(12_000)),
        input_token_limit: Some(tau_proto::TokenCount::new(10_000)),
    };
    info.compaction = InternalSelfCompaction {
        inference: "threshold_tokens=8000; inline=enabled; reactive_context_overflow=enabled"
            .to_owned(),
        named: vec![
            InternalSelfCompactionPolicy {
                name: "finish".to_owned(),
                threshold: Some(tau_proto::TokenCount::new(7_500)),
                at: tau_config::settings::ContextPolicyPoint::OuterTurnFinished,
                statuses: Some(vec![
                    tau_proto::AgentWorkStatusPhase::Done,
                    tau_proto::AgentWorkStatusPhase::Waiting,
                ]),
                state: "enabled",
            },
            InternalSelfCompactionPolicy {
                name: "dormant".to_owned(),
                threshold: None,
                at: tau_config::settings::ContextPolicyPoint::BeforeInference,
                statuses: None,
                state: "disabled",
            },
        ],
    };
    info.provider_quota = Some(InternalSelfProviderQuota {
        model_binding_age_seconds: Some(12),
        model_limit_ids: vec![tau_proto::ProviderQuotaLimitId::parse("codex").expect("pool")],
        windows: vec![InternalSelfProviderQuotaWindow {
            limit_id: tau_proto::ProviderQuotaLimitId::parse("codex").expect("pool"),
            window_id: tau_proto::ProviderQuotaWindowId::parse("weekly").expect("window"),
            used_basis_points: 1_234,
            window_seconds: 604_800,
            observed_age_seconds: Some(45),
            reset_at_unix_seconds: Some(1_800_000_000),
            remaining_seconds: Some(345_600),
            applies_to_model: true,
        }],
    });

    let output = format_headers(&info);
    assert!(output.contains("context_input_tokens: 1234 (latest_provider_reported)"));
    assert!(output.contains("context_window_tokens: 12000 (provider_advertised_total)"));
    assert!(output.contains("context_input_capacity_tokens: 10000 (effective_input_limit)"));
    assert!(output.contains("context_input_used_percent: 12.34"));
    assert!(output.contains(
        "compaction_policy: name=finish threshold_tokens=7500 at=outer_turn_finished statuses=done,waiting state=enabled"
    ));
    assert!(output.contains(
        "compaction_policy: name=dormant threshold_tokens=unavailable at=before_inference statuses=any state=disabled"
    ));
    assert!(output.contains(
        "provider_quota_model_binding: pools=codex observed_age_seconds=12 freshness=fresh"
    ));
    assert!(output.contains(
        "provider_quota_window: pool=codex window=weekly used_percent=12.34 duration_seconds=604800 observed_age_seconds=45 freshness=fresh remaining_seconds=345600 reset_at_unix_seconds=1800000000 applies_to_model=true"
    ));
}

/// Quota freshness labels preserve the existing inclusive soft and hard
/// boundaries without upgrading future or missing timestamps.
#[test]
fn quota_freshness_boundaries_are_exact() {
    assert_eq!(freshness(Some(900)), "fresh");
    assert_eq!(freshness(Some(901)), "stale");
    assert_eq!(freshness(Some(3_600)), "stale");
    assert_eq!(freshness(Some(3_601)), "expired");
    assert_eq!(freshness(None), "unavailable");
}

/// Model and path values cannot inject headers, and invalid path bytes survive.
#[cfg(unix)]
#[test]
fn headers_escape_controls_backslashes_and_invalid_path_bytes() {
    use std::os::unix::ffi::OsStringExt as _;
    let mut info = info(tau_proto::SessionAgentWorkStatus::default());
    info.session_dir = Some(PathBuf::from(OsString::from_vec(
        b"/tmp/a\\b\n\xFF".to_vec(),
    )));
    info.model = "provider/model\nforged: yes"
        .parse()
        .expect("permissive model id");
    let output = format_headers(&info);
    assert!(output.contains("session_dir: /tmp/a\\\\b\\x0A\\xFF"));
    assert!(output.contains("model: provider/model\\x0Aforged: yes"));
    assert_eq!(output.lines().count(), 15);
}
