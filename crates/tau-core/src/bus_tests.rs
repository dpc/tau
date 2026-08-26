use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use tau_proto::{
    ClientKind, Event, EventName, EventSelector, HarnessNotice, HarnessOutputMessage, NoticeLevel,
    NoticePurpose,
};

use super::EventBus;
use crate::{
    Connection, ConnectionOrigin, ConnectionSendError, ConnectionSink, PendingConnectionMetadata,
    RoutedFrame, SharedConsumerId, SharedDeliveryGroup, SharedDeliveryTarget,
};

fn test_connection_id(value: &str) -> tau_proto::ConnectionId {
    tau_proto::ConnectionId::parse(value).expect("test connection id")
}

fn test_extension_name(value: &str) -> tau_proto::ExtensionName {
    tau_proto::ExtensionName::parse(value).expect("test extension name")
}

#[derive(Clone, Default)]
struct DeliveryTrace(Arc<Mutex<Vec<String>>>);

impl DeliveryTrace {
    fn record(&self, value: String) {
        self.0.lock().expect("delivery trace").push(value);
    }

    fn normalized(&self) -> Vec<String> {
        let mut values = self.0.lock().expect("delivery trace").clone();
        values.sort_unstable();
        values
    }
}

struct ModelSink {
    label: String,
    shared_target: Option<SharedDeliveryTarget>,
    admitted_consumers: Arc<HashSet<SharedConsumerId>>,
    fail_call: bool,
    trace: DeliveryTrace,
}

struct ModelConnection<'a> {
    label: &'a str,
    kind: ClientKind,
    shared_target: Option<SharedDeliveryTarget>,
    fail_call: bool,
    subscribed: bool,
    visible: bool,
}

impl ConnectionSink for ModelSink {
    fn send(&mut self, _frame: RoutedFrame) -> Result<(), ConnectionSendError> {
        self.trace.record(format!("legacy:{}", self.label));
        if self.fail_call {
            Err(ConnectionSendError::new("injected legacy failure"))
        } else {
            Ok(())
        }
    }

    fn shared_delivery_target(&self) -> Option<SharedDeliveryTarget> {
        self.shared_target
    }

    fn send_shared(
        &mut self,
        _frame: RoutedFrame,
        targets: &[SharedDeliveryTarget],
    ) -> Result<Vec<SharedDeliveryTarget>, ConnectionSendError> {
        for target in targets {
            self.trace.record(format!("shared:{:?}", target.consumer()));
        }
        if self.fail_call {
            return Err(ConnectionSendError::new("injected shared failure"));
        }
        Ok(targets
            .iter()
            .copied()
            .filter(|target| self.admitted_consumers.contains(&target.consumer()))
            .collect())
    }
}

fn notice() -> HarnessOutputMessage {
    HarnessOutputMessage::deliver(Event::HarnessNotice(HarnessNotice {
        kind: "routing-model".to_owned(),
        message: "payload".to_owned(),
        level: NoticeLevel::Info,
        purpose: NoticePurpose::Diagnostic,
    }))
}

fn connect_model(
    bus: &mut EventBus,
    trace: &DeliveryTrace,
    admitted_consumers: Arc<HashSet<SharedConsumerId>>,
    model: ModelConnection<'_>,
) {
    let id = test_connection_id(model.label);
    let sink = ModelSink {
        label: model.label.to_owned(),
        shared_target: model.shared_target,
        admitted_consumers,
        fail_call: model.fail_call,
        trace: trace.clone(),
    };
    let visible = model.visible;
    let connection = Connection::new(
        PendingConnectionMetadata {
            id: Some(id.clone()),
            name: test_extension_name(model.label),
            kind: model.kind,
            origin: ConnectionOrigin::InMemory,
        },
        Box::new(sink),
    )
    .with_visibility_filter(Box::new(move |_frame: &RoutedFrame| visible));
    bus.connect(connection);
    let selector = if model.subscribed {
        EventSelector::Exact(EventName::HARNESS_NOTICE)
    } else {
        EventSelector::Exact(EventName::TOOL_STARTED)
    };
    bus.set_subscriptions(&id, Vec::new(), vec![selector])
        .expect("model subscription");
}

fn model_bus() -> (EventBus, DeliveryTrace) {
    let mut bus = EventBus::new();
    let trace = DeliveryTrace::default();
    let group = SharedDeliveryGroup::new(7);
    let admitted = Arc::new(HashSet::from([
        SharedConsumerId::new(1),
        SharedConsumerId::new(3),
    ]));
    for (label, consumer, fail) in [
        ("shared-admitted", 1, false),
        ("shared-retired", 2, false),
        ("shared-failing", 3, true),
    ] {
        connect_model(
            &mut bus,
            &trace,
            Arc::clone(&admitted),
            ModelConnection {
                label,
                kind: ClientKind::Tool,
                shared_target: Some(SharedDeliveryTarget::new(
                    if fail {
                        SharedDeliveryGroup::new(8)
                    } else {
                        group
                    },
                    SharedConsumerId::new(consumer),
                )),
                fail_call: fail,
                subscribed: true,
                visible: true,
            },
        );
    }
    connect_model(
        &mut bus,
        &trace,
        Arc::clone(&admitted),
        ModelConnection {
            label: "legacy-ok",
            kind: ClientKind::Tool,
            shared_target: None,
            fail_call: false,
            subscribed: true,
            visible: true,
        },
    );
    connect_model(
        &mut bus,
        &trace,
        Arc::clone(&admitted),
        ModelConnection {
            label: "legacy-fail",
            kind: ClientKind::Tool,
            shared_target: None,
            fail_call: true,
            subscribed: true,
            visible: true,
        },
    );
    connect_model(
        &mut bus,
        &trace,
        Arc::clone(&admitted),
        ModelConnection {
            label: "unsubscribed",
            kind: ClientKind::Tool,
            shared_target: None,
            fail_call: false,
            subscribed: false,
            visible: true,
        },
    );
    connect_model(
        &mut bus,
        &trace,
        Arc::clone(&admitted),
        ModelConnection {
            label: "filtered",
            kind: ClientKind::Tool,
            shared_target: None,
            fail_call: false,
            subscribed: true,
            visible: false,
        },
    );
    connect_model(
        &mut bus,
        &trace,
        admitted,
        ModelConnection {
            label: "excluded-provider",
            kind: ClientKind::Provider,
            shared_target: None,
            fail_call: false,
            subscribed: true,
            visible: true,
        },
    );
    (bus, trace)
}

/// The no-report collector must execute the same selector, filter, exclusion,
/// shared-generation, retirement, failure, and legacy delivery decisions.
#[test]
fn no_report_broadcast_matches_detailed_routing_model() {
    let (mut detailed_bus, detailed_trace) = model_bus();
    let (mut no_report_bus, no_report_trace) = model_bus();
    let excluded = [ClientKind::Provider];

    let report = detailed_bus.publish_from_excluding_kinds(None, notice(), &excluded);
    no_report_bus.publish_from_excluding_kinds_without_report(None, notice(), &excluded);

    assert_eq!(detailed_trace.normalized(), no_report_trace.normalized());
    assert_eq!(report.delivered_to.len(), 2);
    assert_eq!(report.blocked_by_filter.len(), 1);
    assert_eq!(report.skipped_by_subscription.len(), 2);
    assert_eq!(report.failed_deliveries.len(), 3);
}

/// Detailed broadcasts retain their exact per-vector ordering while using the
/// shared collector traversal instead of a separate routing loop.
#[test]
fn detailed_route_report_order_is_stable() {
    let (mut bus, _) = model_bus();
    let excluded = [ClientKind::Provider];

    let first = bus.publish_from_excluding_kinds(None, notice(), &excluded);
    let second = bus.publish_from_excluding_kinds(None, notice(), &excluded);

    assert_eq!(first, second);
}

/// A discarded report must clone no IDs into diagnostic vectors, and admitted
/// shared targets must require exactly one constant-time membership probe each.
#[test]
fn no_report_broadcast_omits_report_clones_and_reconciles_linearly() {
    let mut bus = EventBus::new();
    let trace = DeliveryTrace::default();
    let group = SharedDeliveryGroup::new(11);
    let consumers = (0..64).map(SharedConsumerId::new).collect::<HashSet<_>>();
    let admitted = Arc::new(consumers.clone());
    for (index, consumer) in consumers.into_iter().enumerate() {
        connect_model(
            &mut bus,
            &trace,
            Arc::clone(&admitted),
            ModelConnection {
                label: &format!("shared-{index}"),
                kind: ClientKind::Tool,
                shared_target: Some(SharedDeliveryTarget::new(group, consumer)),
                fail_call: false,
                subscribed: true,
                visible: true,
            },
        );
    }
    connect_model(
        &mut bus,
        &trace,
        Arc::clone(&admitted),
        ModelConnection {
            label: "skipped",
            kind: ClientKind::Tool,
            shared_target: None,
            fail_call: false,
            subscribed: false,
            visible: true,
        },
    );
    connect_model(
        &mut bus,
        &trace,
        admitted,
        ModelConnection {
            label: "blocked",
            kind: ClientKind::Tool,
            shared_target: None,
            fail_call: false,
            subscribed: true,
            visible: false,
        },
    );

    bus.publish_from_excluding_kinds_without_report(None, notice(), &[]);

    assert_eq!(bus.last_route_work().report_id_clones, 0);
    assert_eq!(bus.last_route_work().report_entries, 0);
    assert_eq!(bus.last_route_work().admitted_membership_probes, 64);
}
