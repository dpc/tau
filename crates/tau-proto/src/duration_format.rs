//! Compact human-readable formatting for approximate durations.

#[cfg(test)]
#[path = "duration_format/tests.rs"]
mod tests;

const SECONDS_PER_MINUTE: u64 = 60;
const MINUTES_PER_HOUR: u64 = 60;
const HOURS_PER_DAY: u64 = 24;
const SECONDS_PER_HOUR: u64 = MINUTES_PER_HOUR * SECONDS_PER_MINUTE;
const SECONDS_PER_DAY: u64 = HOURS_PER_DAY * SECONDS_PER_HOUR;

/// Formats whole seconds as a compact approximate duration with at most two
/// units.
///
/// Seconds remain exact below one hour. Longer durations round to the nearest
/// minute or hour so multi-day provider waits stay readable.
#[must_use]
pub fn format_approximate_duration_secs(seconds: u64) -> String {
    if seconds < SECONDS_PER_MINUTE {
        return format!("{seconds}s");
    }
    if seconds < SECONDS_PER_HOUR {
        return format_two_units(
            seconds / SECONDS_PER_MINUTE,
            "m",
            seconds % SECONDS_PER_MINUTE,
            "s",
        );
    }

    if seconds < SECONDS_PER_DAY {
        let rounded_minutes = rounded_units(seconds, SECONDS_PER_MINUTE);
        if rounded_minutes < HOURS_PER_DAY * MINUTES_PER_HOUR {
            return format_two_units(
                rounded_minutes / MINUTES_PER_HOUR,
                "h",
                rounded_minutes % MINUTES_PER_HOUR,
                "m",
            );
        }
    }

    let rounded_hours = rounded_units(seconds, SECONDS_PER_HOUR);
    format_two_units(
        rounded_hours / HOURS_PER_DAY,
        "d",
        rounded_hours % HOURS_PER_DAY,
        "h",
    )
}

fn rounded_units(seconds: u64, unit_seconds: u64) -> u64 {
    let whole = seconds / unit_seconds;
    whole + u64::from(seconds % unit_seconds >= unit_seconds / 2)
}

fn format_two_units(major: u64, major_suffix: &str, minor: u64, minor_suffix: &str) -> String {
    if minor == 0 {
        format!("{major}{major_suffix}")
    } else {
        format!("{major}{major_suffix} {minor}{minor_suffix}")
    }
}
