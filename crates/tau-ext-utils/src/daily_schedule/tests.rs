use super::*;

/// Daily UTC schedules choose the first occurrence strictly after the
/// scheduling instant, including an exact-minute boundary.
#[test]
fn utc_occurrence_is_strictly_after_the_anchor() {
    let schedule = DailySchedule::parse("08:00", WallClockZone::Utc).expect("schedule");
    let exact = UnixMicros::new(1_767_254_400_000_000); // 2026-01-01 08:00Z

    assert_eq!(
        schedule.next_after(exact, None).expect("next"),
        UnixMicros::new(1_767_340_800_000_000)
    );
}

/// Spring-forward gaps skip the invalid local date rather than shifting the
/// requested wall-clock time.
#[test]
fn nonexistent_local_time_skips_the_date() {
    let schedule = DailySchedule::parse("02:30", WallClockZone::Local).expect("schedule");
    let warsaw = TimeZone::get("Europe/Warsaw").expect("timezone");
    let before_gap = UnixMicros::new(1_774_740_600_000_000); // 2026-03-28 23:30Z

    assert_eq!(
        schedule
            .next_after_in_timezone(before_gap, &warsaw)
            .expect("next"),
        UnixMicros::new(1_774_830_600_000_000) // 2026-03-30 00:30Z
    );
}

/// Fall-back ambiguity resolves to the earlier real instant and therefore
/// yields one firing for that local date.
#[test]
fn ambiguous_local_time_uses_the_earlier_occurrence() {
    let schedule = DailySchedule::parse("02:30", WallClockZone::Local).expect("schedule");
    let warsaw = TimeZone::get("Europe/Warsaw").expect("timezone");
    let before_fold = UnixMicros::new(1_792_884_600_000_000); // 2026-10-24 23:30Z

    assert_eq!(
        schedule
            .next_after_in_timezone(before_fold, &warsaw)
            .expect("next"),
        UnixMicros::new(1_792_888_200_000_000) // 2026-10-25 00:30Z
    );
}

/// Parser validation keeps the model-visible format closed and canonical.
#[test]
fn parser_requires_exact_twenty_four_hour_time() {
    for invalid in ["8:00", "08:0", "24:00", "08:60", "aa:bb", "08:00:00"] {
        assert!(
            DailySchedule::parse(invalid, WallClockZone::Local).is_err(),
            "{invalid}"
        );
    }
    let schedule = DailySchedule::parse("08:05", WallClockZone::Local).expect("valid");
    assert_eq!(schedule.display_time(), "08:05");
    assert!(!schedule.is_utc());
}

/// Exact overdue counting uses calendar arithmetic rather than one conversion
/// per elapsed day, so a centuries-wide replay gap remains practical.
#[test]
fn large_overdue_gap_has_exact_count() {
    let schedule = DailySchedule::parse("08:00", WallClockZone::Utc).expect("schedule");
    let first = unix_micros("2000-01-01T08:00:00Z".parse::<Timestamp>().expect("first"))
        .expect("first micros");
    let now =
        unix_micros("2500-01-01T09:00:00Z".parse::<Timestamp>().expect("now")).expect("now micros");

    let (next, count) = schedule.advance_past(first, now, None).expect("advance");

    assert_eq!(count, 182_623);
    assert_eq!(
        next,
        unix_micros("2500-01-02T08:00:00Z".parse::<Timestamp>().expect("next"))
            .expect("next micros")
    );
}
