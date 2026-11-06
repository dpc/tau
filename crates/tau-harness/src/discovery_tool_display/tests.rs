use std::path::Path;

use super::*;

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
