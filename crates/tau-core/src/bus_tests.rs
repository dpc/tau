use std::cell::Cell;
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
    notice_with_message("payload")
}

fn notice_with_message(message: &str) -> HarnessOutputMessage {
    HarnessOutputMessage::deliver(Event::HarnessNotice(HarnessNotice {
        kind: "routing-model".to_owned(),
        message: message.to_owned(),
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

/// Count-only broadcasts must preserve detailed delivery decisions without
/// retaining recipient identities or errors for prompt-path diagnostics.
#[test]
fn delivery_count_broadcast_matches_detailed_delivery_outcomes_without_report_work() {
    let (mut detailed_bus, detailed_trace) = model_bus();
    let (mut counted_bus, counted_trace) = model_bus();
    let excluded = [ClientKind::Provider];

    let report = detailed_bus.publish_from_excluding_kinds(None, notice(), &excluded);
    let delivery_count =
        counted_bus.publish_from_excluding_kinds_with_delivery_count(None, notice(), &excluded);

    assert_eq!(detailed_trace.normalized(), counted_trace.normalized());
    assert_eq!(
        delivery_count.get(),
        report.delivered_to.len() + report.failed_deliveries.len(),
        "count-only routing includes successful, failed, and retired eligible recipients"
    );
    assert_eq!(counted_bus.last_route_work().report_id_clones, 0);
    assert_eq!(counted_bus.last_route_work().report_entries, 0);
}

/// Detailed broadcasts retain their exact per-vector ordering while using the
/// shared collector traversal instead of a separate routing loop.
#[test]
fn detailed_route_report_order_is_stable() {
    let (mut bus, _) = model_bus();
    let excluded = [ClientKind::Provider];
    let expected_failed = [
        test_connection_id("shared-retired"),
        test_connection_id("shared-failing"),
        test_connection_id("legacy-fail"),
    ];
    let expected_delivered = [
        test_connection_id("shared-admitted"),
        test_connection_id("legacy-ok"),
    ];

    let first = bus.publish_from_excluding_kinds(None, notice(), &excluded);
    for _ in 0..32 {
        let report = bus.publish_from_excluding_kinds(None, notice(), &excluded);
        assert_eq!(first, report);
        assert_eq!(report.delivered_to, expected_delivered);
        assert_eq!(
            report
                .failed_deliveries
                .iter()
                .map(|failure| &failure.connection_id)
                .collect::<Vec<_>>(),
            expected_failed.iter().collect::<Vec<_>>(),
        );
    }
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

/// A lazy event broadcast must not construct its payload when no
/// non-excluded subscriber selects the event.
#[test]
fn lazy_event_broadcast_skips_payload_without_candidate() {
    let (mut bus, trace) = model_bus();
    let built = Cell::new(false);

    bus.publish_event_from_excluding_kinds_lazy_without_report(
        None,
        HarnessOutputMessage::deliver(Event::AgentPromptCreated(tau_proto::AgentPromptCreated {
            agent_prompt_id: "ap-lazy".parse().expect("prompt"),
            agent_id: tau_proto::AgentId::parse("agent-lazy").expect("agent"),
            session_id: tau_proto::SessionId::parse("session-lazy").expect("session"),
            system_prompt: String::new(),
            context: Default::default(),
            tools: Vec::new(),
            tools_ref: None,
            hosted_tools: Vec::new(),
            model: "test/model".parse().expect("model"),
            model_params: Default::default(),
            tool_choice: Default::default(),
            originator: Default::default(),
            share_user_cache_key: false,
            ctx_id: None,
            compaction: None,
            operation: tau_proto::PromptOperation::Inference,
        })),
        &[ClientKind::Provider],
        || {
            built.set(true);
            notice()
        },
    );

    assert!(!built.get());
    assert!(trace.normalized().is_empty());
}

/// A lazy event broadcast must preserve the ordinary selector, visibility,
/// exclusion, shared-generation, failure, and legacy fanout decisions.
#[test]
fn lazy_event_broadcast_matches_eager_fanout() {
    let (mut eager_bus, eager_trace) = model_bus();
    let (mut lazy_bus, lazy_trace) = model_bus();
    let built = Cell::new(false);
    let excluded = [ClientKind::Provider];

    eager_bus.publish_from_excluding_kinds_without_report(None, notice(), &excluded);
    lazy_bus.publish_event_from_excluding_kinds_lazy_without_report(
        None,
        notice(),
        &excluded,
        || {
            built.set(true);
            notice()
        },
    );

    assert!(built.get());
    assert_eq!(eager_trace.normalized(), lazy_trace.normalized());
}

/// A lazy event broadcast must construct one exact projection before applying
/// arbitrary payload-sensitive visibility filters, then skip hidden delivery.
#[test]
fn lazy_event_broadcast_builds_once_when_all_candidates_are_hidden() {
    let mut bus = EventBus::new();
    let trace = DeliveryTrace::default();
    connect_model(
        &mut bus,
        &trace,
        Arc::new(HashSet::new()),
        ModelConnection {
            label: "hidden",
            kind: ClientKind::Ui,
            shared_target: None,
            fail_call: false,
            subscribed: true,
            visible: false,
        },
    );
    let built = Cell::new(false);

    bus.publish_event_from_excluding_kinds_lazy_without_report(None, notice(), &[], || {
        built.set(true);
        notice()
    });

    assert!(built.get());
    assert!(trace.normalized().is_empty());
}

/// A lazy event broadcast must evaluate payload-sensitive visibility against
/// the exact observer projection that it delivers.
#[test]
fn lazy_event_broadcast_filters_the_built_projection() {
    let mut bus = EventBus::new();
    let trace = DeliveryTrace::default();
    let id = test_connection_id("projection-filter");
    let connection = Connection::new(
        PendingConnectionMetadata {
            id: Some(id.clone()),
            name: test_extension_name("projection-filter"),
            kind: ClientKind::Ui,
            origin: ConnectionOrigin::InMemory,
        },
        Box::new(ModelSink {
            label: "projection-filter".to_owned(),
            shared_target: None,
            admitted_consumers: Arc::new(HashSet::new()),
            fail_call: false,
            trace: trace.clone(),
        }),
    )
    .with_visibility_filter(Box::new(|frame: &RoutedFrame| {
        matches!(
            frame.frame.delivered_event(),
            Some(Event::HarnessNotice(notice)) if notice.message == "observer projection"
        )
    }));
    bus.connect(connection);
    bus.set_subscriptions(
        &id,
        Vec::new(),
        vec![EventSelector::Prefix("harness.".to_owned())],
    )
    .expect("prefix subscription");

    bus.publish_event_from_excluding_kinds_lazy_without_report(
        None,
        notice_with_message("canonical payload"),
        &[],
        || notice_with_message("observer projection"),
    );

    assert_eq!(trace.normalized(), ["legacy:projection-filter".to_owned()]);
}

/// A lazy event broadcast must reject a builder result whose event identity
/// differs from the frame used to establish selector candidates.
#[test]
fn lazy_event_broadcast_rejects_mismatched_built_event() {
    let (mut bus, trace) = model_bus();

    bus.publish_event_from_excluding_kinds_lazy_without_report(None, notice(), &[], || {
        HarnessOutputMessage::deliver(Event::ToolStarted(tau_proto::ToolStarted {
            invocation_policy: tau_proto::ToolInvocationPolicy::default(),
            call_id: tau_proto::ToolCallId::from("call-mismatch"),
            agent_id: tau_proto::AgentId::parse("agent-mismatch").expect("agent id"),
            tool_name: tau_proto::ToolName::new("mismatch"),
            arguments: tau_proto::CborValue::Null,
            originator: Default::default(),
        }))
    });

    assert!(trace.normalized().is_empty());
}

/// A lazy event broadcast must reject a same-name projection that changes only
/// the replay marker carried by the admission delivery.
#[test]
fn lazy_event_broadcast_rejects_mismatched_replay_marker() {
    let (mut bus, trace) = model_bus();
    let recorded_at = tau_proto::UnixMicros::new(1_700_000_000_000_000);
    let admission = HarnessOutputMessage::deliver_live(
        recorded_at,
        Event::HarnessNotice(HarnessNotice {
            kind: "routing-model".to_owned(),
            message: "canonical payload".to_owned(),
            level: NoticeLevel::Info,
            purpose: NoticePurpose::Diagnostic,
        }),
    );

    bus.publish_event_from_excluding_kinds_lazy_without_report(None, admission, &[], || {
        HarnessOutputMessage::deliver_replay(
            recorded_at,
            Event::HarnessNotice(HarnessNotice {
                kind: "routing-model".to_owned(),
                message: "observer projection".to_owned(),
                level: NoticeLevel::Info,
                purpose: NoticePurpose::Diagnostic,
            }),
        )
    });

    assert!(trace.normalized().is_empty());
}

/// A lazy event broadcast must reject a same-name projection that changes only
/// the timestamp carried by the admission delivery.
#[test]
fn lazy_event_broadcast_rejects_mismatched_recorded_at() {
    let (mut bus, trace) = model_bus();
    let recorded_at = tau_proto::UnixMicros::new(1_700_000_000_000_000);
    let admission = HarnessOutputMessage::deliver_live(
        recorded_at,
        Event::HarnessNotice(HarnessNotice {
            kind: "routing-model".to_owned(),
            message: "canonical payload".to_owned(),
            level: NoticeLevel::Info,
            purpose: NoticePurpose::Diagnostic,
        }),
    );

    bus.publish_event_from_excluding_kinds_lazy_without_report(None, admission, &[], || {
        HarnessOutputMessage::deliver_live(
            tau_proto::UnixMicros::new(recorded_at.get() + 1),
            Event::HarnessNotice(HarnessNotice {
                kind: "routing-model".to_owned(),
                message: "observer projection".to_owned(),
                level: NoticeLevel::Info,
                purpose: NoticePurpose::Diagnostic,
            }),
        )
    });

    assert!(trace.normalized().is_empty());
}
