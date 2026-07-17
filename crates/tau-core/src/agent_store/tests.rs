//! Agent-store framing and retained-history compatibility tests.

use tau_proto::{CborValue, HarnessNotice, NoticeLevel};

use super::*;

/// Historical agent records used an unrelated event id and had no durable
/// per-journal sequence field.
#[derive(Serialize)]
struct LegacyPersistedAgentEvent {
    /// Historical global event id, ignored by retained-history scans.
    id: u64,
    /// Connection that published the event.
    source: Option<ConnectionId>,
    /// Persisted semantic event.
    event: Event,
    /// Explicit fold parent.
    parent: AgentEventParent,
    /// Original append timestamp.
    recorded_at: UnixMicros,
}

/// Returns one small non-folding event suitable for storage compatibility
/// tests.
fn notice(message: &str) -> Event {
    Event::HarnessNotice(HarnessNotice {
        kind: "test".to_owned(),
        message: message.to_owned(),
        level: NoticeLevel::Info,
        always_show: false,
    })
}

/// Creates one durable agent directory and returns its event-log path.
fn event_log(temp: &tempfile::TempDir) -> PathBuf {
    let agent_dir = temp.path().join("agent-a");
    fs::create_dir(&agent_dir).expect("agent dir");
    agent_dir.join("events.cbor")
}

/// Encodes a serializable value into its generic CBOR representation.
fn cbor_value(value: &impl Serialize) -> CborValue {
    let bytes = tau_proto::encode_message_to_vec(value).expect("encode test value");
    tau_proto::decode_message_from_slice(&bytes).expect("decode test value")
}

/// Returns a structurally complete sequenced record.
fn sequenced_record(sequence: u64) -> CborValue {
    cbor_value(&PersistedAgentEvent {
        seq: PersistedAgentEventSeq::new(sequence),
        source: None,
        event: notice("current"),
        parent: AgentEventParent::InheritHead,
        recorded_at: UnixMicros::new(sequence),
    })
}

/// Returns a generic CBOR map's fields.
fn map_fields(value: &mut CborValue) -> &mut Vec<(CborValue, CborValue)> {
    let CborValue::Map(fields) = value else {
        panic!("test value must be a map");
    };
    fields
}

/// Returns a uniquely named mutable map field.
fn map_field_mut<'a>(value: &'a mut CborValue, name: &str) -> &'a mut CborValue {
    map_fields(value)
        .iter_mut()
        .find_map(|(key, value)| {
            matches!(key, CborValue::Text(key) if key == name).then_some(value)
        })
        .expect("test field")
}

/// Appends one named map field.
fn push_field(value: &mut CborValue, name: &str, field: CborValue) {
    map_fields(value).push((CborValue::Text(name.to_owned()), field));
}

/// Replaces one named map field.
fn replace_field(value: &mut CborValue, name: &str, replacement: CborValue) {
    *map_field_mut(value, name) = replacement;
}

/// Removes every map field with the given name.
fn remove_field(value: &mut CborValue, name: &str) {
    map_fields(value).retain(|(key, _)| !matches!(key, CborValue::Text(key) if key == name));
}

/// Expected fail-closed classification for one raw retained record.
#[derive(Clone, Copy)]
enum ExpectedScanError {
    /// Record markers or event discriminator are structurally invalid.
    InvalidEncoding,
    /// Explicit sequence does not match file order.
    InvalidSequence,
    /// A selected incoming record cannot be semantically decoded.
    Decode,
}

/// Asserts one named raw record fails as expected without projecting an
/// incoming occurrence.
fn assert_retained_scan_fails(case_name: &str, record: CborValue, expected: ExpectedScanError) {
    let temp = tempfile::TempDir::new().expect("temporary state");
    let path = event_log(&temp);
    append_cbor_record(&path, &record).expect("raw record");
    let store = AgentStore::open_lazy(temp.path()).expect("agent store");
    let agent_id = AgentId::parse("agent-a").expect("agent id");
    let mut projected = false;
    let error = store
        .visit_retained_transport_ingress_events(&agent_id, |_| {
            projected = true;
            true
        })
        .expect_err(case_name);
    let matches_expected = match expected {
        ExpectedScanError::InvalidEncoding => {
            matches!(error, AgentStoreError::InvalidRetainedEncoding { .. })
        }
        ExpectedScanError::InvalidSequence => {
            matches!(error, AgentStoreError::InvalidSequence { .. })
        }
        ExpectedScanError::Decode => matches!(error, AgentStoreError::Decode { .. }),
    };
    assert!(matches_expected, "{case_name}: unexpected error: {error}");
    assert!(!projected, "{case_name}: projected an invalid record");
}

/// Ensures the writer rejects a record length that the matching loader would
/// reject, before opening or mutating the journal.
#[test]
fn write_record_limit_matches_read_limit() {
    let error = validate_record_length(Path::new("/not/opened/events.cbor"), MAX_RECORD_BYTES + 1)
        .expect_err("oversized record must be rejected");
    assert!(matches!(
        error,
        AgentStoreError::RecordTooLarge {
            record_length,
            maximum: MAX_RECORD_BYTES,
            ..
        } if record_length == MAX_RECORD_BYTES + 1
    ));
}

/// A uniformly legacy journal is structurally accepted as predating typed
/// ingress without requiring unrelated historical payloads to decode.
#[test]
fn retained_scan_accepts_uniform_pre_sequence_journal() {
    let temp = tempfile::TempDir::new().expect("temporary state");
    let path = event_log(&temp);
    for (id, message) in ["first", "second"].into_iter().enumerate() {
        append_cbor_record(
            &path,
            &LegacyPersistedAgentEvent {
                id: id as u64,
                source: None,
                event: notice(message),
                parent: AgentEventParent::InheritHead,
                recorded_at: UnixMicros::new(id as u64),
            },
        )
        .expect("legacy record");
    }
    let store = AgentStore::open_lazy(temp.path()).expect("agent store");
    let agent_id = AgentId::parse("agent-a").expect("agent id");
    let mut incoming_count = 0;
    store
        .visit_retained_transport_ingress_events(&agent_id, |_| {
            incoming_count += 1;
            true
        })
        .expect("legacy retained scan");
    assert_eq!(incoming_count, 0);
    assert!(matches!(
        store.agent_events("agent-a"),
        Err(AgentStoreError::Decode { .. })
    ));
}

/// Schema drift solely inside an unrelated sequenced event payload cannot
/// disable a transport-only derived-index rebuild.
#[test]
fn retained_scan_ignores_obsolete_non_transport_payload() {
    let temp = tempfile::TempDir::new().expect("temporary state");
    let path = event_log(&temp);
    let mut record = sequenced_record(0);
    let event = map_field_mut(&mut record, "event");
    replace_field(
        event,
        "event",
        CborValue::Text("agent.head_moved".to_owned()),
    );
    replace_field(event, "payload", CborValue::Map(Vec::new()));
    append_cbor_record(&path, &record).expect("obsolete record");
    let store = AgentStore::open_lazy(temp.path()).expect("agent store");
    let agent_id = AgentId::parse("agent-a").expect("agent id");
    store
        .visit_retained_transport_ingress_events(&agent_id, |_| {
            panic!("unrelated event must not be projected")
        })
        .expect("transport-only retained scan");
    assert!(matches!(
        store.agent_events("agent-a"),
        Err(AgentStoreError::Decode { .. })
    ));
}

/// A journal that switches between legacy and sequenced records in either
/// direction is not a valid historical encoding and fails closed.
#[test]
fn retained_scan_rejects_mixed_sequence_encodings() {
    for legacy_first in [true, false] {
        let temp = tempfile::TempDir::new().expect("temporary state");
        let path = event_log(&temp);
        let legacy = LegacyPersistedAgentEvent {
            id: 0,
            source: None,
            event: notice("legacy"),
            parent: AgentEventParent::InheritHead,
            recorded_at: UnixMicros::new(0),
        };
        let current = PersistedAgentEvent {
            seq: PersistedAgentEventSeq::new(0),
            source: None,
            event: notice("sequenced"),
            parent: AgentEventParent::InheritHead,
            recorded_at: UnixMicros::new(1),
        };
        if legacy_first {
            append_cbor_record(&path, &legacy).expect("legacy record");
            append_cbor_record(&path, &current).expect("sequenced record");
        } else {
            append_cbor_record(&path, &current).expect("sequenced record");
            append_cbor_record(&path, &legacy).expect("legacy record");
        }
        let store = AgentStore::open_lazy(temp.path()).expect("agent store");
        let agent_id = AgentId::parse("agent-a").expect("agent id");
        assert!(matches!(
            store.visit_retained_transport_ingress_events(&agent_id, |_| true),
            Err(AgentStoreError::InvalidRetainedEncoding { .. })
        ));
    }
}

/// Marker presence, type, uniqueness, and explicit sequence are fail-closed.
#[test]
fn retained_scan_rejects_invalid_markers_and_sequences() {
    let mut cases = Vec::new();

    let mut both = sequenced_record(0);
    push_field(&mut both, "id", CborValue::Integer(7_u64.into()));
    cases.push(("both markers", both, ExpectedScanError::InvalidEncoding));

    let mut malformed_opposite = sequenced_record(0);
    push_field(
        &mut malformed_opposite,
        "id",
        CborValue::Text("bad".to_owned()),
    );
    cases.push((
        "malformed opposite marker",
        malformed_opposite,
        ExpectedScanError::InvalidEncoding,
    ));

    let mut legacy_with_malformed_seq = sequenced_record(0);
    replace_field(
        &mut legacy_with_malformed_seq,
        "seq",
        CborValue::Text("bad".to_owned()),
    );
    push_field(
        &mut legacy_with_malformed_seq,
        "id",
        CborValue::Integer(7_u64.into()),
    );
    cases.push((
        "legacy marker with malformed sequence",
        legacy_with_malformed_seq,
        ExpectedScanError::InvalidEncoding,
    ));

    let mut duplicate_seq = sequenced_record(0);
    push_field(&mut duplicate_seq, "seq", CborValue::Integer(0_u64.into()));
    cases.push((
        "duplicate sequence",
        duplicate_seq,
        ExpectedScanError::InvalidEncoding,
    ));

    let mut duplicate_id = sequenced_record(0);
    remove_field(&mut duplicate_id, "seq");
    push_field(&mut duplicate_id, "id", CborValue::Integer(1_u64.into()));
    push_field(&mut duplicate_id, "id", CborValue::Integer(2_u64.into()));
    cases.push((
        "duplicate legacy id",
        duplicate_id,
        ExpectedScanError::InvalidEncoding,
    ));

    let mut unmarked = sequenced_record(0);
    remove_field(&mut unmarked, "seq");
    cases.push((
        "unmarked record",
        unmarked,
        ExpectedScanError::InvalidEncoding,
    ));

    cases.push((
        "incorrect explicit sequence",
        sequenced_record(1),
        ExpectedScanError::InvalidSequence,
    ));

    for (case_name, record, expected) in cases {
        assert_retained_scan_fails(case_name, record, expected);
    }
}

/// Missing, duplicate, malformed, and falsely selected event discriminators
/// cannot be skipped as unrelated history.
#[test]
fn retained_scan_rejects_invalid_event_discriminators_and_selected_payload() {
    let mut cases = Vec::new();

    let mut missing = sequenced_record(0);
    remove_field(map_field_mut(&mut missing, "event"), "event");
    cases.push((
        "missing discriminator",
        missing,
        ExpectedScanError::InvalidEncoding,
    ));

    let mut duplicate = sequenced_record(0);
    push_field(
        map_field_mut(&mut duplicate, "event"),
        "event",
        CborValue::Text("harness.notice".to_owned()),
    );
    cases.push((
        "duplicate discriminator",
        duplicate,
        ExpectedScanError::InvalidEncoding,
    ));

    let mut malformed = sequenced_record(0);
    replace_field(
        map_field_mut(&mut malformed, "event"),
        "event",
        CborValue::Text("agent_message_incoming".to_owned()),
    );
    cases.push((
        "malformed discriminator",
        malformed,
        ExpectedScanError::InvalidEncoding,
    ));

    let mut malformed_selected = sequenced_record(0);
    replace_field(
        map_field_mut(&mut malformed_selected, "event"),
        "event",
        CborValue::Text(EventName::AGENT_MESSAGE_INCOMING.to_string()),
    );
    cases.push((
        "malformed selected incoming payload",
        malformed_selected,
        ExpectedScanError::Decode,
    ));

    for (case_name, record, expected) in cases {
        assert_retained_scan_fails(case_name, record, expected);
    }
}
