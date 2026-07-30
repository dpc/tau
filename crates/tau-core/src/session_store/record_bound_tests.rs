use tau_proto::{MessageAgentTarget, MessageDelivered, MessageFactId, MessageParty};

use super::*;

fn delivered_message(body: String) -> Event {
    Event::MessageDelivered(MessageDelivered::new(
        tau_proto::MessagePublisherId::parse("bridge-main")
            .expect("canonical publisher id must satisfy the identifier grammar"),
        MessageAgentTarget::new("missing-agent"),
        MessageFactId::new("message-1"),
        MessageParty {
            stable_id: "sender-1".to_owned(),
            display_name: None,
            sender_auth: None,
        },
        None,
        body,
    ))
}

fn encoded_record_length(body_len: usize, seq: u64) -> usize {
    let record = PersistedSessionEvent {
        seq: PersistedSessionEventSeq::new(seq),
        source: None,
        event: delivered_message("x".repeat(body_len)),
        recorded_at: UnixMicros::new(42),
    };
    let mut encoded = Vec::new();
    ciborium::into_writer(&record, &mut encoded).expect("test record encodes");
    encoded.len()
}

fn body_for_encoded_length(encoded_length: usize, seq: u64) -> String {
    const PROBE_BODY_BYTES: usize = 1024 * 1024;
    let overhead = encoded_record_length(PROBE_BODY_BYTES, seq) - PROBE_BODY_BYTES;
    let body = "x".repeat(encoded_length - overhead);
    assert_eq!(
        encoded_record_length(body.len(), seq),
        encoded_length,
        "large-record CBOR overhead should remain stable"
    );
    body
}

/// Ensures the session writer accepts the loader's exact maximum rather
/// than introducing an off-by-one mismatch at the shared boundary.
#[test]
fn session_record_limit_accepts_exact_boundary() {
    validate_record_length(Path::new("/not/opened/events.cbor"), MAX_RECORD_BYTES)
        .expect("exact boundary must be accepted");
}

/// Ensures a successfully appended session record can be decoded by the
/// bounded loader after reopening the durable store.
#[test]
fn bounded_session_record_round_trips_after_write() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = SessionStore::open(temp.path()).expect("store opens");
    let event = delivered_message("read after write".to_owned());
    store
        .append_session_event_at("session-1", None, event.clone(), UnixMicros::new(42))
        .expect("bounded record appends");
    drop(store);

    let reopened = SessionStore::open(temp.path()).expect("written record reloads");
    let events = reopened
        .session_events("session-1")
        .expect("written record reads");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, event);
}

/// Ensures an oversized encoded session record is rejected before journal,
/// folded sequence, or derived metadata state changes.
#[test]
fn oversized_session_append_is_atomic() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = SessionStore::open(temp.path()).expect("store opens");
    store
        .append_session_event_at(
            "session-1",
            None,
            delivered_message("baseline".to_owned()),
            UnixMicros::new(41),
        )
        .expect("baseline appends");
    let session_dir = temp.path().join("session-1");
    let journal_path = session_dir.join("events.cbor");
    let meta_path = session_dir.join("meta.json");
    write_meta(
        &meta_path,
        &SessionMeta {
            created_at: 1,
            last_touched: 2,
        },
    )
    .expect("write sentinel metadata");
    let journal_before = fs::read(&journal_path).expect("baseline journal");
    let meta_before = fs::read(&meta_path).expect("derived metadata");
    let oversized_body = body_for_encoded_length((MAX_RECORD_BYTES + 1) as usize, 1);

    let error = store
        .append_session_event_at(
            "session-1",
            None,
            delivered_message(oversized_body),
            UnixMicros::new(42),
        )
        .expect_err("oversized record must fail");

    assert!(matches!(
        error,
        SessionStoreError::RecordTooLarge {
            record_length,
            maximum: MAX_RECORD_BYTES,
            ..
        } if record_length == MAX_RECORD_BYTES + 1
    ));
    assert_eq!(
        fs::read(&journal_path).expect("journal remains"),
        journal_before
    );
    assert_eq!(fs::read(&meta_path).expect("metadata remains"), meta_before);
    let outcome = store
        .append_session_event_at(
            "session-1",
            None,
            delivered_message("after rejection".to_owned()),
            UnixMicros::new(43),
        )
        .expect("later bounded record appends");
    assert_eq!(outcome.seq, PersistedSessionEventSeq::new(1));
    assert_eq!(
        store
            .session_events("session-1")
            .expect("journal remains readable")
            .len(),
        2
    );
}
