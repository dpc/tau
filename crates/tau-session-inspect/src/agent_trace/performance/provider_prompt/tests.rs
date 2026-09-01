use super::*;

/// Unavailable zero and backward wall clocks cannot manufacture elapsed
/// time or relative offsets.
#[test]
fn unknown_and_backward_timestamps_are_omitted() {
    assert_eq!(valid_elapsed(UnixMicros::new(0), UnixMicros::new(9)), None);
    assert_eq!(valid_elapsed(UnixMicros::new(9), UnixMicros::new(0)), None);
    assert_eq!(valid_elapsed(UnixMicros::new(9), UnixMicros::new(8)), None);
    assert_eq!(
        relative_time(Some(UnixMicros::new(0)), Some(UnixMicros::new(7))),
        None
    );
    assert_eq!(relative_time(Some(UnixMicros::new(9)), None), None);
    assert_eq!(
        elapsed_without_regression(
            PromptStart {
                journal_seq: PersistedAgentEventSeq::new(0),
                recorded_at: UnixMicros::new(10),
                clock_regressions: 0,
            },
            UnixMicros::new(20),
            1,
        ),
        None
    );
}

/// Equal available wall timestamps form a genuine reported zero interval.
#[test]
fn equal_timestamps_report_zero_elapsed() {
    assert_eq!(
        valid_elapsed(UnixMicros::new(9), UnixMicros::new(9)),
        Some(0)
    );
}
