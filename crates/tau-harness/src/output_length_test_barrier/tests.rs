use std::io::{BufRead as _, BufReader};
use std::os::unix::net::UnixListener;

use tau_core::DurabilityBarrierOutcome;

use super::*;

/// Keeps stable cut names aligned with the external deterministic fixture.
#[test]
fn persistence_barrier_cut_protocol_names_are_stable() {
    assert_eq!(
        OutputLengthCommitCut::AfterPlannedResponse.protocol_name(),
        "planned-response"
    );
    assert_eq!(
        OutputLengthCommitCut::AfterContinuationSteer.protocol_name(),
        "continuation-steer"
    );
    assert_eq!(
        OutputLengthCommitCut::AfterTypedReceiptSenderTerminal.protocol_name(),
        "typed-receipt-sender-terminal"
    );
    assert_eq!(
        OutputLengthCommitCut::AfterNextProviderResponse.protocol_name(),
        "next-provider-response"
    );
}

/// Exercises the real producer mapping and proves failure records are visible
/// before the preserved durability assertion fires.
#[test]
fn persistence_barrier_producer_reports_every_typed_outcome_before_assertion() {
    for (outcome, expected_wire, assertion_fails) in [
        (DurabilityBarrierOutcome::Durable, "durable", false),
        (
            DurabilityBarrierOutcome::DeadlineExpired,
            "durability-timeout",
            true,
        ),
        (
            DurabilityBarrierOutcome::UnavailableOrFailed,
            "durability-failed",
            true,
        ),
    ] {
        let tempdir = tempfile::TempDir::new().expect("tempdir");
        let path = tempdir.path().join("barrier.sock");
        let listener = UnixListener::bind(&path).expect("bind observer");
        install(OutputLengthCommitCut::AfterPlannedResponse, path).expect("install barrier");
        let (producer, reported) =
            wait_and_report(OutputLengthCommitCut::AfterPlannedResponse, |_| outcome);
        let (stream, _) = listener.accept().expect("accept producer");
        let mut lines = BufReader::new(stream).lines();
        let hook = lines.next().expect("hook line").expect("read hook");
        let result = lines.next().expect("outcome line").expect("read outcome");
        assert!(hook.contains("cut=planned-response"), "{hook}");
        assert!(
            hook.contains(&format!("pid={}", std::process::id())),
            "{hook}"
        );
        assert!(hook.contains("durability_timeout_ms=5000"), "{hook}");
        assert!(
            result.contains(&format!("outcome={expected_wire}")),
            "{result}"
        );
        assert!(result.contains("elapsed_ms="), "{result}");
        let assertion = std::panic::catch_unwind(|| {
            assert_durable(reported, "preserved durability assertion");
        });
        assert_eq!(assertion.is_err(), assertion_fails);
        drop(producer);
    }
}
