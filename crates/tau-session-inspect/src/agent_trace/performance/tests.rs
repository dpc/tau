use super::*;

/// An unavailable wall timestamp cannot erase the preceding comparable sample
/// and hide a subsequent regression.
#[test]
fn unavailable_clock_sample_does_not_hide_regression() {
    let mut previous = None;
    let mut regressions = 0;
    for current in [100, 0, 50, 110] {
        observe_clock(&mut previous, &mut regressions, UnixMicros::new(current));
    }

    assert_eq!(previous, Some(UnixMicros::new(110)));
    assert_eq!(regressions, 1);
}
