use std::path::Path;

use super::*;

/// Session rows omit redundant identity labels without losing metadata, scan
/// warnings, or the structured result consumed by the model.
#[test]
fn session_ui_uses_bare_ids_and_preserves_metadata() {
    let result = serde_json::from_value::<CborValue>(serde_json::json!({
        "sessions": [
            {"session_id": "isolate-zvpks2"},
            {"session_id": "tau-ri282t", "state": "running", "prompt": "PRIVATE"}
        ],
        "truncated": true,
        "scan_truncated": true
    }))
    .expect("discovery result");
    let original = result.clone();
    let display = discovery_tool_display(&result, DiscoveryKind::Sessions);
    assert_eq!(
        display.payload,
        Some(ToolUsePayload::Text {
            text:
                "isolate-zvpks2\ntau-ri282t  state: running\ntruncated: true\nscan_truncated: true"
                    .into()
        })
    );
    assert_eq!(display.stats.matches, Some(2));
    assert_eq!(display.status_text, "ok");
    assert_eq!(display.info_chips, ["truncated", "scan truncated"]);
    assert_eq!(result, original);

    let empty = discovery_tool_display(&CborValue::Map(vec![]), DiscoveryKind::Sessions);
    assert_eq!(empty.stats.matches, Some(0));
    assert_eq!(empty.payload, None);
}

/// Returned identities and incomplete-scan facts remain visible, but arbitrary
/// result fields and private prompt text never become UI payloads.
#[test]
fn discovery_ui_projects_only_identity_and_scan_facts() {
    let result = CborValue::Map(vec![
        (
            CborValue::Text("agents".into()),
            CborValue::Array(vec![CborValue::Map(vec![
                (
                    CborValue::Text("agent_id".into()),
                    CborValue::Text("reviewer-demo".into()),
                ),
                (
                    CborValue::Text("role".into()),
                    CborValue::Text("reviewer".into()),
                ),
                (
                    CborValue::Text("state".into()),
                    CborValue::Text("idle".into()),
                ),
                (
                    CborValue::Text("prompt".into()),
                    CborValue::Text("PRIVATE".into()),
                ),
            ])]),
        ),
        (CborValue::Text("truncated".into()), CborValue::Bool(true)),
    ]);
    let display = discovery_tool_display(&result, DiscoveryKind::Agents);
    assert_eq!(display.stats.matches, Some(1));
    assert_eq!(display.info_chips, ["truncated"]);
    let Some(ToolUsePayload::Text { text }) = &display.payload else {
        panic!("identity rows")
    };
    assert!(text.contains("reviewer-demo"));
    assert!(text.contains("agent_id: reviewer-demo"));
    assert!(!text.contains("PRIVATE"));
    let sessions = CborValue::Map(vec![
        (
            CborValue::Text("sessions".into()),
            CborValue::Array(vec![CborValue::Map(vec![(
                CborValue::Text("session_id".into()),
                CborValue::Text("session-demo".into()),
            )])]),
        ),
        (
            CborValue::Text("scan_truncated".into()),
            CborValue::Bool(true),
        ),
    ]);
    if let Ok(directory) = std::env::var("TAU_TOOL_UI_FIXTURE_DIR") {
        std::fs::write(Path::new(&directory).join("discovery.json"),
            serde_json::to_vec_pretty(&serde_json::json!([
                {"case":"agents-truncated", "tool_name":"agent_list", "display":display},
                {"case":"sessions-scan-truncated", "tool_name":"session_list", "display":discovery_tool_display(&sessions, DiscoveryKind::Sessions)},
                {"case":"agents-empty", "tool_name":"agent_list", "display":discovery_tool_display(&CborValue::Map(vec![]), DiscoveryKind::Agents)},
            ])).expect("serialize samples")).expect("write samples");
    }
}
