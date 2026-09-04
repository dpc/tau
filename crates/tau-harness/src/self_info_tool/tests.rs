#[cfg(unix)]
use std::ffi::OsString;
#[cfg(unix)]
use std::path::PathBuf;

use super::*;

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
    assert_eq!(resolve_result(&CborValue::Map(Vec::new()), Some(&info)), Ok(
        "agent_id: engineer-test\nsession_id: session-test\nsession_dir: (none)\nmodel: provider/model\neffort_requested: 0.75\neffort_effective: high\nstatus: working\nstatus_task_name: Implement self information".to_owned()
    ));
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
    assert_eq!(output.lines().count(), 8);
}
