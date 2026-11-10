use super::*;

/// Directed outcomes remain distinct on the wire; absence of the optional
/// marker preserves historical ordinary message interpretation.
#[test]
fn message_acceptance_wire_roundtrip_and_legacy_marker_default() {
    let message = MessageDelivered::new(
        RawMessagePublisherId::new("bridge"),
        crate::MessageAgentTarget::new("agent"),
        crate::MessageFactId::new("message"),
        crate::MessageParty {
            stable_id: "sender".to_owned(),
            display_name: None,
            sender_auth: None,
            sender_trust: None,
        },
        None,
        "body",
    );
    let json = serde_json::to_value(&message).expect("message JSON");
    assert!(json.get("idempotent_acceptance").is_none());
    let old: MessageDelivered<RawMessagePublisherId> =
        serde_json::from_value(json).expect("old fact shape");
    assert!(!old.idempotent_acceptance);
    let request =
        crate::HarnessInputMessage::MessageAcceptanceRequest(Box::new(MessageAcceptanceRequest {
            request_id: "attempt".to_owned(),
            session_id: "session".parse().expect("session"),
            report: message,
        }));
    let mut bytes = Vec::new();
    ciborium::into_writer(&request, &mut bytes).expect("request encode");
    assert_eq!(
        ciborium::from_reader::<crate::HarnessInputMessage, _>(bytes.as_slice())
            .expect("request decode"),
        request
    );
    for outcome in [
        MessageAcceptanceOutcome::Accepted { duplicate: false },
        MessageAcceptanceOutcome::Accepted { duplicate: true },
        MessageAcceptanceOutcome::Rejected {
            reason: "refused".to_owned(),
        },
        MessageAcceptanceOutcome::Unknown {
            reason: "uncertain".to_owned(),
        },
    ] {
        let result =
            crate::HarnessOutputMessage::MessageAcceptanceResult(MessageAcceptanceResult {
                request_id: "attempt".to_owned(),
                outcome,
            });
        bytes.clear();
        ciborium::into_writer(&result, &mut bytes).expect("result encode");
        assert_eq!(
            ciborium::from_reader::<crate::HarnessOutputMessage, _>(bytes.as_slice())
                .expect("result decode"),
            result
        );
    }
}
