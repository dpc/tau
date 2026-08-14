#[cfg(test)]
mod tests;

use jiff::Timestamp;
use jiff::civil::{DateTime, Time};
use jiff::tz::{AmbiguousOffset, TimeZone};
use tau_proto::UnixMicros;

/// Stable reference used by schedules that explicitly select UTC.
static UTC_TIMEZONE: TimeZone = TimeZone::UTC;

/// Clock authority for one daily schedule.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WallClockZone {
    /// Follow the running host's configured local timezone.
    Local,
    /// Use Coordinated Universal Time.
    Utc,
}

/// One daily wall-clock firing time.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DailySchedule {
    /// Local or UTC time of day.
    time: Time,
    /// Clock authority used to interpret the time of day.
    zone: WallClockZone,
}

impl DailySchedule {
    /// Parse an exact `HH:MM` daily time.
    pub(crate) fn parse(value: &str, zone: WallClockZone) -> Result<Self, String> {
        let bytes = value.as_bytes();
        if bytes.len() != 5 || bytes[2] != b':' {
            return Err("daily_time must use exact HH:MM format".to_owned());
        }
        let hour = parse_pair(&bytes[..2])
            .filter(|hour| *hour < 24)
            .ok_or_else(|| "daily_time hour must be 00..23".to_owned())?;
        let minute = parse_pair(&bytes[3..])
            .filter(|minute| *minute < 60)
            .ok_or_else(|| "daily_time minute must be 00..59".to_owned())?;
        let time = Time::new(hour as i8, minute as i8, 0, 0)
            .map_err(|_| "daily_time must use exact HH:MM format".to_owned())?;
        Ok(Self { time, zone })
    }

    /// Return whether this schedule uses UTC rather than host-local time.
    pub(crate) fn is_utc(&self) -> bool {
        self.zone == WallClockZone::Utc
    }

    /// Render the canonical `HH:MM` wall-clock time.
    pub(crate) fn display_time(&self) -> String {
        format!("{:02}:{:02}", self.time.hour(), self.time.minute())
    }

    /// Find the first valid occurrence strictly after `after`.
    pub(crate) fn next_after(
        &self,
        after: UnixMicros,
        local_timezone: Option<&TimeZone>,
    ) -> Result<UnixMicros, String> {
        let timezone = self.timezone(local_timezone)?;
        self.next_after_in_timezone(after, timezone)
    }

    /// Advance beyond `now` and exactly count overdue occurrences.
    ///
    /// The count uses calendar distance and timezone transitions rather than
    /// iterating once per elapsed day.
    pub(crate) fn advance_past(
        &self,
        first: UnixMicros,
        now: UnixMicros,
        local_timezone: Option<&TimeZone>,
    ) -> Result<(UnixMicros, u64), String> {
        let timezone = self.timezone(local_timezone)?;
        let first = timestamp(first)?;
        let now = timestamp(now)?;
        let next = timestamp(self.next_after_in_timezone(unix_micros(now)?, timezone)?)?;
        let first_date = timezone.to_datetime(first).date();
        let next_date = timezone.to_datetime(next).date();
        let days = first_date
            .until(next_date)
            .map_err(|error| format!("daily timer calendar range is invalid: {error}"))?
            .get_days();
        let mut count = u64::try_from(days)
            .map_err(|_| "daily timer occurrence range is negative".to_owned())?;
        count = count.saturating_sub(self.gaps_between(timezone, first, next)?);
        Ok((unix_micros(next)?, count.max(1)))
    }

    /// Choose UTC or the runtime's single sampled host-local timezone.
    fn timezone<'a>(&self, local_timezone: Option<&'a TimeZone>) -> Result<&'a TimeZone, String> {
        if self.zone == WallClockZone::Utc {
            return Ok(&UTC_TIMEZONE);
        }
        local_timezone.ok_or_else(|| "could not determine the host local timezone".to_owned())
    }

    /// Find the first occurrence in an injected timezone for deterministic
    /// tests.
    fn next_after_in_timezone(
        &self,
        after: UnixMicros,
        timezone: &TimeZone,
    ) -> Result<UnixMicros, String> {
        let after = timestamp(after)?;
        let mut date = timezone.to_datetime(after).date();
        loop {
            let candidate = DateTime::from_parts(date, self.time);
            let ambiguous = timezone.to_ambiguous_timestamp(candidate);
            if !matches!(ambiguous.offset(), AmbiguousOffset::Gap { .. }) {
                let candidate = ambiguous
                    .earlier()
                    .map_err(|error| format!("daily timer occurrence is invalid: {error}"))?;
                if after < candidate {
                    return unix_micros(candidate);
                }
            }
            date = date
                .tomorrow()
                .map_err(|_| "daily timer exceeds the supported calendar range".to_owned())?;
        }
    }

    /// Count scheduled civil datetimes removed by forward timezone jumps.
    fn gaps_between(
        &self,
        timezone: &TimeZone,
        first: Timestamp,
        next: Timestamp,
    ) -> Result<u64, String> {
        let mut gaps = 0_u64;
        let mut before = timezone.to_offset(first);
        for transition in timezone.following(first) {
            if next <= transition.timestamp() {
                break;
            }
            let after = transition.offset();
            let gap_start = before.to_datetime(transition.timestamp());
            let gap_end = after.to_datetime(transition.timestamp());
            if gap_start < gap_end {
                gaps = gaps.saturating_add(self.candidates_in_gap(gap_start, gap_end)?);
            }
            before = after;
        }
        Ok(gaps)
    }

    /// Count this schedule's daily candidates inside one bounded civil gap.
    fn candidates_in_gap(&self, start: DateTime, end: DateTime) -> Result<u64, String> {
        let mut date = start.date();
        let mut count = 0_u64;
        loop {
            let candidate = DateTime::from_parts(date, self.time);
            if start <= candidate && candidate < end {
                count = count.saturating_add(1);
            }
            if end.date() <= date {
                return Ok(count);
            }
            date = date
                .tomorrow()
                .map_err(|_| "timezone gap exceeds the supported calendar range".to_owned())?;
        }
    }
}

/// Parse exactly two ASCII decimal digits.
fn parse_pair(bytes: &[u8]) -> Option<u8> {
    if bytes.len() != 2 || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some((bytes[0] - b'0') * 10 + (bytes[1] - b'0'))
}

/// Convert Tau's nonnegative microsecond timestamp to Jiff.
fn timestamp(value: UnixMicros) -> Result<Timestamp, String> {
    let micros = i64::try_from(value.get())
        .map_err(|_| "timer timestamp exceeds the supported calendar range".to_owned())?;
    Timestamp::from_microsecond(micros)
        .map_err(|_| "timer timestamp exceeds the supported calendar range".to_owned())
}

/// Convert Jiff back to Tau's nonnegative wall-clock timestamp.
fn unix_micros(value: Timestamp) -> Result<UnixMicros, String> {
    let micros = u64::try_from(value.as_microsecond())
        .map_err(|_| "timer timestamp precedes the Unix epoch".to_owned())?;
    Ok(UnixMicros::new(micros))
}
