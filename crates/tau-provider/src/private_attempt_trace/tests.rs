use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt as _};
use tracing_subscriber::{Layer, Registry};

use super::*;

/// Exact captured field projection for one production trace event.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CapturedField {
    /// Static field name from the callsite metadata.
    name: &'static str,
    /// Scalar visitor method selected by tracing.
    kind: &'static str,
    /// String/debug value, retained only in this privacy test.
    value: Option<String>,
}

/// One event captured without depending on formatter spelling.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CapturedEvent {
    /// Static tracing target.
    target: &'static str,
    /// Static tracing level.
    level: Level,
    /// Fields in callsite declaration order.
    fields: Vec<CapturedField>,
}

/// Exact event collector used to reject added or dynamically typed fields.
#[derive(Clone, Default)]
struct CaptureLayer(Arc<Mutex<Vec<CapturedEvent>>>);

impl<S: Subscriber> Layer<S> for CaptureLayer {
    /// Capture each event's static metadata and exact typed field set.
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        self.0.lock().expect("capture lock").push(CapturedEvent {
            target: event.metadata().target(),
            level: *event.metadata().level(),
            fields: visitor.fields,
        });
    }
}

/// Typed tracing field visitor.
#[derive(Default)]
struct FieldVisitor {
    /// Fields collected in declaration order.
    fields: Vec<CapturedField>,
}

impl FieldVisitor {
    /// Append one typed field.
    fn push(&mut self, field: &Field, kind: &'static str, value: Option<String>) {
        self.fields.push(CapturedField {
            name: field.name(),
            kind,
            value,
        });
    }
}

impl Visit for FieldVisitor {
    /// Capture unsigned scalar fields.
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.push(field, "u64", Some(value.to_string()));
    }

    /// Capture boolean scalar fields.
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.push(field, "bool", Some(value.to_string()));
    }

    /// Capture string-class fields.
    fn record_str(&mut self, field: &Field, value: &str) {
        self.push(field, "str", Some(value.to_owned()));
    }

    /// Capture the static message field and reject any unexpected debug field.
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.push(field, "debug", Some(format!("{value:?}")));
    }
}

/// The disabled target must select no observation state; this prevents
/// accidental clocks, byte sizing, allocations, or retained per-attempt state.
#[test]
fn disabled_target_selects_no_attempt_state() {
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        assert!(AttemptTrace::selected(Backend::Codex, Transport::Websocket).is_none());
    });
}

/// The production callsite exposes one exact fixed scalar/class schema and no
/// field capable of acquiring a prompt, identifier, endpoint, or raw error.
#[test]
fn enabled_trace_has_exact_content_free_schema() {
    let capture = CaptureLayer::default();
    let subscriber = Registry::default().with(capture.clone());
    tracing::subscriber::with_default(subscriber, || {
        let mut trace = AttemptTrace::selected(Backend::PublicResponses, Transport::HttpSse)
            .expect("TRACE target enabled");
        trace.lowering_finished();
        trace.serialization_finished(Instant::now(), 73);
        trace.capture_finished(Instant::now());
        trace.record_dispatch();
        trace.enqueue_finished(Instant::now());
        trace.first_input(41);
        trace.decoded(Instant::now(), true);
        trace.finish(Outcome::Completed);
    });
    let events = capture.0.lock().expect("capture lock");
    assert_eq!(events.len(), 1, "one finite attempt emits exactly once");
    let event = &events[0];
    assert_eq!(event.target, LOG_TARGET);
    assert_eq!(event.level, Level::TRACE);
    let expected = [
        ("message", "debug"),
        ("backend", "str"),
        ("transport", "str"),
        ("lowering_us", "u64"),
        ("serialization_us", "u64"),
        ("capture_us", "u64"),
        ("pool_wait_us", "u64"),
        ("connect_upgrade_us", "u64"),
        ("enqueue_us", "u64"),
        ("first_input_us", "u64"),
        ("decode_us", "u64"),
        ("first_semantic_us", "u64"),
        ("request_bytes_total", "u64"),
        ("first_input_bytes", "u64"),
        ("dispatch_count", "u64"),
        ("decode_count", "u64"),
        ("first_input_seen", "bool"),
        ("first_semantic_seen", "bool"),
        ("stage_accounted_us", "u64"),
        ("unattributed_us", "u64"),
        ("total_us", "u64"),
        ("outcome", "str"),
    ];
    let actual = event
        .fields
        .iter()
        .map(|field| (field.name, field.kind))
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
    let rendered_values = event
        .fields
        .iter()
        .filter_map(|field| field.value.as_deref())
        .collect::<Vec<_>>()
        .join(" ");
    for canary in [
        "private prompt",
        "model/account",
        "https://",
        "Bearer",
        "raw error",
    ] {
        assert!(!rendered_values.contains(canary));
    }
}
