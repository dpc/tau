use super::*;

/// Builds one valid deterministic gateway report identity.
fn report_id(update_id: i64) -> TelegramReportId {
    TelegramReportId::for_gateway(
        "checkpoint-test",
        TelegramUpdateId::new(update_id).expect("valid update"),
    )
}

/// Builds a minimal exact routed delivery for checkpoint tests.
fn delivery(report_id: &TelegramReportId) -> GatewayDelivery {
    GatewayDelivery {
        request_id: report_id.clone(),
        session_id: "session".to_owned(),
        agent_id: "agent".to_owned(),
        message_id: "message".to_owned(),
        sender_id: "7".to_owned(),
        source: "sender".to_owned(),
        conversation_id: "10".to_owned(),
        text: "body".to_owned(),
    }
}

/// Ensures an acknowledged later route cannot skip an earlier pending route,
/// while a later non-routed checkpoint advances with the prefix once unblocked.
#[test]
fn mixed_prefix_advances_only_contiguously() {
    let mut checkpoints = GatewayCheckpoints::default();
    let report_10 = report_id(10);
    let report_12 = report_id(12);
    checkpoints.insert_routed(
        TelegramUpdateId::new(10).expect("valid update"),
        delivery(&report_10),
    );
    checkpoints.insert_non_routed(TelegramUpdateId::new(11).expect("valid update"));
    checkpoints.insert_routed(
        TelegramUpdateId::new(12).expect("valid update"),
        delivery(&report_12),
    );

    assert!(checkpoints.acknowledge(&report_12));
    assert_eq!(checkpoints.advance_prefix(None), None);
    assert!(checkpoints.acknowledge(&report_10));
    assert_eq!(checkpoints.advance_prefix(None), Some(13));
    assert!(checkpoints.pending_deliveries().is_empty());
}

/// Ensures persisted checkpoints retain the exact report and acknowledgement
/// state needed for restart replay.
#[test]
fn serde_round_trip_replays_exact_pending_report() {
    let mut checkpoints = GatewayCheckpoints::default();
    let report_id = report_id(42);
    checkpoints.insert_routed(
        TelegramUpdateId::new(42).expect("valid update"),
        delivery(&report_id),
    );
    let bytes = serde_json::to_vec(&checkpoints).expect("encode checkpoints");
    let restored: GatewayCheckpoints = serde_json::from_slice(&bytes).expect("decode checkpoints");

    assert_eq!(restored.pending_deliveries(), vec![delivery(&report_id)]);
}

/// Ensures corrupted durable state cannot construct an update ID whose
/// successor would panic during prefix advancement.
#[test]
fn deserialization_rejects_maximum_update_id() {
    let json = format!(
        r#"[{{"update_id":{},"checkpoint":{{"kind":"non_routed"}}}}]"#,
        i64::MAX
    );

    assert!(serde_json::from_str::<GatewayCheckpoints>(&json).is_err());
}

/// Ensures corrupt persisted checkpoint order cannot move the Telegram cursor
/// backwards during prefix advancement.
#[test]
fn deserialization_rejects_unordered_update_ids() {
    let json = r#"[
        {"update_id":20,"checkpoint":{"kind":"non_routed"}},
        {"update_id":10,"checkpoint":{"kind":"non_routed"}}
    ]"#;

    assert!(serde_json::from_str::<GatewayCheckpoints>(json).is_err());
}

/// Ensures duplicate persisted update identities cannot leave a second routed
/// record unreachable by exact acknowledgement.
#[test]
fn deserialization_rejects_duplicate_update_ids() {
    let json = r#"[
        {"update_id":10,"checkpoint":{"kind":"non_routed"}},
        {"update_id":10,"checkpoint":{"kind":"non_routed"}}
    ]"#;

    assert!(serde_json::from_str::<GatewayCheckpoints>(json).is_err());
}
