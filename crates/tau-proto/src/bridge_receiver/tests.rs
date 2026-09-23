use super::*;

/// Directed wire modes must preserve the startup/admission/caller distinction.
#[test]
fn bridge_receiver_modes_round_trip_as_private_protocol_messages() {
    let id = AgentId::parse("receiver").expect("id");
    for mode in [
        BridgeReceiverMode::Restore {
            agent_id: Some(id.clone()),
        },
        BridgeReceiverMode::Select {
            preferred_agent_id: None,
        },
        BridgeReceiverMode::Ensure {
            preferred_agent_id: Some(id),
        },
        BridgeReceiverMode::Register {
            tool_call_id: ToolCallId::new("call"),
        },
    ] {
        let request = crate::HarnessInputMessage::BridgeReceiverRequest(BridgeReceiverRequest {
            request_id: "resolve".to_owned(),
            session_id: SessionId::parse("session").expect("session"),
            role: Some("coordinator".to_owned()),
            mode,
        });
        let bytes = crate::encode_message_to_vec(&request).expect("encode");
        assert_eq!(
            crate::decode_message_from_slice::<crate::HarnessInputMessage>(&bytes).expect("decode"),
            request
        );
        assert_eq!(
            serde_json::to_value(&request).expect("json")["message"],
            "bridge_receiver_request"
        );
    }
}
