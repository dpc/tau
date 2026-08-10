//! Deterministic regression coverage for public Responses stream deadlines.

use std::time::{Duration, Instant};

use super::deadlines::{STREAM_IDLE_TIMEOUT, STREAM_TOTAL_TIMEOUT, StreamDeadlines};

/// Semantic output dripped before each idle deadline must renew its five-minute
/// wait, but the tenth minute still ends the stream.
#[test]
fn semantic_drip_renews_idle_without_extending_total_deadline() {
    let start = Instant::now();
    let mut deadlines = StreamDeadlines::new(start);

    deadlines.renew_for_qualifying_progress(start + Duration::from_secs(4 * 60));
    assert!(!deadlines.expired(start + Duration::from_secs(8 * 60 + 59)));
    assert!(deadlines.expired(start + STREAM_TOTAL_TIMEOUT));
}

/// Heartbeat-like observations must not refresh semantic-idle time, so a stream
/// of transport-only keepalives fails at its original fifth minute.
#[test]
fn heartbeats_do_not_renew_semantic_idle_deadline() {
    let start = Instant::now();
    let deadlines = StreamDeadlines::new(start);

    for minute in 1..5 {
        let heartbeat = start + Duration::from_secs(minute * 60);
        assert!(!deadlines.expired(heartbeat));
    }
    assert!(deadlines.expired(start + STREAM_IDLE_TIMEOUT));
}
