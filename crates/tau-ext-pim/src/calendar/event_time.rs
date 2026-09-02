//! Validated calendar event write boundaries.

use time::format_description::well_known::Rfc3339;
use time::{Date, OffsetDateTime};

#[cfg(test)]
mod tests;

/// One exact calendar event boundary with parsed comparison state.
enum EventBoundary {
    /// An all-day boundary whose raw date remains provider-compatible.
    AllDay {
        /// Exact date text accepted at the validation boundary.
        raw: String,
        /// Parsed date used for ordering and default construction.
        date: Date,
    },
    /// A timed boundary whose raw instant remains provider-compatible.
    Timed {
        /// Exact RFC3339 text accepted at the validation boundary.
        raw: String,
        /// Parsed instant used for ordering and default construction.
        instant: OffsetDateTime,
    },
}

/// A validated, strictly ordered pair of same-kind calendar event boundaries.
pub(super) struct EventTimeRange {
    /// Inclusive event start.
    start: EventBoundary,
    /// Exclusive event end.
    end: EventBoundary,
}

impl EventBoundary {
    /// Parse one exact normalized or persisted event boundary.
    fn parse(raw: String, field: &str) -> Result<Self, String> {
        if let Some(date) = parse_date(&raw) {
            return Ok(Self::AllDay { raw, date });
        }
        let instant = OffsetDateTime::parse(&raw, &Rfc3339)
            .map_err(|error| format!("{field} must be RFC3339 or YYYY-MM-DD: {error}"))?;
        Ok(Self::Timed { raw, instant })
    }

    /// Return the exact boundary text.
    fn raw(&self) -> &str {
        match self {
            Self::AllDay { raw, .. } | Self::Timed { raw, .. } => raw,
        }
    }
}

impl EventTimeRange {
    /// Validate an exact normalized or persisted start/end pair.
    pub(super) fn from_exact(start: String, end: String) -> Result<Self, String> {
        match (parse_date(&start), parse_date(&end)) {
            (Some(start_date), Some(end_date)) => Self::from_boundaries(
                EventBoundary::AllDay {
                    raw: start,
                    date: start_date,
                },
                EventBoundary::AllDay {
                    raw: end,
                    date: end_date,
                },
            ),
            (None, None) => Self::from_boundaries(
                EventBoundary::parse(start, "start")?,
                EventBoundary::parse(end, "end")?,
            ),
            _ => Err(
                "event start and end must both be all-day dates or both be RFC3339 date-times"
                    .to_owned(),
            ),
        }
    }

    /// Validate a raw Google adapter pair with its historical field parse
    /// order.
    pub(super) fn from_google_exact(start: String, end: String) -> Result<Self, String> {
        let start = EventBoundary::parse(start, "start")?;
        let end = EventBoundary::parse(end, "end")?;
        Self::from_boundaries(start, end)
    }

    /// Build a range by applying the existing create-event default duration.
    pub(super) fn with_default_end(start: String) -> Result<Self, String> {
        let start = EventBoundary::parse(start, "start")?;
        let end = match &start {
            EventBoundary::AllDay { date, .. } => {
                let end = date
                    .next_day()
                    .ok_or_else(|| "default event end is out of range".to_owned())?;
                EventBoundary::AllDay {
                    raw: end.to_string(),
                    date: end,
                }
            }
            EventBoundary::Timed { instant, .. } => {
                let end = instant
                    .checked_add(time::Duration::hours(1))
                    .ok_or_else(|| "default event end is out of range".to_owned())?;
                let raw = end.format(&Rfc3339).map_err(|error| {
                    format!("default event end could not be formatted: {error}")
                })?;
                EventBoundary::Timed { raw, instant: end }
            }
        };
        Self::from_boundaries(start, end)
    }

    /// Return the exact start text.
    pub(super) fn start_raw(&self) -> &str {
        self.start.raw()
    }

    /// Return the exact end text.
    pub(super) fn end_raw(&self) -> &str {
        self.end.raw()
    }

    /// Return whether this range uses all-day dates.
    pub(super) fn is_all_day(&self) -> bool {
        matches!(self.start, EventBoundary::AllDay { .. })
    }

    /// Require same-kind boundaries in strict chronological order.
    fn from_boundaries(start: EventBoundary, end: EventBoundary) -> Result<Self, String> {
        let ordered = match (&start, &end) {
            (
                EventBoundary::AllDay {
                    date: start_date, ..
                },
                EventBoundary::AllDay { date: end_date, .. },
            ) => start_date < end_date,
            (
                EventBoundary::Timed {
                    instant: start_instant,
                    ..
                },
                EventBoundary::Timed {
                    instant: end_instant,
                    ..
                },
            ) => start_instant < end_instant,
            _ => {
                return Err(
                    "event start and end must both be all-day dates or both be RFC3339 date-times"
                        .to_owned(),
                );
            }
        };
        if !ordered {
            return Err("event start must be before event end".to_owned());
        }
        Ok(Self { start, end })
    }
}

/// Parse the exact calendar all-day grammar without accepting wider date forms.
fn parse_date(value: &str) -> Option<Date> {
    let bytes = value.as_bytes();
    if bytes.len() != 10
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || !bytes[..4].iter().all(u8::is_ascii_digit)
        || !bytes[5..7].iter().all(u8::is_ascii_digit)
        || !bytes[8..].iter().all(u8::is_ascii_digit)
    {
        return None;
    }
    let year = value[0..4].parse::<i32>().ok()?;
    let month = time::Month::try_from(value[5..7].parse::<u8>().ok()?).ok()?;
    let day = value[8..10].parse::<u8>().ok()?;
    Date::from_calendar_date(year, month, day).ok()
}
