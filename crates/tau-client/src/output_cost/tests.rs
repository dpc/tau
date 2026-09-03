use std::io::{self, Write};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use super::*;
use crate::writer_thread::WriterCommand;

/// Thread-safe trace writer used to inspect the private scalar schema.
#[derive(Clone, Default)]
struct TraceWriter(Arc<Mutex<Vec<u8>>>);

impl Write for TraceWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("trace lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Disabled diagnostics must not read clocks or construct observation state.
#[test]
fn disabled_output_cost_observation_is_absent() {
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter("off")
        .without_time()
        .with_writer(io::sink)
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        assert!(OutputCostObservation::start().is_none());
    });
}

/// Enabled observations retain fixed scalar fields, correlate privately, and
/// omit a content canary.
#[test]
fn enabled_output_cost_observation_is_correlated_and_content_free() {
    let trace = TraceWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter("tau_client::output_cost=trace")
        .without_time()
        .with_ansi(false)
        .with_writer({
            let trace = trace.clone();
            move || trace.clone()
        })
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let started = OutputCostObservation::start();
        let observation =
            OutputCostObservation::measured(started, 17).expect("enabled observation");
        observation.begin_admission(OutputLane::Detached).admitted();
        observation.writer_started();
        observation.finish(
            Duration::from_micros(3),
            Duration::from_micros(5),
            "written",
        );
    });
    let trace = String::from_utf8(trace.0.lock().expect("trace lock").clone()).expect("UTF-8");
    assert!(trace.contains("client_correlation="));
    assert!(trace.contains("frame_bytes=17"));
    assert!(trace.contains("lane=\"detached\""));
    assert!(!trace.contains("private-output-canary"));
    let line = trace
        .lines()
        .find(|line| line.contains("client output cost observation"))
        .expect("one observation");
    assert_eq!(
        field_keys(line),
        [
            "admission_us",
            "client_correlation",
            "encode_us",
            "flush_us",
            "frame_bytes",
            "lane",
            "measure_us",
            "outcome",
            "writer_wait_us",
        ]
    );
}

/// Return the sorted trace application-field allowlist.
fn field_keys(line: &str) -> Vec<&str> {
    let mut keys = line
        .split_whitespace()
        .filter_map(|token| token.split_once('=').map(|(key, _)| key))
        .collect::<Vec<_>>();
    keys.sort_unstable();
    keys
}

/// The production writer path closes wait, encode, and flush phases for one
/// measured typed frame without changing its wire bytes.
#[test]
fn production_writer_emits_complete_output_cost_observation() {
    let trace = TraceWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter("tau_client::output_cost=trace")
        .without_time()
        .with_ansi(false)
        .with_writer({
            let trace = trace.clone();
            move || trace.clone()
        })
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let (sender, receiver) = crate::writer_thread::writer_channel();
        let dispatcher = tracing::dispatcher::get_default(Clone::clone);
        let writer = std::thread::spawn(move || {
            tracing::dispatcher::with_default(&dispatcher, || {
                crate::writer_thread::run_writer(Vec::new(), receiver)
            })
        });
        let output = crate::PeerOutput::prepare(tau_proto::HarnessInputMessage::ConfigError(
            tau_proto::ConfigError {
                message: "content-free fixture".to_owned(),
            },
        ))
        .expect("prepare output");
        let admission = output
            .begin_admission(OutputLane::Synchronous)
            .expect("enabled admission");
        let (ack_tx, ack_rx) = mpsc::channel();
        sender
            .send_observed(WriterCommand::Send(output, ack_tx), Some(admission))
            .expect("queue writer command");
        ack_rx.recv().expect("writer ack").expect("writer success");
        drop(sender);
        writer.join().expect("join writer").expect("writer exit");
    });
    let trace = String::from_utf8(trace.0.lock().expect("trace lock").clone()).expect("UTF-8");
    assert!(trace.contains("outcome=\"written\""));
    assert!(trace.contains("measure_us="));
    assert!(trace.contains("admission_us="));
    assert!(trace.contains("writer_wait_us="));
    assert!(trace.contains("encode_us="));
    assert!(trace.contains("flush_us="));
    assert!(!trace.contains("content-free fixture"));
}

/// The ignored release sweep fixes sizes, queue slots, worker counts,
/// cancellation, blocked-transport applicability, and repetitions. It emits
/// redacted CSV only; timings are descriptive rather than pass/fail oracles.
#[test]
#[ignore = "deterministic release measurement fixture"]
fn release_output_cost_sweep() {
    use std::hint::black_box;

    const REPETITIONS: usize = 5;
    eprintln!("phase,payload_bytes,repetition,encoded_bytes,elapsed_ns,result");
    let sizes = [
        0_usize,
        64,
        4 * 1024,
        256 * 1024,
        (8 * 1024 * 1024) - 45,
        (8 * 1024 * 1024) - 44,
        (8 * 1024 * 1024) - 43,
    ];
    for payload_bytes in sizes {
        for repetition in 0..REPETITIONS {
            let message = tau_proto::HarnessInputMessage::ConfigError(tau_proto::ConfigError {
                message: "x".repeat(payload_bytes),
            });
            let started = Instant::now();
            let output = crate::PeerOutput::prepare(black_box(message))
                .expect("content-free fixture encodes");
            let elapsed = started.elapsed().as_nanos();
            let result = if output.encoded_bytes() <= crate::MAX_OUTBOUND_FRAME_BYTES {
                "admissible"
            } else {
                "overloaded"
            };
            eprintln!(
                "frame_measure,{payload_bytes},{repetition},{},{elapsed},{result}",
                output.encoded_bytes()
            );
        }
    }
    let started = Instant::now();
    crate::tests::detached_fifo_item_limit_and_blocked_writer_recovery();
    eprintln!(
        "detached_slots_and_blocked,0,0,0,{},passed_64_slots_blocked_transport",
        started.elapsed().as_nanos()
    );
    let started = Instant::now();
    crate::tests::detached_fifo_byte_limit_and_individual_frame_limit_are_exact();
    eprintln!(
        "exact_boundary,8388608,0,8388608,{},passed_below_equal_above",
        started.elapsed().as_nanos()
    );
}

/// The large-payload fixture's CBOR envelope is 44 bytes, so the three release
/// cases exercise exact below/equal/above individual-frame admission.
#[test]
fn release_sweep_hits_exact_eight_mib_boundary() {
    for (payload_bytes, expected) in [
        ((8 * 1024 * 1024) - 45, crate::MAX_OUTBOUND_FRAME_BYTES - 1),
        ((8 * 1024 * 1024) - 44, crate::MAX_OUTBOUND_FRAME_BYTES),
        ((8 * 1024 * 1024) - 43, crate::MAX_OUTBOUND_FRAME_BYTES + 1),
    ] {
        let output = crate::PeerOutput::prepare(tau_proto::HarnessInputMessage::ConfigError(
            tau_proto::ConfigError {
                message: "x".repeat(payload_bytes),
            },
        ))
        .expect("boundary fixture encodes");
        assert_eq!(output.encoded_bytes(), expected);
    }
}
