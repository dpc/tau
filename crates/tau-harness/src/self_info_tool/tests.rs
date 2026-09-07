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
            inference: None,
            overflow: false,
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
        Ok(format!(
            "agent_id: engineer-test\nsession_id: session-test\nsession_dir: (none)\ntau: {}\nmodel: provider/model\neffort_requested: 0.75\neffort_effective: high\nstatus: working\nstatus_task_name: Implement self information",
            tau_version_label()
        ))
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

/// Complete operational facts render concisely while inactive and
/// non-applicable diagnostic details stay hidden.
#[test]
fn operational_records_are_concise_and_applicable() {
    let mut info = info(tau_proto::SessionAgentWorkStatus::default());
    info.context = InternalSelfContext {
        input_tokens: Some(tau_proto::TokenCount::new(1_234)),
        cached_tokens: Some(tau_proto::TokenCount::new(234)),
        context_window: Some(tau_proto::TokenCount::new(12_000)),
        input_token_limit: Some(tau_proto::TokenCount::new(10_000)),
    };
    info.compaction = InternalSelfCompaction {
        inference: Some("threshold=8000".to_owned()),
        overflow: true,
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
    assert!(output.contains("context_usage: 1234/10000"));
    assert!(output.contains("compaction: threshold=8000"));
    assert!(output.contains("compaction: on_context_overflow"));
    assert!(output.contains("compaction: threshold=7500 at=outer_turn_finished"));
    assert!(output.contains("provider_quota_usage: 12.34% resets_seconds=345600"));
    assert!(!output.contains("cached"));
    assert!(!output.contains("dormant"));
    assert!(!output.contains("freshness"));
    assert!(!output.contains("applies_to_model"));
}

/// Partial context is omitted, while known applicable quota usage survives a
/// missing reset and multiple windows receive the shortest useful identity.
#[test]
fn partial_context_and_multiple_quota_windows_stay_truthful() {
    let mut info = info(tau_proto::SessionAgentWorkStatus::default());
    info.context.input_token_limit = Some(tau_proto::TokenCount::new(10_000));
    let window =
        |window_id: &str, used_basis_points, applies_to_model| InternalSelfProviderQuotaWindow {
            limit_id: tau_proto::ProviderQuotaLimitId::parse("codex").expect("pool"),
            window_id: tau_proto::ProviderQuotaWindowId::parse(window_id).expect("window"),
            used_basis_points,
            window_seconds: 100,
            observed_age_seconds: None,
            reset_at_unix_seconds: None,
            remaining_seconds: None,
            applies_to_model,
        };
    info.provider_quota = Some(InternalSelfProviderQuota {
        model_binding_age_seconds: None,
        model_limit_ids: vec![tau_proto::ProviderQuotaLimitId::parse("codex").expect("pool")],
        windows: vec![
            window("primary", 300, true),
            window("secondary", 400, true),
            window("unrelated", 500, false),
        ],
    });

    let output = format_headers(&info);
    assert!(!output.contains("context_usage"));
    assert!(output.contains("provider_quota_usage: 3.00% window=primary"));
    assert!(output.contains("provider_quota_usage: 4.00% window=secondary"));
    assert!(!output.contains("unrelated"));
    assert!(!output.contains("resets_seconds"));
}

/// Duplicate window names switch the minimal quota discriminator from window
/// identity to pool identity.
#[test]
fn quota_discriminator_uses_pool_for_duplicate_window_names() {
    let window = |pool: &str| InternalSelfProviderQuotaWindow {
        limit_id: tau_proto::ProviderQuotaLimitId::parse(pool).expect("pool"),
        window_id: tau_proto::ProviderQuotaWindowId::parse("primary").expect("window"),
        used_basis_points: 0,
        window_seconds: 100,
        observed_age_seconds: None,
        reset_at_unix_seconds: None,
        remaining_seconds: None,
        applies_to_model: true,
    };
    let left = window("codex");
    let right = window("other");
    assert_eq!(
        quota_discriminator(&[&left, &right]),
        QuotaDiscriminator::Pool
    );
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
    assert_eq!(output.lines().count(), 9);
}
