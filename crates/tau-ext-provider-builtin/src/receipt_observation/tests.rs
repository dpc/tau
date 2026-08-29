use super::*;
use crate::tests::SharedTraceWriter;

/// The production trace must retain one fixed scalar schema and must not
/// expose representative prompt, model, profile, Secret, path, account,
/// cursor, or error values through fields or Debug output.
#[test]
fn receipt_trace_is_fixed_cardinality_and_content_free() {
    let output = SharedTraceWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .without_time()
        .with_ansi(false)
        .with_writer({
            let output = output.clone();
            move || output.clone()
        })
        .finish();
    let canaries = [
        "prompt-content-canary",
        "model-canary",
        "profile-canary",
        "secret-canary",
        "/private/path-canary",
        "account-canary",
        "cursor-canary",
        "error-canary",
    ];
    let mut observation = ReceiptObservation::new(LocalInputObservation {
        frame_bytes: tau_proto::ProtocolMessageBytes::new(73).expect("nonzero frame"),
        decode_elapsed: Duration::from_micros(2),
        decoded_at: Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("one-second monotonic history"),
    });
    observation.handler_started();
    observation.handler_materialized();
    observation.handler_dispatched();
    let settings_started = Instant::now();
    std::thread::sleep(Duration::from_millis(1));
    observation.settings_cloned(settings_started.elapsed(), 2);
    observation.secret_started();
    observation.secret_finished(41);
    let quota_started = Instant::now();
    std::thread::sleep(Duration::from_millis(1));
    observation.quota_resolved(quota_started.elapsed());
    observation.queued(4);
    observation.spawning();
    let debug = format!("{observation:?}");
    tracing::subscriber::with_default(subscriber, || observation.worker_started());
    let trace = String::from_utf8(output.bytes()).expect("UTF-8 trace");

    for canary in canaries {
        assert!(!trace.contains(canary));
        assert!(!debug.contains(canary));
    }
    let ordered_fields = [
        "frame_bytes=73",
        "frame_read_decode_us=2",
        "reader_queue_us=",
        "handler_clone_us=",
        "handler_dispatch_us=",
        "settings_clone_us=",
        "profile_count=2",
        "secret_rpc_count=1",
        "secret_bytes=41",
        "secret_wait_us=",
        "oauth_class=\"none\"",
        "oauth_us=0",
        "quota_us=",
        "cooldown_queue_us=0",
        "cooldown_depth=0",
        "slot_queue_us=",
        "slot_depth=4",
        "spawn_us=",
        "stage_accounted_us=",
        "unattributed_us=",
        "receipt_to_worker_us=",
        "outcome=\"started\"",
    ];
    let mut previous = 0;
    for field in ordered_fields {
        let index = trace.find(field).unwrap_or_else(|| {
            panic!("missing field {field:?} in {trace}");
        });
        assert!(index >= previous, "field order changed at {field}: {trace}");
        previous = index;
    }
    assert_eq!(trace.matches("provider receipt observation").count(), 1);
    let scalar = |name: &str| {
        let value = trace
            .split_whitespace()
            .find_map(|field| field.strip_prefix(name))
            .unwrap_or_else(|| panic!("missing scalar {name} in {trace}"));
        value
            .trim_end_matches(|character: char| !character.is_ascii_digit())
            .parse::<u64>()
            .unwrap_or_else(|_| panic!("invalid scalar {name}{value}"))
    };
    assert_eq!(
        scalar("stage_accounted_us=") + scalar("unattributed_us="),
        scalar("receipt_to_worker_us="),
        "disjoint stage decomposition changed: {trace}"
    );
}

/// Closed OAuth and cancellation paths must use bounded classes rather than
/// rendering their source error, account, credential, or cancellation data.
#[test]
fn receipt_trace_uses_closed_oauth_and_terminal_classes() {
    let output = SharedTraceWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .without_time()
        .with_ansi(false)
        .with_writer({
            let output = output.clone();
            move || output.clone()
        })
        .finish();
    let mut observation = ReceiptObservation::new(LocalInputObservation {
        frame_bytes: tau_proto::ProtocolMessageBytes::new(1).expect("nonzero frame"),
        decode_elapsed: Duration::ZERO,
        decoded_at: Instant::now(),
    });
    observation.oauth_started();
    observation.oauth_failed();
    tracing::subscriber::with_default(subscriber, || {
        observation.finished_before_worker(ReceiptOutcome::Canceled);
    });
    let trace = String::from_utf8(output.bytes()).expect("UTF-8 trace");

    assert!(trace.contains("oauth_class=\"failed\""));
    assert!(trace.contains("outcome=\"canceled\""));
    assert!(!trace.contains("oauth-refresh-error-canary"));
}
