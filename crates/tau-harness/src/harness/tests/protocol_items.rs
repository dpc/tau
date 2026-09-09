use super::*;

/// Only live timestamped deliveries retain their metadata in the shared test
/// wrapper; replay and synthetic deliveries remain ordinary event assertions.
#[test]
fn protocol_item_delivery_classification_preserves_all_metadata_cases() {
    let event = Event::AgentUserInteractionRecorded(tau_proto::AgentUserInteractionRecorded {
        agent_id: crate::parse_agent_id("delivery-agent"),
    });
    for replay in [false, true] {
        for recorded_at in [None, Some(tau_proto::UnixMicros::new(123))] {
            let delivery = EventDelivery {
                event: Box::new(event.clone()),
                replay,
                recorded_at,
            };
            let item = TestProtocolItem::from_output_message(HarnessOutputMessage::Deliver(
                delivery.clone(),
            ));
            match item {
                TestProtocolItem::Message(TestMessage::LiveDelivery(actual)) => {
                    assert!(!replay);
                    assert!(recorded_at.is_some());
                    assert_eq!(actual, delivery);
                }
                TestProtocolItem::Event(actual) => {
                    assert!(replay || recorded_at.is_none());
                    assert_eq!(actual, event);
                }
                other => panic!("unexpected delivery wrapper: {other:?}"),
            }
        }
    }
}
