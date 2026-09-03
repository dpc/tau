use std::io::{self, Write};
use std::sync::{Arc, Mutex, mpsc};

use super::*;

/// Thread-safe trace capture for the private provider target.
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

/// Disabled provider observations perform no enabled-only clock or state work.
#[test]
fn disabled_provider_output_cost_observation_is_absent() {
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter("off")
        .without_time()
        .with_writer(io::sink)
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        assert!(SamplerObservation::enabled(false).is_none());
        assert!(WorkerQueueState::enabled().is_none());
        assert!(WorkerDrainObservation::enabled().is_none());
    });
}

/// Enabled sampler, worker queue, and drain observations use only fixed scalar
/// fields, one shared provider correlation, and balanced depth ownership.
#[test]
fn enabled_provider_output_cost_observations_are_content_free_and_correlated() {
    let trace = TraceWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter("provider-builtin.output-cost=trace")
        .without_time()
        .with_ansi(false)
        .with_writer({
            let trace = trace.clone();
            move || trace.clone()
        })
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let mut sampler = SamplerObservation::enabled(false).expect("enabled sampler");
        sampler.count_deltas(&[]);
        let state = WorkerQueueState::enabled().expect("enabled queue state");
        let (tx, rx) = mpsc::channel();
        let mut sink = crate::WorkerReportSink {
            tx,
            waker: SweepWaker,
            worker_output_depth: Some(Arc::clone(&state)),
            cancel_generation: 0,
            agent_prompt_id: tau_proto::AgentPromptId::parse("sampled-correlation")
                .expect("static prompt id"),
            cooldown_probe: None,
        };
        crate::ProviderReportSink::send_sampled_report(
            &mut sink,
            tau_proto::HarnessInputMessage::ConfigError(tau_proto::ConfigError {
                message: String::new(),
            }),
            Some(sampler),
        )
        .expect("sampled worker admission");
        let crate::WorkerMessage::Output {
            output_cost: Some(observation),
            ..
        } = rx.recv().expect("sampled output")
        else {
            panic!("sampled report must carry an observation");
        };
        assert_eq!(*state.depth.lock().expect("depth lock"), 1);
        observation.finish("dequeued");
        assert_eq!(*state.depth.lock().expect("depth lock"), 0);
        let mut drain = WorkerDrainObservation::enabled().expect("enabled drain");
        drain.message(true);
        drain.message(false);
    });
    let trace = String::from_utf8(trace.0.lock().expect("trace lock").clone()).expect("UTF-8");
    assert!(trace.contains("phase=\"sampler_materialization\""));
    let correlations = trace
        .lines()
        .filter(|line| {
            line.contains("phase=\"sampler_materialization\"")
                || line.contains("phase=\"worker_queue\"")
        })
        .filter_map(|line| {
            line.split_whitespace()
                .find_map(|field| field.strip_prefix("provider_correlation="))
        })
        .collect::<Vec<_>>();
    assert_eq!(correlations.len(), 2);
    assert_eq!(correlations[0], correlations[1]);
    assert!(trace.contains("phase=\"worker_queue\""));
    assert!(trace.contains("phase=\"worker_drain\""));
    assert!(trace.contains("batch_size=2"));
    assert!(trace.contains("output_batch_size=1"));
    assert!(!trace.contains("private-provider-output-canary"));
    let schemas = trace
        .lines()
        .filter(|line| line.contains("provider output cost observation"))
        .map(field_keys)
        .collect::<Vec<_>>();
    assert!(schemas.contains(&vec![
        "item_bytes",
        "item_count",
        "materialize_us",
        "outcome",
        "phase",
        "provider_correlation",
        "terminal",
    ]));
    assert!(schemas.contains(&vec![
        "admission_measure_us",
        "frame_bytes",
        "outcome",
        "phase",
        "provider_correlation",
        "queue_age_us",
        "queue_depth",
    ]));
    assert!(schemas.contains(&vec!["batch_size", "output_batch_size", "phase",]));
}

/// A disconnected worker channel records failed admission with zero queue
/// depth/age because no queue ownership ever began.
#[test]
fn queue_closed_observation_marks_queue_fields_not_applicable() {
    let trace = TraceWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter("provider-builtin.output-cost=trace")
        .without_time()
        .with_ansi(false)
        .with_writer({
            let trace = trace.clone();
            move || trace.clone()
        })
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let (tx, rx) = mpsc::channel();
        drop(rx);
        let mut sink = crate::WorkerReportSink {
            tx,
            waker: SweepWaker,
            worker_output_depth: WorkerQueueState::enabled(),
            cancel_generation: 0,
            agent_prompt_id: tau_proto::AgentPromptId::parse("queue-closed")
                .expect("static prompt id"),
            cooldown_probe: None,
        };
        assert!(
            crate::ProviderReportSink::send_report(
                &mut sink,
                tau_proto::HarnessInputMessage::ConfigError(tau_proto::ConfigError {
                    message: String::new(),
                }),
            )
            .is_err()
        );
    });
    let trace = String::from_utf8(trace.0.lock().expect("trace lock").clone()).expect("UTF-8");
    let line = trace
        .lines()
        .find(|line| line.contains("outcome=\"queue_closed\""))
        .expect("queue-closed observation");
    assert!(line.contains("queue_depth=0"));
    assert!(line.contains("queue_age_us=0"));
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

/// No-op wake used by the deterministic worker-count sweep.
#[derive(Clone, Copy)]
struct SweepWaker;

impl crate::WorkerReportWaker for SweepWaker {
    fn wake_provider_loop(&self) {}
}

/// The ignored provider release fixture exercises actual concurrent worker
/// report admission for 1/2/8 workers and the production queued-cancellation
/// arbitration test. It emits scalar CSV only.
#[test]
#[ignore = "deterministic provider output-cost release fixture"]
fn release_worker_and_cancellation_sweep() {
    use std::time::Instant;

    eprintln!("phase,workers,repetition,elapsed_ns,result");
    for workers in [1_usize, 2, 8] {
        for repetition in 0..5 {
            let (tx, rx) = mpsc::channel();
            let state = Arc::new(WorkerQueueState {
                admission: Mutex::new(()),
                depth: Mutex::new(0),
            });
            let started = Instant::now();
            std::thread::scope(|scope| {
                for _ in 0..workers {
                    let tx = tx.clone();
                    let state = Arc::clone(&state);
                    scope.spawn(move || {
                        let mut sink = crate::WorkerReportSink {
                            tx,
                            waker: SweepWaker,
                            worker_output_depth: Some(state),
                            cancel_generation: 0,
                            agent_prompt_id: tau_proto::AgentPromptId::parse("sweep-prompt")
                                .expect("static prompt id"),
                            cooldown_probe: None,
                        };
                        crate::ProviderReportSink::send_report(
                            &mut sink,
                            tau_proto::HarnessInputMessage::ConfigError(tau_proto::ConfigError {
                                message: String::new(),
                            }),
                        )
                        .expect("worker report admission");
                    });
                }
            });
            drop(tx);
            let admitted = rx.into_iter().count();
            assert_eq!(admitted, workers);
            assert_eq!(*state.depth.lock().expect("depth lock"), 0);
            eprintln!(
                "worker_admission,{workers},{repetition},{},passed",
                started.elapsed().as_nanos()
            );
        }
    }
    let cancellation_trace = TraceWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter("provider-builtin.output-cost=trace")
        .without_time()
        .with_ansi(false)
        .with_writer({
            let cancellation_trace = cancellation_trace.clone();
            move || cancellation_trace.clone()
        })
        .finish();
    let started = Instant::now();
    tracing::subscriber::with_default(subscriber, || {
        crate::openai_tests::targeted_cancel_between_output_enqueue_and_main_drain_is_terminal_once(
        );
    });
    let trace = String::from_utf8(
        cancellation_trace
            .0
            .lock()
            .expect("cancellation trace lock")
            .clone(),
    )
    .expect("UTF-8 cancellation trace");
    assert!(trace.contains("phase=\"worker_queue\""));
    assert!(trace.contains("outcome=\"dequeued\""));
    eprintln!(
        "queued_cancellation,1,0,{},passed",
        started.elapsed().as_nanos()
    );
}
