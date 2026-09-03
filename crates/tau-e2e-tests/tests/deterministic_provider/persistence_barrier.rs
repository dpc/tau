//! Coordinated observation of fixture-only persistence crash barriers.

mod protocol_reader;

use std::error::Error;
use std::fmt;
use std::os::fd::{AsFd as _, BorrowedFd};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::time::{Duration, Instant};

use protocol_reader::{ProtocolReadFailure, ProtocolReader, Readiness, wait_for_readiness};

use super::daemon_support::{DaemonGuard, OutputLengthCrashCut};

/// Maximum time the fixture allows the daemon to reach its selected hook.
const HOOK_REACH_TIMEOUT: Duration = Duration::from_secs(5);

/// Additional time allowed to transport an outcome after the producer's fixed
/// durability deadline.
const OUTCOME_TRANSPORT_GRACE: Duration = Duration::from_secs(2);

/// Distinguishable failure classes from the bounded crash-barrier protocol.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PersistenceBarrierFailureKind {
    /// The matching hook was absent through bounded fixture termination.
    HookNotReached,
    /// The producer reported that its durability deadline expired.
    ProducerDurabilityTimeout,
    /// The producer reported an unavailable or failed durability worker.
    ProducerDurabilityFailed,
    /// The daemon terminated before a complete producer outcome arrived.
    PrematureDaemonExit,
    /// The observer could not decode or transport the producer protocol.
    ObserverProtocolFailure,
}

/// One classified crash-barrier failure with timing and identity diagnostics.
#[derive(Debug)]
struct PersistenceBarrierFailure {
    /// Stable class used by focused regressions.
    kind: PersistenceBarrierFailureKind,
    /// Human-readable producer, timing, and daemon identity facts.
    diagnostic: String,
}

impl fmt::Display for PersistenceBarrierFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.diagnostic)
    }
}

impl Error for PersistenceBarrierFailure {}

/// Listener for one fixture-private producer outcome stream.
pub(super) struct PersistenceBarrier {
    /// Bound socket accepting the matching daemon hook.
    listener: UnixListener,
    /// Exact crash cut expected from the daemon.
    cut: OutputLengthCrashCut,
}

impl PersistenceBarrier {
    /// Binds an absent private socket before its daemon generation starts.
    pub(super) fn bind(path: &Path, cut: OutputLengthCrashCut) -> std::io::Result<Self> {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        let listener = UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        Ok(Self { listener, cut })
    }

    /// Waits for one classified producer outcome while also observing daemon
    /// exit.
    pub(super) fn wait(self, daemon: &mut DaemonGuard) -> Result<(), Box<dyn Error>> {
        let expected_pid = daemon.pid();
        let pidfd = daemon.pidfd().try_clone_to_owned()?;
        self.wait_with_process(
            expected_pid,
            pidfd.as_fd(),
            HOOK_REACH_TIMEOUT,
            tau_harness::output_length_test_barrier::PERSISTENCE_BARRIER_DURABILITY_TIMEOUT
                + OUTCOME_TRANSPORT_GRACE,
            || daemon.poll_exit_diagnostic(),
        )
        .map_err(Into::into)
    }

    /// Implements the protocol with injectable bounds and process readiness.
    fn wait_with_process(
        self,
        expected_pid: u32,
        process_fd: BorrowedFd<'_>,
        hook_timeout: Duration,
        outcome_timeout: Duration,
        mut process_exit: impl FnMut() -> Result<Option<String>, String>,
    ) -> Result<(), PersistenceBarrierFailure> {
        let observation_started = Instant::now();
        let hook_deadline = observation_started + hook_timeout;
        match wait_for_readiness(self.listener.as_fd(), process_fd, hook_deadline).map_err(
            |error| {
                self.failure(
                    PersistenceBarrierFailureKind::ObserverProtocolFailure,
                    expected_pid,
                    observation_started.elapsed(),
                    format!("producer outcome=unknown; hook readiness failed: {error}"),
                )
            },
        )? {
            Readiness::ProcessExit => {
                return Err(self.premature_exit(
                    &mut process_exit,
                    expected_pid,
                    observation_started.elapsed(),
                    "before-hook",
                ));
            }
            Readiness::DeadlineExpired => {
                return Err(self.failure(
                    PersistenceBarrierFailureKind::HookNotReached,
                    expected_pid,
                    observation_started.elapsed(),
                    format!(
                        "producer outcome=hook-not-reached; hook_timeout_ms={}",
                        hook_timeout.as_millis()
                    ),
                ));
            }
            Readiness::Socket => {}
        }
        let (stream, _) = self.listener.accept().map_err(|error| {
            self.failure(
                PersistenceBarrierFailureKind::ObserverProtocolFailure,
                expected_pid,
                observation_started.elapsed(),
                format!("producer outcome=unknown; accept failed after readiness: {error}"),
            )
        })?;
        stream.set_nonblocking(true).map_err(|error| {
            self.failure(
                PersistenceBarrierFailureKind::ObserverProtocolFailure,
                expected_pid,
                observation_started.elapsed(),
                format!("producer outcome=unknown; configure stream failed: {error}"),
            )
        })?;
        let hook_elapsed = observation_started.elapsed();
        let protocol_deadline = Instant::now() + outcome_timeout;
        let mut reader = ProtocolReader::new(stream);
        let hook = reader
            .read_line(process_fd, protocol_deadline)
            .map_err(|failure| {
                self.protocol_read_failure(
                    &mut process_exit,
                    expected_pid,
                    hook_elapsed,
                    "hook-identity",
                    outcome_timeout,
                    failure,
                )
            })?;
        let expected_hook = format!(
            "tau-persistence-barrier-v1 hook cut={} pid={} durability_timeout_ms={}",
            self.cut.protocol_name(),
            expected_pid,
            tau_harness::output_length_test_barrier::PERSISTENCE_BARRIER_DURABILITY_TIMEOUT
                .as_millis()
        );
        if hook != expected_hook {
            return Err(self.failure(
                PersistenceBarrierFailureKind::ObserverProtocolFailure,
                expected_pid,
                hook_elapsed,
                format!(
                    "producer outcome=unknown; phase=hook-identity; invalid={hook:?}; expected={expected_hook:?}; protocol_timeout_ms={}",
                    outcome_timeout.as_millis()
                ),
            ));
        }
        let outcome = reader
            .read_line(process_fd, protocol_deadline)
            .map_err(|failure| {
                self.protocol_read_failure(
                    &mut process_exit,
                    expected_pid,
                    hook_elapsed,
                    "producer-outcome",
                    outcome_timeout,
                    failure,
                )
            })?;
        let Some((outcome_name, producer_elapsed_ms)) = parse_outcome(&outcome) else {
            return Err(self.failure(
                PersistenceBarrierFailureKind::ObserverProtocolFailure,
                expected_pid,
                hook_elapsed,
                format!(
                    "producer outcome=unknown; phase=producer-outcome; invalid={outcome:?}; protocol_timeout_ms={}",
                    outcome_timeout.as_millis()
                ),
            ));
        };
        let facts = format!(
            "cut={} pid={} hook_elapsed_ms={} producer_elapsed_ms={} durability_timeout_ms={} protocol_timeout_ms={}",
            self.cut.protocol_name(),
            expected_pid,
            hook_elapsed.as_millis(),
            producer_elapsed_ms,
            tau_harness::output_length_test_barrier::PERSISTENCE_BARRIER_DURABILITY_TIMEOUT
                .as_millis(),
            outcome_timeout.as_millis()
        );
        match outcome_name {
            "durable" => Ok(()),
            "durability-timeout" => Err(PersistenceBarrierFailure {
                kind: PersistenceBarrierFailureKind::ProducerDurabilityTimeout,
                diagnostic: format!("producer outcome=durability-timeout; {facts}"),
            }),
            "durability-failed" => Err(PersistenceBarrierFailure {
                kind: PersistenceBarrierFailureKind::ProducerDurabilityFailed,
                diagnostic: format!("producer outcome=durability-failed; {facts}"),
            }),
            _ => Err(PersistenceBarrierFailure {
                kind: PersistenceBarrierFailureKind::ObserverProtocolFailure,
                diagnostic: format!(
                    "producer outcome=unknown; phase=producer-outcome; invalid outcome name={outcome_name:?}; {facts}"
                ),
            }),
        }
    }

    /// Converts a bounded protocol read failure into an observer or daemon
    /// class.
    fn protocol_read_failure(
        &self,
        process_exit: &mut impl FnMut() -> Result<Option<String>, String>,
        expected_pid: u32,
        hook_elapsed: Duration,
        phase: &str,
        protocol_timeout: Duration,
        failure: ProtocolReadFailure,
    ) -> PersistenceBarrierFailure {
        if matches!(failure, ProtocolReadFailure::ProcessExit) {
            return self.premature_exit(process_exit, expected_pid, hook_elapsed, phase);
        }
        self.failure(
            PersistenceBarrierFailureKind::ObserverProtocolFailure,
            expected_pid,
            hook_elapsed,
            format!(
                "producer outcome=unknown; phase={phase}; observer protocol failed: {failure}; protocol_timeout_ms={}",
                protocol_timeout.as_millis()
            ),
        )
    }

    /// Reads the terminal daemon status after its pidfd reports readiness.
    fn premature_exit(
        &self,
        process_exit: &mut impl FnMut() -> Result<Option<String>, String>,
        expected_pid: u32,
        hook_elapsed: Duration,
        phase: &str,
    ) -> PersistenceBarrierFailure {
        match process_exit() {
            Ok(Some(exit)) => self.failure(
                PersistenceBarrierFailureKind::PrematureDaemonExit,
                expected_pid,
                hook_elapsed,
                format!("producer outcome=premature-daemon-exit; phase={phase}; {exit}"),
            ),
            Ok(None) => self.failure(
                PersistenceBarrierFailureKind::ObserverProtocolFailure,
                expected_pid,
                hook_elapsed,
                format!(
                    "producer outcome=unknown; phase={phase}; pidfd ready without observable daemon status"
                ),
            ),
            Err(error) => self.failure(
                PersistenceBarrierFailureKind::ObserverProtocolFailure,
                expected_pid,
                hook_elapsed,
                format!("producer outcome=unknown; phase={phase}; daemon exit probe failed: {error}"),
            ),
        }
    }

    /// Builds one diagnostic with common cut, process, and observer timing
    /// facts.
    fn failure(
        &self,
        kind: PersistenceBarrierFailureKind,
        expected_pid: u32,
        hook_elapsed: Duration,
        detail: String,
    ) -> PersistenceBarrierFailure {
        PersistenceBarrierFailure {
            kind,
            diagnostic: format!(
                "persistence crash barrier failed: {detail}; cut={} pid={} hook_elapsed_ms={}",
                self.cut.protocol_name(),
                expected_pid,
                hook_elapsed.as_millis()
            ),
        }
    }
}

/// Parses the fixed producer outcome record into its name and elapsed time.
fn parse_outcome(line: &str) -> Option<(&str, u128)> {
    let suffix = line.strip_prefix("tau-persistence-barrier-v1 outcome=")?;
    let (outcome, elapsed) = suffix.split_once(" elapsed_ms=")?;
    Some((outcome, elapsed.parse().ok()?))
}

#[cfg(test)]
#[path = "persistence_barrier/tests.rs"]
mod tests;
