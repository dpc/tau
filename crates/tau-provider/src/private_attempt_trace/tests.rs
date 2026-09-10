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

/// Capture selection may retain the existing fixed scalar state without
/// enabling the identity-free TRACE sink or allocating per-event storage.
#[test]
fn capture_selection_uses_small_fixed_state_without_trace_output() {
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let mut trace =
            AttemptTrace::selected_for_capture(Backend::Codex, Transport::Websocket, true)
                .expect("capture selects state");
        let state_bytes = std::mem::size_of::<AttemptTrace>();
        assert!(
            state_bytes <= 384,
            "attempt timing state unexpectedly grew to {state_bytes} bytes"
        );
        trace.record_dispatch();
        trace.first_input(0);
        trace.terminal();
        let timing = trace.finish_with_timing(Outcome::Completed);
        assert!(timing.dispatch_to_first_input_us.is_some());
        assert!(timing.dispatch_to_terminal_us.is_some());
        assert_eq!(timing.dispatch_count, 1);
    });
}

/// Transparent repair must keep legacy TRACE first-input state while the timing
/// capture reports final-dispatch coverage and bytes consistently.
#[test]
fn repair_resets_final_dispatch_input_without_rewriting_legacy_first_input() {
    let mut trace = AttemptTrace::selected_for_capture(Backend::Codex, Transport::Websocket, true)
        .expect("capture selects state");
    trace.record_dispatch();
    trace.first_input(41);
    trace.text_message_read(Instant::now());
    trace.associated_message_read(Instant::now());
    trace.decoded_payload();
    trace.semantic_qualified();
    trace.record_dispatch();
    let timing = trace.finish_with_timing(Outcome::Failed);
    assert_eq!(timing.dispatch_count, 2);
    assert_eq!(timing.dispatch_to_first_input_us, None);
    assert_eq!(timing.first_input_bytes, 0);
    assert_eq!(timing.dispatch_to_first_semantic_us, None);
    assert_eq!(timing.text_message_read, None);
    assert_eq!(timing.associated_message_read, None);
    assert_eq!(timing.dispatch_to_first_decoded_payload_us, None);
    assert!(timing.final_dispatch_us.is_some());
}

/// Predispatch read times must remain unavailable rather than look like an
/// instantaneous response; first-seen paired samples must never be overwritten.
#[test]
fn message_read_boundaries_preserve_pairs_and_reject_predispatch_samples() {
    let mut trace = AttemptTrace::selected_for_capture(Backend::Codex, Transport::Websocket, true)
        .expect("capture selects state");
    let before_dispatch = Instant::now() - Duration::from_secs(1);
    trace.text_message_read(before_dispatch);
    trace.decoded_payload();
    assert!(trace.final_dispatch.text_message_read.is_none());
    assert!(trace.final_dispatch.first_decoded_payload_us.is_none());
    trace.record_dispatch();
    trace.text_message_read(before_dispatch);
    trace.associated_message_read(before_dispatch);
    assert!(trace.final_dispatch.text_message_read.is_none());
    assert!(trace.final_dispatch.associated_message_read.is_none());
    let read_at = Instant::now();
    trace.text_message_read(read_at);
    trace.associated_message_read(read_at);
    trace.decoded_payload();
    let first = trace.final_dispatch.text_message_read;
    let associated = trace.final_dispatch.associated_message_read;
    let decoded = trace.final_dispatch.first_decoded_payload_us;
    trace.text_message_read(Instant::now());
    trace.associated_message_read(Instant::now());
    trace.decoded_payload();
    let timing = trace.finish_with_timing(Outcome::Completed);
    assert_eq!(timing.text_message_read, first);
    assert_eq!(timing.associated_message_read, associated);
    assert_eq!(timing.dispatch_to_first_decoded_payload_us, decoded);
    assert_eq!(
        first.expect("read pair").0,
        associated.expect("associated pair").0
    );
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
