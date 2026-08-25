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

/// Proves the production queue admits its exact bound, then rejects and returns
/// the next capture without waiting or changing its private attribution.
#[test]
fn production_queue_bound_rejects_new_capture_without_blocking() {
    assert_eq!(super::CAPTURE_QUEUE_CAPACITY, 64);
    let (sender, _receiver) = mpsc::sync_channel(super::CAPTURE_QUEUE_CAPACITY);
    let queue = CaptureQueue::with_sender(sender);
    for index in 1..=super::CAPTURE_QUEUE_CAPACITY {
        queue
            .try_submit(job(&format!("prompt-{index}"), b"capture"))
            .unwrap_or_else(|_| panic!("admission {index} within production capacity"));
    }

    let rejected = queue
        .try_submit(job("rejected", b"private rejected capture"))
        .expect_err("first capture over the bound");
    let mpsc::TrySendError::Full(rejected) = rejected else {
        panic!("queue should reject the capture because it is full");
    };
    assert_eq!(rejected.capture.session_id.as_str(), "session-test");
    assert_eq!(rejected.capture.agent_prompt_id.as_str(), "rejected");
    assert_eq!(
        rejected.capture.class,
        tau_proto::ProviderDebugCaptureClass::HttpSseRequest
    );
    assert_eq!(rejected.capture.json, b"private rejected capture");
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

/// Proves the Provider compresses the exact JSON and preserves only structured
/// attribution in the dedicated non-event protocol message.
#[test]
fn compression_builds_opaque_attributed_protocol_message() {
    let json = br#"{"secret":"debug"}"#;
    let capture = ProviderDebugCapture::new(
        tau_proto::SessionId::parse("session-test").expect("session"),
        tau_proto::AgentPromptId::parse("prompt").expect("prompt"),
        tau_proto::ProviderDebugCaptureClass::HttpSseResponse,
        json.to_vec(),
    );
    let message = compressed_message(&CaptureJob::new(capture)).expect("compress");
    let tau_proto::HarnessInputMessage::ProviderDebugCapture(capture) = message else {
        panic!("dedicated capture message");
    };
    assert_eq!(capture.session_id.as_str(), "session-test");
    assert_eq!(capture.agent_prompt_id.as_str(), "prompt");
    assert_eq!(
        capture.class,
        tau_proto::ProviderDebugCaptureClass::HttpSseResponse
    );
    assert_eq!(
        zstd::stream::decode_all(&capture.zstd[..]).expect("decode"),
        json
    );
    assert!(!format!("{capture:?}").contains("secret"));
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
