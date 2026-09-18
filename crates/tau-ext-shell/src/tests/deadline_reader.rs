use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::*;

const SHORT_DEADLINE: Duration = Duration::from_millis(80);
const SCHEDULING_TOLERANCE: Duration = Duration::from_secs(2);

/// Builds one deadline-aware reader and its fake protocol peer.
fn reader_pair() -> (TestExtensionReader, UnixStream) {
    let (reader, peer) = UnixStream::pair().expect("test socket pair");
    (EventReader::deadline_aware(reader), peer)
}

/// Encodes one peer-to-harness protocol message.
fn encoded(message: &HarnessInputMessage) -> Vec<u8> {
    tau_proto::encode_message_to_vec(message).expect("encode test message")
}

/// Builds an event that is easy to recognize after event-reader filtering.
fn session_shutdown(session: &str) -> HarnessInputMessage {
    HarnessInputMessage::emit(Event::SessionShutdown(tau_proto::SessionShutdown {
        session_id: session.parse().expect("test session id"),
    }))
}

/// Requires a decode failure to retain the transport timeout classification.
fn assert_timeout(error: tau_proto::DecodeError) {
    assert!(
        matches!(error, tau_proto::DecodeError::Io(ref error) if error.kind() == std::io::ErrorKind::TimedOut),
        "expected timeout, got {error}"
    );
}

/// Ensures a silent connected peer cannot block past the local absolute
/// deadline.
#[test]
fn deadline_reader_times_out_silent_peer_and_closes_connection() {
    let (mut reader, mut peer) = reader_pair();
    let started = Instant::now();
    let _deadline = reader.arm_deadline(started + SHORT_DEADLINE);
    assert_timeout(
        reader
            .read_raw_message()
            .expect_err("silent peer must time out"),
    );
    assert!(started.elapsed() < SCHEDULING_TOLERANCE);

    peer.set_read_timeout(Some(SCHEDULING_TOLERANCE))
        .expect("peer timeout");
    let mut byte = [0_u8];
    assert_eq!(
        peer.read(&mut byte)
            .expect("deadline shutdown reaches peer"),
        0
    );
}

/// Ensures an incomplete valid message is terminal instead of being retried
/// after timeout.
#[test]
fn deadline_reader_times_out_partial_message_and_retires_decoder() {
    let (mut reader, mut peer) = reader_pair();
    let bytes = encoded(&HarnessInputMessage::Ready(tau_proto::Ready::default()));
    peer.write_all(&bytes[..bytes.len() / 2])
        .expect("partial message");

    let _deadline = reader.arm_deadline(Instant::now() + SHORT_DEADLINE);
    assert_timeout(
        reader
            .read_raw_message()
            .expect_err("partial message must time out"),
    );
    drop(_deadline);
    assert!(matches!(reader.read_raw_message(), Err(_) | Ok(None)));
}

/// Ensures trickled bytes do not reset the original absolute deadline.
#[test]
fn deadline_reader_times_out_trickled_message_and_joins_writer() {
    let (mut reader, mut peer) = reader_pair();
    let bytes = encoded(&session_shutdown("trickle-session"));
    let (done_tx, done_rx) = mpsc::channel();
    let writer = thread::spawn(move || {
        for byte in bytes {
            if peer.write_all(&[byte]).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = done_tx.send(());
    });

    let started = Instant::now();
    let _deadline = reader.arm_deadline(started + SHORT_DEADLINE);
    assert_timeout(
        reader
            .read_event()
            .expect_err("trickled message must time out"),
    );
    assert!(started.elapsed() < SCHEDULING_TOLERANCE);
    done_rx
        .recv_timeout(SCHEDULING_TOLERANCE)
        .expect("failed-connection writer must stop");
    writer.join().expect("trickle writer");
}

/// Ensures internally skipped buffered and streaming messages cannot hide
/// expiry.
#[test]
fn deadline_reader_checks_expiry_while_skipping_messages() {
    let (mut reader, mut peer) = reader_pair();
    let skipped = encoded(&HarnessInputMessage::Ready(tau_proto::Ready::default()));
    let mut buffered = Vec::new();
    for _ in 0..64 {
        buffered.extend_from_slice(&skipped);
    }
    peer.write_all(&buffered)
        .expect("buffered skipped messages");
    let (done_tx, done_rx) = mpsc::channel();
    let writer = thread::spawn(move || {
        while peer.write_all(&skipped).is_ok() {}
        let _ = done_tx.send(());
    });

    let _deadline = reader.arm_deadline(Instant::now() + SHORT_DEADLINE);
    assert_timeout(
        reader
            .read_event()
            .expect_err("skipped messages must not extend deadline"),
    );
    done_rx
        .recv_timeout(SCHEDULING_TOLERANCE)
        .expect("skipped-message writer must stop");
    writer.join().expect("skipped-message writer");
}

/// Ensures expiry is checked before accepting a message already buffered by the
/// decoder.
#[test]
fn deadline_reader_rejects_buffered_event_after_expiry() {
    let (mut reader, mut peer) = reader_pair();
    let first = encoded(&HarnessInputMessage::Ready(tau_proto::Ready::default()));
    let matching = encoded(&session_shutdown("buffered-session"));
    let mut batch = first;
    batch.extend_from_slice(&matching);
    peer.write_all(&batch).expect("buffered message batch");

    assert!(matches!(
        reader.read_raw_message().expect("first buffered message"),
        Some(HarnessInputMessage::Ready(_))
    ));
    let _deadline = reader.arm_deadline(Instant::now() - Duration::from_millis(1));
    assert_timeout(
        reader
            .read_event()
            .expect_err("expired buffered event must not be accepted"),
    );
}

/// Ensures successful consecutive waits restore socket state and use distinct
/// budgets.
#[test]
fn deadline_reader_restores_state_between_successful_waits() {
    let (mut reader, mut peer) = reader_pair();
    let prior_timeout = Duration::from_millis(700);
    reader
        .deadline
        .as_ref()
        .expect("deadline control")
        .stream
        .set_read_timeout(Some(prior_timeout))
        .expect("prior timeout");
    let first = encoded(&session_shutdown("first-session"));
    let second = encoded(&session_shutdown("second-session"));
    peer.write_all(&first).expect("first event");
    peer.write_all(&second).expect("second event");

    {
        let deadline = reader.arm_deadline(Instant::now() + Duration::from_millis(200));
        assert!(matches!(
            reader.read_event().expect("first wait"),
            Some(Event::SessionShutdown(event)) if event.session_id.as_str() == "first-session"
        ));
        deadline.finish();
    }
    assert_eq!(
        reader
            .deadline
            .as_ref()
            .expect("deadline control")
            .stream
            .read_timeout()
            .expect("restored timeout"),
        Some(prior_timeout)
    );
    {
        let deadline = reader.arm_deadline(Instant::now() + Duration::from_millis(500));
        assert!(matches!(
            reader.read_event().expect("second wait"),
            Some(Event::SessionShutdown(event)) if event.session_id.as_str() == "second-session"
        ));
        deadline.finish();
    }
    assert_eq!(
        reader
            .deadline
            .as_ref()
            .expect("deadline control")
            .stream
            .read_timeout()
            .expect("second restored timeout"),
        Some(prior_timeout)
    );
}

/// Ensures EOF and malformed input remain distinguishable from deadline expiry.
#[test]
fn deadline_reader_preserves_eof_and_malformed_errors() {
    let (mut eof_reader, eof_peer) = reader_pair();
    drop(eof_peer);
    let deadline = eof_reader.arm_deadline(Instant::now() + SHORT_DEADLINE);
    assert_eq!(eof_reader.read_raw_message().expect("clean EOF"), None);
    deadline.finish();

    let (mut malformed_reader, mut malformed_peer) = reader_pair();
    malformed_peer.write_all(&[0xff]).expect("malformed CBOR");
    drop(malformed_peer);
    let _deadline = malformed_reader.arm_deadline(Instant::now() + SHORT_DEADLINE);
    let error = malformed_reader
        .read_raw_message()
        .expect_err("malformed frame");
    assert!(
        !matches!(error, tau_proto::DecodeError::Io(error) if error.kind() == std::io::ErrorKind::TimedOut)
    );
}

/// Ensures unwinding a successful connection scope restores its prior socket
/// timeout.
#[test]
fn deadline_reader_restores_timeout_during_unwind() {
    let (reader, mut peer) = reader_pair();
    let control = Arc::clone(reader.deadline.as_ref().expect("deadline control"));
    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _deadline = reader.arm_deadline(Instant::now() + SHORT_DEADLINE);
        panic!("fixture unwind");
    }));
    assert!(unwind.is_err());
    assert_eq!(
        control.stream.read_timeout().expect("restored timeout"),
        None
    );
    assert_eq!(*control.deadline.lock().expect("deadline state"), None);
    peer.set_read_timeout(Some(SCHEDULING_TOLERANCE))
        .expect("peer timeout");
    let mut byte = [0_u8];
    assert_eq!(
        peer.read(&mut byte)
            .expect("unwind shutdown reaches retained peer"),
        0
    );
}
