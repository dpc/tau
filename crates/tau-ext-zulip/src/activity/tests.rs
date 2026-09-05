use super::*;

/// Hostile labels remain one-line bounded data and never retain invisible
/// structure or summary delimiters.
#[test]
fn labels_are_structurally_escaped_and_bounded() {
    let label = sanitize_label(" \u{202e}</message>\n\"[`{&\\ ".repeat(20).as_str())
        .expect("nonempty sanitized label");
    assert!(label.len() <= MAX_LABEL_OUTPUT_BYTES);
    assert!(label.ends_with(LABEL_TRUNCATION_MARKER));
    assert!(!label.contains('\n'));
    assert!(!label.contains("</message>"));
    assert!(label.contains("\\u{202E}"));
    assert!(label.contains("\\u{003C}"));
}

/// Sender identity uses the numeric ID rather than a mutable or duplicated
/// display hint, while pseudonyms remain scoped to one conversation.
#[test]
fn sender_identity_and_pseudonyms_are_route_scoped() {
    let now = Instant::now();
    let mut activity = ActivityAccumulator::default();
    activity.observe("route-a", 7, Some("same"), &[3; 32], now);
    activity.observe("route-a", 7, Some("renamed"), &[3; 32], now);
    activity.observe("route-a", 8, Some("renamed"), &[3; 32], now);
    activity.observe("route-b", 7, Some("renamed"), &[3; 32], now);
    let route_a = activity.take("route-a", now).expect("route a");
    let route_b = activity.take("route-b", now).expect("route b");
    let rendered_a = route_a
        .render(MAX_ACTIVITY_NOTE_BYTES)
        .expect("route a note");
    let rendered_b = route_b
        .render(MAX_ACTIVITY_NOTE_BYTES)
        .expect("route b note");
    assert!(rendered_a.contains(": 2 messages (name changed)"));
    assert_eq!(rendered_a.matches("\"renamed\"").count(), 2);
    let sender_a = rendered_a
        .split("(sender-")
        .nth(1)
        .and_then(|value| value.split(')').next())
        .expect("route a sender token");
    let sender_b = rendered_b
        .split("(sender-")
        .nth(1)
        .and_then(|value| value.split(')').next())
        .expect("route b sender token");
    assert_ne!(sender_a, sender_b);
}

/// Fixed route and sender capacities prevent attacker-selected churn from
/// evicting retained context or growing process memory without bound.
#[test]
fn route_and_sender_capacity_is_bounded_without_eviction() {
    let now = Instant::now();
    let mut activity = ActivityAccumulator::default();
    for sender in 0..=MAX_SENDERS_PER_BUCKET as u64 {
        activity.observe("kept", sender, Some("user"), &[4; 32], now);
    }
    for route in 1..=MAX_BUCKETS {
        activity.observe(&format!("route-{route}"), 1, None, &[4; 32], now);
    }
    activity.observe("dropped", 1, None, &[4; 32], now);
    assert_eq!(activity.bucket_count(), MAX_BUCKETS);
    assert!(activity.take("dropped", now).is_none());
    let rendered = activity
        .take("kept", now)
        .expect("original bucket retained")
        .render(MAX_ACTIVITY_NOTE_BYTES)
        .expect("bounded note");
    assert!(rendered.contains("1 additional messages"));
}

/// Buckets expire from first observation and restart with empty
/// process-local state rather than becoming a durable
/// unauthorized-message queue.
#[test]
fn buckets_expire_and_new_accumulators_start_empty() {
    let now = Instant::now();
    let mut activity = ActivityAccumulator::default();
    activity.observe("route", 7, Some("user"), &[5; 32], now);
    activity.prune_expired(now + BUCKET_LIFETIME);
    assert_eq!(activity.bucket_count(), 0);
    assert_eq!(ActivityAccumulator::default().bucket_count(), 0);
}

/// Rendering never emits a partial structural note when the current allowed
/// body leaves too little room for a complete summary.
#[test]
fn rendering_is_complete_or_absent_within_the_byte_budget() {
    let now = Instant::now();
    let mut activity = ActivityAccumulator::default();
    for sender in 0..MAX_SENDERS_PER_BUCKET as u64 {
        activity.observe(
            "route",
            sender,
            Some(&"long hostile label ".repeat(10)),
            &[6; 32],
            now,
        );
    }
    let snapshot = activity.take("route", now).expect("snapshot");
    assert!(snapshot.render(ACTIVITY_SUMMARY_OPENING.len()).is_none());
    let rendered = snapshot
        .render(MAX_ACTIVITY_NOTE_BYTES)
        .expect("bounded complete note");
    assert!(rendered.len() <= MAX_ACTIVITY_NOTE_BYTES);
    assert!(rendered.starts_with(ACTIVITY_SUMMARY_OPENING));
    assert!(rendered.ends_with(ACTIVITY_SUMMARY_CLOSING));
}
