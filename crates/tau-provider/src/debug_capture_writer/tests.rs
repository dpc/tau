use std::io;
use std::sync::{Arc, Mutex, mpsc};

use super::{
    CaptureJob, CaptureQueue, ProviderDebugCapture, capture_fits_raw_bound, compressed_message,
    enforce_encoded_bound, run_worker, start_transport_with,
};

/// Build one typed capture job for worker and queue tests.
fn job(prompt: &str, json: &[u8]) -> CaptureJob {
    CaptureJob::new(ProviderDebugCapture::new(
        tau_proto::SessionId::parse("session-test").expect("session"),
        tau_proto::AgentPromptId::parse(prompt).expect("prompt"),
        tau_proto::ProviderDebugCaptureClass::HttpSseRequest,
        json.to_vec(),
    ))
}

/// Proves full queues reject new captures immediately rather than waiting for
/// worker progress.
#[test]
fn overload_drops_new_capture_without_blocking() {
    let (sender, _receiver) = mpsc::sync_channel(1);
    let queue = CaptureQueue::with_sender(sender);
    queue.try_submit(job("one", b"one")).expect("first job");
    assert!(matches!(
        queue.try_submit(job("two", b"two")),
        Err(mpsc::TrySendError::Full(_))
    ));
}

/// Proves one transport failure does not stop later accepted captures.
#[test]
fn transport_failure_isolated_from_later_capture() {
    let (sender, receiver) = mpsc::sync_channel(2);
    sender.try_send(job("one", b"one")).expect("first");
    sender.try_send(job("two", b"two")).expect("second");
    drop(sender);
    let attempted = Arc::new(Mutex::new(Vec::new()));
    let worker_attempted = Arc::clone(&attempted);
    run_worker(receiver, move |job| {
        let prompt = job.capture.agent_prompt_id.as_str().to_owned();
        worker_attempted
            .lock()
            .expect("attempts")
            .push(prompt.clone());
        if prompt == "one" {
            Err(io::Error::other("synthetic failure"))
        } else {
            Ok(())
        }
    });
    assert_eq!(*attempted.lock().expect("attempts"), ["one", "two"]);
}

/// Proves the shared production constructor accepts admissions 1 through 64
/// and rejects admission 65 without blocking.
#[test]
fn production_queue_capacity_is_enforced() {
    assert_eq!(super::CAPTURE_QUEUE_CAPACITY, 64);
    let (sender, _receiver) = mpsc::sync_channel(super::CAPTURE_QUEUE_CAPACITY);
    let queue = CaptureQueue::with_sender(sender);
    for index in 1..=64 {
        queue
            .try_submit(job(&format!("prompt-{index}"), b"capture"))
            .unwrap_or_else(|_| panic!("admission {index} within production capacity"));
    }
    assert!(matches!(
        queue.try_submit(job("prompt-65", b"capture")),
        Err(mpsc::TrySendError::Full(_))
    ));
}

/// Proves the Provider compresses the exact JSON and preserves only structured
/// attribution in the dedicated non-event protocol message.
#[test]
fn compression_builds_opaque_attributed_protocol_message() {
    let json = br#"{"secret":"debug"}"#;
    let message = compressed_message(&job("prompt", json)).expect("compress");
    let tau_proto::HarnessInputMessage::ProviderDebugCapture(capture) = message else {
        panic!("dedicated capture message");
    };
    assert_eq!(capture.session_id.as_str(), "session-test");
    assert_eq!(capture.agent_prompt_id.as_str(), "prompt");
    assert_eq!(
        zstd::stream::decode_all(&capture.zstd[..]).expect("decode"),
        json
    );
    assert!(!format!("{capture:?}").contains("secret"));
}

/// Compact HTTP failure evidence must use the same zstd-compressed,
/// non-journaled harness-provider transport as every other private capture.
#[test]
fn compact_failure_uses_shared_zstd_transport() {
    let capture = ProviderDebugCapture::new(
        tau_proto::SessionId::parse("session-test").expect("session"),
        tau_proto::AgentPromptId::parse("compact-prompt").expect("prompt"),
        tau_proto::ProviderDebugCaptureClass::CompactHttpFailure,
        br#"{"capture_kind":"compact_http_failure"}"#.to_vec(),
    );
    let message = compressed_message(&CaptureJob::new(capture)).expect("compress");
    let tau_proto::HarnessInputMessage::ProviderDebugCapture(capture) = message else {
        panic!("dedicated capture message");
    };
    assert_eq!(
        capture.class,
        tau_proto::ProviderDebugCaptureClass::CompactHttpFailure
    );
    assert_eq!(
        zstd::stream::decode_all(&capture.zstd[..]).expect("decode"),
        br#"{"capture_kind":"compact_http_failure"}"#
    );
}

/// Ensures absolute, traversal, and malformed session spellings cannot enter
/// structured capture attribution.
#[test]
fn capture_api_rejects_unsafe_session_identity() {
    for invalid in ["../escape", "/absolute", ".", "has/slash", "has space"] {
        assert!(tau_proto::SessionId::parse(invalid).is_err(), "{invalid}");
    }
}

/// Proves the raw payload bound is inclusive at the established protocol
/// ceiling and rejects the next byte before queue admission.
#[test]
fn raw_payload_bound_precedes_queue_admission() {
    let exact = job(
        "exact",
        &vec![0; tau_proto::MAX_PROTOCOL_MESSAGE_BYTES as usize],
    );
    let oversized = job(
        "oversized",
        &vec![0; tau_proto::MAX_PROTOCOL_MESSAGE_BYTES as usize + 1],
    );
    assert!(capture_fits_raw_bound(&exact.capture));
    assert!(!capture_fits_raw_bound(&oversized.capture));
}

/// Proves worker spawn failure leaves capture transport unavailable without
/// failing Provider startup.
#[test]
fn worker_spawn_failure_is_nonfatal() {
    let queue = start_transport_with(|| Err(io::Error::other("spawn failed")));
    assert!(queue.is_none());
}

/// Proves the complete encoded frame bound is inclusive at the shared ceiling
/// and rejects the next byte through a deterministic size seam.
#[test]
fn encoded_complete_frame_bound_is_enforced() {
    let message = compressed_message(&job("frame", b"small")).expect("message");
    assert!(enforce_encoded_bound(message.clone(), tau_proto::MAX_PROTOCOL_MESSAGE_BYTES).is_ok());
    assert!(enforce_encoded_bound(message, tau_proto::MAX_PROTOCOL_MESSAGE_BYTES + 1).is_err());
}
