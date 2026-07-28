use super::*;

fn info(message: &str) -> Event {
    Event::HarnessNotice(tau_proto::HarnessNotice {
        kind: "test.info".to_owned(),
        message: message.to_owned(),
        level: tau_proto::NoticeLevel::Info,
        always_show: false,
    })
}

#[test]
fn append_assigns_sequence_and_timestamp_without_retaining_payloads_in_production() {
    let log = EventLog::new();
    let (seq, recorded_at) = log.append();
    assert_eq!(seq.get(), 0);
    assert!(recorded_at.get() > 0, "append should stamp wall-clock time");
    assert_eq!(log.next_seq().get(), 1);
}

#[test]
fn test_observer_records_committed_events() {
    let log = EventLog::new();
    let (seq, recorded_at) = log.append();
    log.record_for_test(
        seq,
        recorded_at,
        Some(crate::test_connection_id("conn-1")),
        info("hello"),
    );

    let entry = log
        .get_next_from(crate::event_log::EventLogSeq::new(0))
        .expect("entry should exist");
    assert_eq!(entry.seq.get(), 0);
    assert_eq!(entry.recorded_at, recorded_at);
    assert_eq!(entry.source, Some(crate::test_connection_id("conn-1")));
}

#[test]
fn get_next_from_skips_earlier_test_observer_entries() {
    let log = EventLog::new();
    for message in ["a", "b", "c"] {
        let (seq, recorded_at) = log.append();
        log.record_for_test(seq, recorded_at, None, info(message));
    }

    let entry = log
        .get_next_from(crate::event_log::EventLogSeq::new(1))
        .expect("entry should exist");
    assert_eq!(entry.seq.get(), 1);
    let Event::HarnessNotice(info) = &entry.event else {
        panic!("expected HarnessNotice");
    };
    assert_eq!(info.message, "b");
}
