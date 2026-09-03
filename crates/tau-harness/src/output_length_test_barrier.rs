//! Test-only post-commit crash barriers for deterministic output-length replay.

#[cfg(not(unix))]
use std::io;
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Maximum time the producer waits for the persistence owner to drain debt.
pub const PERSISTENCE_BARRIER_DURABILITY_TIMEOUT: Duration = Duration::from_secs(5);

/// Exact durable cut at which the deterministic daemon must stop progressing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputLengthCommitCut {
    /// The planned response committed, before its steer is scheduled.
    AfterPlannedResponse,
    /// The continuation steer committed, before its owner is scheduled.
    AfterContinuationSteer,
    /// A typed receipt and the following sender terminal both committed.
    AfterTypedReceiptSenderTerminal,
    /// Two provider terminals committed in one resumed daemon.
    AfterNextProviderResponse,
}

impl OutputLengthCommitCut {
    /// Returns the stable fixture-protocol identity for this cut.
    fn protocol_name(self) -> &'static str {
        match self {
            Self::AfterPlannedResponse => "planned-response",
            Self::AfterContinuationSteer => "continuation-steer",
            Self::AfterTypedReceiptSenderTerminal => "typed-receipt-sender-terminal",
            Self::AfterNextProviderResponse => "next-provider-response",
        }
    }
}

/// Connected producer half of one fixture-private crash-barrier observation.
#[derive(Debug)]
struct PersistenceBarrierProducer {
    /// Stream carrying hook identity followed by the producer outcome.
    stream: UnixStream,
}

/// One installed cut and its fixture-private outcome socket.
#[derive(Debug)]
struct Barrier {
    /// Exact admission-complete boundary whose persistence debt is drained
    /// first.
    cut: OutputLengthCommitCut,
    /// Bound observer socket that receives hook identity and producer outcome.
    reached_path: PathBuf,
    /// Whether the typed-receipt cut observed its inbound durable fact.
    receipt_seen: bool,
    /// Provider terminal count for the resumed two-response cut.
    response_count: u8,
}

static BARRIER: OnceLock<Mutex<Option<Barrier>>> = OnceLock::new();

/// Install the process-local one-shot barrier before starting the test daemon.
///
/// # Errors
///
/// Returns an error if another barrier is already installed.
pub fn install(cut: OutputLengthCommitCut, reached_path: PathBuf) -> Result<(), &'static str> {
    let mut barrier = BARRIER
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| "output-length test barrier lock poisoned")?;
    if barrier.is_some() {
        return Err("output-length test barrier already installed");
    }
    *barrier = Some(Barrier {
        cut,
        reached_path,
        receipt_seen: false,
        response_count: 0,
    });
    Ok(())
}

/// Advances the typed-receipt cut and reports when its sender terminal closes
/// it.
pub(crate) fn observe_typed_receipt(event: &tau_proto::Event) -> Option<OutputLengthCommitCut> {
    let mut installed = BARRIER
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("typed-receipt test barrier lock");
    let barrier = installed.as_mut()?;
    if barrier.cut == OutputLengthCommitCut::AfterNextProviderResponse {
        if matches!(event, tau_proto::Event::ProviderResponseFinished(_)) {
            barrier.response_count += 1;
        }
        return (barrier.response_count == 1).then_some(barrier.cut);
    }
    if barrier.cut != OutputLengthCommitCut::AfterTypedReceiptSenderTerminal {
        return None;
    }
    if matches!(event, tau_proto::Event::AgentMessageReceived(_)) {
        barrier.receipt_seen = true;
        return None;
    }
    (barrier.receipt_seen && matches!(event, tau_proto::Event::ProviderResponseFinished(_)))
        .then_some(barrier.cut)
}

/// Preserves the existing durability assertion while reporting its producer
/// outcome to a matching fixture observer.
pub(crate) fn wait_and_reach(
    cut: OutputLengthCommitCut,
    wait: impl FnOnce(Duration) -> tau_core::DurabilityBarrierOutcome,
    failure_message: &str,
) {
    let (producer, outcome) = wait_and_report(cut, wait);
    assert_durable(outcome, failure_message);
    if let Some(producer) = producer {
        producer.block();
    }
}

/// Runs the typed durability wait and sends its outcome before assertion or
/// event-loop blocking.
fn wait_and_report(
    cut: OutputLengthCommitCut,
    wait: impl FnOnce(Duration) -> tau_core::DurabilityBarrierOutcome,
) -> (
    Option<PersistenceBarrierProducer>,
    tau_core::DurabilityBarrierOutcome,
) {
    let mut producer = begin(cut);
    let started = Instant::now();
    let outcome = wait(PERSISTENCE_BARRIER_DURABILITY_TIMEOUT);
    if let Some(producer) = producer.as_mut() {
        producer
            .report(outcome, started.elapsed())
            .expect("report persistence crash-barrier producer outcome");
    }
    (producer, outcome)
}

/// Applies the preserved producer durability assertion after its report.
fn assert_durable(outcome: tau_core::DurabilityBarrierOutcome, failure_message: &str) {
    assert!(
        outcome == tau_core::DurabilityBarrierOutcome::Durable,
        "{failure_message}"
    );
}

/// Connects the matching one-shot barrier and reports that its hook was
/// reached.
fn begin(cut: OutputLengthCommitCut) -> Option<PersistenceBarrierProducer> {
    let mut installed = BARRIER
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("output-length test barrier lock");
    if !installed.as_ref().is_some_and(|barrier| barrier.cut == cut) {
        return None;
    }
    let barrier = installed.take().expect("matching barrier is installed");
    drop(installed);
    Some(
        PersistenceBarrierProducer::connect(&barrier.reached_path, cut)
            .expect("connect persistence crash-barrier observer"),
    )
}

impl PersistenceBarrierProducer {
    /// Connects to the fixture observer and sends immutable producer identity.
    fn connect(path: &Path, cut: OutputLengthCommitCut) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            let mut stream = UnixStream::connect(path)?;
            writeln!(
                stream,
                "tau-persistence-barrier-v1 hook cut={} pid={} durability_timeout_ms={}",
                cut.protocol_name(),
                std::process::id(),
                PERSISTENCE_BARRIER_DURABILITY_TIMEOUT.as_millis()
            )?;
            Ok(Self { stream })
        }
        #[cfg(not(unix))]
        {
            let _ = (path, cut);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "test barrier requires Unix sockets",
            ))
        }
    }

    /// Sends the producer's durability result and measured bounded wait time.
    fn report(
        &mut self,
        outcome: tau_core::DurabilityBarrierOutcome,
        elapsed: Duration,
    ) -> std::io::Result<()> {
        use tau_core::DurabilityBarrierOutcome;

        let outcome = match outcome {
            DurabilityBarrierOutcome::Durable => "durable",
            DurabilityBarrierOutcome::DeadlineExpired => "durability-timeout",
            DurabilityBarrierOutcome::UnavailableOrFailed => "durability-failed",
        };
        writeln!(
            self.stream,
            "tau-persistence-barrier-v1 outcome={outcome} elapsed_ms={}",
            elapsed.as_millis()
        )
    }

    /// Stops the event-loop thread at the durable crash cut until process
    /// teardown.
    fn block(self) -> ! {
        drop(self.stream);
        loop {
            std::thread::park();
        }
    }
}

#[cfg(test)]
#[path = "output_length_test_barrier/tests.rs"]
mod tests;
