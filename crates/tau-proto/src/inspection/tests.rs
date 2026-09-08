use super::*;

/// Missing purpose preserves ordinary Configure; an explicit inspection purpose
/// survives wire decoding instead of being mistaken for runtime initialization.
#[test]
fn configure_purpose_is_additive_and_explicit() {
    let old = serde_json::json!({
        "config": null,
        "instance_name": "inspection-test"
    });
    let ordinary: crate::Configure =
        serde_json::from_value(old.clone()).expect("valid inspection test fixture");
    assert_eq!(ordinary.purpose, ConfigurePurpose::Runtime);
    assert!(
        serde_json::to_value(&ordinary)
            .expect("valid inspection test fixture")
            .get("purpose")
            .is_none()
    );
    let mut inspection = old;
    inspection["purpose"] = serde_json::json!("declaration_inspection");
    let inspection: crate::Configure =
        serde_json::from_value(inspection).expect("valid inspection test fixture");
    assert_eq!(inspection.purpose, ConfigurePurpose::DeclarationInspection);
}

/// Advertised inspection support is absent/false for existing peers and is an
/// ignorable additive field for an old normal Hello decoder.
#[test]
fn hello_inspection_support_does_not_extend_authority_enum() {
    /// Pre-inspection shape deliberately ignores additive Hello fields.
    #[derive(Deserialize)]
    struct OldHello {
        /// Existing closed authority variants must remain decodable.
        capabilities: Vec<crate::PeerCapability>,
    }
    let mut value = serde_json::json!({
        "protocol_version": {"major": 4, "minor": 1},
        "client_name": "inspection-test",
        "client_kind": "tool",
        "capabilities": []
    });
    let old: crate::Hello =
        serde_json::from_value(value.clone()).expect("valid inspection test fixture");
    assert!(!old.declaration_inspection);
    value["declaration_inspection"] = serde_json::json!(true);
    let new: crate::Hello =
        serde_json::from_value(value.clone()).expect("valid inspection test fixture");
    assert!(new.declaration_inspection);
    let old_decoder: OldHello =
        serde_json::from_value(value).expect("valid inspection test fixture");
    assert!(old_decoder.capabilities.is_empty());
}

/// Completion remains a distinct directed message, never an event or Ready,
/// and roundtrips completeness reasons with the typed empty inventory.
#[test]
fn inspection_completion_has_distinct_wire_tag() {
    let message = crate::HarnessInputMessage::InspectionComplete(InspectionComplete {
        gaps: vec![
            InspectionGap::RuntimeDeclarations,
            InspectionGap::ContextDiscovery,
        ],
        ..Default::default()
    });
    let value = serde_json::to_value(&message).expect("valid inspection test fixture");
    assert_eq!(value["message"], "inspection_complete");
    assert_eq!(
        serde_json::from_value::<crate::HarnessInputMessage>(value)
            .expect("valid inspection test fixture"),
        message
    );
}
