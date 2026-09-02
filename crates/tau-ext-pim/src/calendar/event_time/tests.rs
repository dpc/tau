use super::EventTimeRange;

/// Exact write boundaries must retain noncanonical but accepted RFC3339 bytes.
#[test]
fn exact_range_retains_accepted_raw_bytes() {
    let range = EventTimeRange::from_exact(
        "2026-05-28T14:00:00+02:00".to_owned(),
        "2026-05-28T15:00:00+02:00".to_owned(),
    )
    .expect("offset range");

    assert_eq!(range.start_raw(), "2026-05-28T14:00:00+02:00");
    assert_eq!(range.end_raw(), "2026-05-28T15:00:00+02:00");
    assert!(!range.is_all_day());
}

/// Pair validation must preserve the runtime's mixed-kind and ordering
/// diagnostics.
#[test]
fn exact_range_preserves_pair_error_precedence() {
    assert_eq!(
        EventTimeRange::from_exact("2026-05-28".to_owned(), "not a time".to_owned())
            .err()
            .expect("mixed pair"),
        "event start and end must both be all-day dates or both be RFC3339 date-times"
    );
    assert_eq!(
        EventTimeRange::from_exact(
            "2026-05-28T12:00:00Z".to_owned(),
            "2026-05-28T12:00:00Z".to_owned(),
        )
        .err()
        .expect("equal pair"),
        "event start must be before event end"
    );
    assert_eq!(
        EventTimeRange::from_exact("💣".to_owned(), "2026-05-28T13:00:00Z".to_owned())
            .err()
            .expect("invalid UTF-8 text"),
        "start must be RFC3339 or YYYY-MM-DD: the 'year' component could not be parsed"
    );
}

/// Default construction must preserve the all-day exclusive end and timed hour.
#[test]
fn exact_range_builds_existing_create_defaults() {
    let all_day =
        EventTimeRange::with_default_end("2026-05-28".to_owned()).expect("all-day default");
    assert_eq!(all_day.start_raw(), "2026-05-28");
    assert_eq!(all_day.end_raw(), "2026-05-29");

    let timed =
        EventTimeRange::with_default_end("2026-05-28T12:00:00Z".to_owned()).expect("timed default");
    assert_eq!(timed.start_raw(), "2026-05-28T12:00:00Z");
    assert_eq!(timed.end_raw(), "2026-05-28T13:00:00Z");
}
