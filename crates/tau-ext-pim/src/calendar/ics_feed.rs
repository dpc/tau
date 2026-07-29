use std::collections::BTreeMap;
use std::io::Read;
use std::time::Duration;

use calcard::common::timezone::Tz;
use calcard::icalendar::{
    ICalendar, ICalendarComponent, ICalendarComponentType, ICalendarPeriod, ICalendarProperty,
    ICalendarValue,
};
use calcard::{Entry, Parser};
use chrono::TimeZone;
use rrule::{RRule, RRuleSet, Unvalidated};
use tau_proto::SecretValue;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use url::Url;

use super::config::{ValidatedAccount, ValidatedBackendConfig};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_ICS_BYTES: u64 = 2 * 1024 * 1024;
const ICS_CURSOR_PREFIX: &str = "ics:";
const MAX_RANGE_ICS_OCCURRENCES: usize = 50_000;
const MAX_RANGE_ICS_SCAN_OCCURRENCES: usize = 2_000_000;

/// Read-only iCalendar feed backend.
pub struct IcsFeedBackend {
    secrets: BTreeMap<String, SecretValue>,
    agent: ureq::Agent,
}

/// One calendar visible through a backend account.
pub struct BackendCalendar {
    /// Calendar id used in tool calls.
    pub id: String,
    /// User-facing display name.
    pub display_name: String,
    /// Whether the calendar is read-only.
    pub read_only: bool,
}

/// One event parsed from an iCalendar feed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IcsEvent {
    /// Stable event id.
    pub id: String,
    /// iCalendar UID.
    pub uid: String,
    /// Event summary.
    pub summary: String,
    /// Event description.
    pub description: Option<String>,
    /// Event location.
    pub location: Option<String>,
    /// Backend-normalized start value.
    ///
    /// Timed events use UTC RFC3339 internally and are converted to the account
    /// output timezone by the calendar runtime. All-day events preserve
    /// `YYYY-MM-DD` date shape so callers do not treat them as midnight
    /// appointments.
    pub start: String,
    /// Backend-normalized end value.
    ///
    /// Timed events use UTC RFC3339 internally and are converted to the account
    /// output timezone by the calendar runtime. All-day events preserve
    /// `YYYY-MM-DD` date shape so callers do not treat them as midnight
    /// appointments.
    pub end: String,
    /// Parsed start instant, when the event has a concrete time range.
    pub start_utc: Option<OffsetDateTime>,
    /// Parsed end instant, when the event has a concrete time range.
    pub end_utc: Option<OffsetDateTime>,
    /// Event status.
    pub status: Option<String>,
    /// Whether `CLASS` marks this event private/confidential.
    pub private: bool,
    /// Organizer value.
    pub organizer: Option<String>,
    /// Attendee values.
    pub attendees: Vec<String>,
    /// Whether the source component contains recurrence metadata.
    pub recurring: bool,
    /// Whether time filtering could not fully interpret this event's time.
    pub time_unparsed: bool,
}

/// One page of iCalendar feed events.
pub struct IcsEventPage {
    /// Events in this page.
    pub events: Vec<IcsEvent>,
    /// Cursor for the next page, when more matching events remain.
    pub next_cursor: Option<String>,
    /// Whether more matching events remain after this page.
    pub truncated: bool,
}

/// Time range for event and free-busy queries.
#[derive(Clone, Copy, Debug, Default)]
pub struct TimeRange {
    /// Inclusive lower bound.
    pub min: Option<OffsetDateTime>,
    /// Exclusive upper bound.
    pub max: Option<OffsetDateTime>,
}

impl IcsFeedBackend {
    /// Build a backend using the extension-authorized secret set.
    pub fn new(secrets: BTreeMap<String, SecretValue>) -> Self {
        let tls_config = ureq::tls::TlsConfig::builder()
            .root_certs(ureq::tls::RootCerts::PlatformVerifier)
            .build();
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(REQUEST_TIMEOUT))
            .http_status_as_error(false)
            .max_redirects(0)
            .tls_config(tls_config)
            .build();
        let agent = ureq::Agent::new_with_config(config);
        Self { secrets, agent }
    }

    /// List synthetic calendars exposed by an iCalendar feed account.
    pub fn list_calendars(&self, account: &ValidatedAccount) -> Vec<BackendCalendar> {
        account
            .allowed_calendars
            .iter()
            .map(|id| BackendCalendar {
                id: id.clone(),
                display_name: account.display_name.clone().unwrap_or_else(|| id.clone()),
                read_only: true,
            })
            .collect()
    }

    /// List events from the account's feed.
    pub fn list_events(
        &self,
        account: &ValidatedAccount,
        calendar: &str,
        range: TimeRange,
        limit: usize,
    ) -> Result<Vec<IcsEvent>, String> {
        Ok(self
            .list_events_page(account, calendar, range, limit, None)?
            .events)
    }

    /// List one cursor page of events from the account's feed.
    pub fn list_events_page(
        &self,
        account: &ValidatedAccount,
        calendar: &str,
        range: TimeRange,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<IcsEventPage, String> {
        ensure_calendar_allowed(account, calendar)?;
        let offset = parse_ics_cursor(cursor)?;
        let text = self.fetch_feed(account)?;
        let timezone = ics_default_timezone(account)?;
        let mut events = parse_ics_events_in_range(&text, timezone, range)?;
        events.retain(|event| event_overlaps(event, range));
        events.sort_by_key(event_sort_key);
        let truncated = offset.saturating_add(limit) < events.len();
        let next_cursor = if truncated {
            Some(format!("{ICS_CURSOR_PREFIX}{}", offset + limit))
        } else {
            None
        };
        let events = events.into_iter().skip(offset).take(limit).collect();
        Ok(IcsEventPage {
            events,
            next_cursor,
            truncated,
        })
    }

    /// Read one event from the account's feed.
    pub fn read_event(
        &self,
        account: &ValidatedAccount,
        calendar: &str,
        event_id: &str,
    ) -> Result<IcsEvent, String> {
        ensure_calendar_allowed(account, calendar)?;
        let text = self.fetch_feed(account)?;
        let timezone = ics_default_timezone(account)?;
        if let Some(event) = parse_ics_static_events(&text, timezone)?
            .into_iter()
            .find(|event| event.id == event_id)
        {
            return Ok(event);
        }
        let Some(timestamp) = recurring_event_timestamp(event_id) else {
            return Err(format!("calendar event `{event_id}` was not found"));
        };
        let start = OffsetDateTime::from_unix_timestamp(timestamp)
            .map_err(|_| "calendar event id timestamp is out of range".to_owned())?;
        let end = start
            .checked_add(time::Duration::seconds(1))
            .ok_or_else(|| "calendar event id timestamp is out of range".to_owned())?;
        parse_ics_events_in_range(
            &text,
            timezone,
            TimeRange {
                min: Some(start),
                max: Some(end),
            },
        )?
        .into_iter()
        .find(|event| event.id == event_id)
        .ok_or_else(|| format!("calendar event `{event_id}` was not found"))
    }
    fn fetch_feed(&self, account: &ValidatedAccount) -> Result<String, String> {
        let url = self.feed_url(account)?;
        let mut response = self
            .agent
            .get(&url)
            .header("Accept", "text/calendar, text/plain;q=0.8, */*;q=0.1")
            .call()
            .map_err(|error| format!("fetching iCalendar feed failed: {error}"))?;
        if !response.status().is_success() {
            let code = response.status().as_u16();
            return Err(format!("iCalendar feed returned HTTP {code}"));
        }
        let mut bytes = Vec::new();
        response
            .body_mut()
            .as_reader()
            .take(MAX_ICS_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("reading iCalendar feed failed: {error}"))?;
        if MAX_ICS_BYTES < bytes.len() as u64 {
            return Err("iCalendar feed is too large".to_owned());
        }
        String::from_utf8(bytes).map_err(|_| "iCalendar feed is not valid UTF-8".to_owned())
    }

    fn feed_url(&self, account: &ValidatedAccount) -> Result<String, String> {
        let Some(ValidatedBackendConfig::IcsFeed {
            url_secret,
            url,
            allow_plain_http,
        }) = &account.backend
        else {
            return Err(format!(
                "calendar account `{}` is not an ics_feed account",
                account.id
            ));
        };
        let url = match (url_secret, url) {
            (Some(secret), None) => self
                .secrets
                .get(secret)
                .ok_or_else(|| {
                    format!(
                        "calendar account `{}` missing url_secret `{secret}`",
                        account.id
                    )
                })?
                .expose_secret()
                .to_owned(),
            (None, Some(url)) => url.clone(),
            _ => return Err("invalid ics_feed source configuration".to_owned()),
        };
        normalize_feed_url(&url, *allow_plain_http)
    }
}

pub(crate) fn normalize_feed_url(url: &str, allow_plain_http: bool) -> Result<String, String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err("iCalendar feed URL must not be empty".to_owned());
    }
    if trimmed.chars().any(|c| c.is_control()) {
        return Err("iCalendar feed URL must not contain control characters".to_owned());
    }
    let normalized;
    let candidate = if let Some(rest) = trimmed.strip_prefix("webcal://") {
        normalized = format!("https://{rest}");
        normalized.as_str()
    } else {
        trimmed
    };
    let parsed = Url::parse(candidate)
        .map_err(|error| format!("iCalendar feed URL must be absolute: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("iCalendar feed URL must use https://, http://, or webcal://".to_owned());
    }
    let Some(host) = parsed.host_str() else {
        return Err("iCalendar feed URL must include a host".to_owned());
    };
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("iCalendar feed URL must not include credentials".to_owned());
    }
    if parsed.scheme() == "http" && !allow_plain_http && !is_loopback_host(host) {
        return Err(
            "iCalendar feed URL must use https:// or webcal:// unless allow_plain_http is enabled"
                .to_owned(),
        );
    }
    Ok(candidate.to_owned())
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn parse_ics_cursor(cursor: Option<&str>) -> Result<usize, String> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let Some(offset) = cursor.strip_prefix(ICS_CURSOR_PREFIX) else {
        return Err("cursor is not an iCalendar feed cursor returned by this tool".to_owned());
    };
    if offset.is_empty() || !offset.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("iCalendar feed cursor is invalid".to_owned());
    }
    offset
        .parse::<usize>()
        .map_err(|_| "iCalendar feed cursor is too large".to_owned())
}

fn ensure_calendar_allowed(account: &ValidatedAccount, calendar: &str) -> Result<(), String> {
    if account
        .allowed_calendars
        .iter()
        .any(|allowed| allowed == calendar)
    {
        return Ok(());
    }
    Err(format!(
        "calendar `{calendar}` is not allowed for account `{}`",
        account.id
    ))
}

fn ics_default_timezone(account: &ValidatedAccount) -> Result<Tz, String> {
    let Some(timezone) = account.timezone.as_deref() else {
        return Ok(system_ics_timezone().unwrap_or(Tz::UTC));
    };
    timezone.parse::<Tz>().map_err(|_| {
        format!("account timezone `{timezone}` is not recognized for iCalendar feed interpretation")
    })
}

fn system_ics_timezone() -> Option<Tz> {
    system_timezone_name()?.parse::<Tz>().ok()
}

fn system_timezone_name() -> Option<String> {
    if let Ok(value) = std::env::var("TZ") {
        let value = value.trim().trim_start_matches(':');
        if !value.is_empty() {
            return Some(value.to_owned());
        }
    }
    let path = std::fs::read_link("/etc/localtime").ok()?;
    let text = path.to_string_lossy();
    text.split("zoneinfo/").nth(1).map(str::to_owned)
}

#[cfg(test)]
fn parse_ics_events(text: &str) -> Result<Vec<IcsEvent>, String> {
    parse_ics_events_in_range(
        text,
        Tz::UTC,
        TimeRange {
            min: Some(OffsetDateTime::parse("1900-01-01T00:00:00Z", &Rfc3339).expect("time")),
            max: Some(OffsetDateTime::parse("2100-01-01T00:00:00Z", &Rfc3339).expect("time")),
        },
    )
}

struct RangeEventSeed<'a> {
    component: &'a ICalendarComponent,
    uid: String,
    start: chrono::DateTime<Tz>,
    duration: chrono::Duration,
    all_day: bool,
    rrules: Vec<String>,
    rdates: Vec<chrono::DateTime<Tz>>,
    exdates: std::collections::BTreeSet<i64>,
    recurrence_id: Option<i64>,
}

fn parse_ics_events_in_range(
    text: &str,
    timezone: Tz,
    range: TimeRange,
) -> Result<Vec<IcsEvent>, String> {
    let (Some(range_min), Some(range_max)) = (range.min, range.max) else {
        return Err("iCalendar list_events requires a bounded range".to_owned());
    };
    let mut parser = Parser::new(text);
    let mut events = Vec::new();
    loop {
        match parser.entry() {
            Entry::ICalendar(calendar) => {
                events.extend(expand_calendar_events_in_range(
                    &calendar, timezone, range_min, range_max,
                )?);
            }
            Entry::UnexpectedComponentEnd { expected, found } => {
                return Err(format!(
                    "iCalendar component ended as `{}` while `{}` was open",
                    found.as_str(),
                    expected.as_str()
                ));
            }
            Entry::UnterminatedComponent(component) => {
                return Err(format!(
                    "iCalendar component `{component}` was not terminated"
                ));
            }
            Entry::TooManyComponents => {
                return Err("iCalendar feed contains too many components".to_owned());
            }
            Entry::VCard(_) => {}
            Entry::Eof => break,
            _ => {}
        }
    }
    Ok(events)
}

fn parse_ics_static_events(text: &str, timezone: Tz) -> Result<Vec<IcsEvent>, String> {
    let mut parser = Parser::new(text);
    let mut events = Vec::new();
    loop {
        match parser.entry() {
            Entry::ICalendar(calendar) => {
                let resolver = calendar.build_tz_resolver().with_default(timezone);
                for (index, component) in calendar.components.iter().enumerate() {
                    if component.component_type != ICalendarComponentType::VEvent {
                        continue;
                    }
                    let Some(seed) = build_range_event_seed(component, index, &resolver) else {
                        continue;
                    };
                    if seed.recurrence_id.is_none()
                        && seed.rrules.is_empty()
                        && seed.rdates.is_empty()
                    {
                        push_seed_occurrence_if_overlaps(
                            &seed,
                            seed.start,
                            OffsetDateTime::from_unix_timestamp(-5_364_662_400)
                                .unwrap_or(OffsetDateTime::UNIX_EPOCH),
                            OffsetDateTime::from_unix_timestamp(253_402_300_799)
                                .unwrap_or(OffsetDateTime::UNIX_EPOCH),
                            &mut events,
                        );
                    }
                }
            }
            Entry::UnexpectedComponentEnd { expected, found } => {
                return Err(format!(
                    "iCalendar component ended as `{}` while `{}` was open",
                    found.as_str(),
                    expected.as_str()
                ));
            }
            Entry::UnterminatedComponent(component) => {
                return Err(format!(
                    "iCalendar component `{component}` was not terminated"
                ));
            }
            Entry::TooManyComponents => {
                return Err("iCalendar feed contains too many components".to_owned());
            }
            Entry::VCard(_) => {}
            Entry::Eof => break,
            _ => {}
        }
    }
    Ok(events)
}

fn recurring_event_timestamp(event_id: &str) -> Option<i64> {
    event_id.rsplit_once('#')?.1.parse::<i64>().ok()
}

fn expand_calendar_events_in_range(
    calendar: &ICalendar,
    timezone: Tz,
    range_min: OffsetDateTime,
    range_max: OffsetDateTime,
) -> Result<Vec<IcsEvent>, String> {
    let resolver = calendar.build_tz_resolver().with_default(timezone);
    let mut masters = Vec::new();
    let mut overrides = BTreeMap::<String, BTreeMap<i64, RangeEventSeed<'_>>>::new();
    for (index, component) in calendar.components.iter().enumerate() {
        if component.component_type != ICalendarComponentType::VEvent {
            continue;
        }
        let Some(seed) = build_range_event_seed(component, index, &resolver) else {
            continue;
        };
        if let Some(recurrence_id) = seed.recurrence_id {
            overrides
                .entry(seed.uid.clone())
                .or_default()
                .insert(recurrence_id, seed);
        } else {
            masters.push(seed);
        }
    }

    let mut events = Vec::new();
    let mut emitted_override_ids = std::collections::BTreeSet::new();
    for master in &masters {
        let override_group = overrides.get(&master.uid);
        expand_master_in_range(
            master,
            override_group,
            range_min,
            range_max,
            &mut emitted_override_ids,
            &mut events,
        )?;
    }
    for override_group in overrides.values() {
        for (recurrence_id, override_seed) in override_group {
            if emitted_override_ids
                .contains(&override_event_key(&override_seed.uid, *recurrence_id))
                || is_cancelled(override_seed.component)
            {
                continue;
            }
            push_seed_occurrence_if_overlaps(
                override_seed,
                override_seed.start,
                range_min,
                range_max,
                &mut events,
            );
        }
    }
    Ok(events)
}

fn build_range_event_seed<'a>(
    component: &'a ICalendarComponent,
    comp_index: usize,
    resolver: &calcard::icalendar::timezone::TzResolver<&str>,
) -> Option<RangeEventSeed<'a>> {
    let mut dt_start = None;
    let mut dt_start_tzid = None;
    let mut dt_end = None;
    let mut recurrence_id = None;
    let mut recurrence_tzid = None;
    let mut rrules = Vec::new();
    let mut rdates = Vec::new();
    let mut exdates = std::collections::BTreeSet::new();
    for entry in &component.entries {
        match (&entry.name, entry.values.first()) {
            (ICalendarProperty::Dtstart, Some(ICalendarValue::PartialDateTime(dt))) => {
                dt_start = dt.to_date_time();
                dt_start_tzid = entry.tz_id();
            }
            (ICalendarProperty::Dtend, Some(ICalendarValue::PartialDateTime(dt))) => {
                dt_end = dt.to_date_time();
            }
            (ICalendarProperty::RecurrenceId, Some(ICalendarValue::PartialDateTime(dt))) => {
                recurrence_id = dt.to_date_time();
                recurrence_tzid = entry.tz_id();
            }
            (ICalendarProperty::Rrule, Some(ICalendarValue::RecurrenceRule(rule))) => {
                rrules.push(rule.to_string());
            }
            (ICalendarProperty::Rdate, _) => {
                let tz_id = entry.tz_id().or(dt_start_tzid);
                for value in &entry.values {
                    if let Some(date) = value_partial_datetime(value)
                        .and_then(|dt| dt.to_date_time())
                        .and_then(|dt| dt.to_date_time_with_tz(resolver.resolve_or_default(tz_id)))
                    {
                        rdates.push(date);
                    }
                }
            }
            (ICalendarProperty::Exdate, _) => {
                let tz_id = entry.tz_id().or(dt_start_tzid);
                for value in &entry.values {
                    if let Some(date) = value_partial_datetime(value)
                        .and_then(|dt| dt.to_date_time())
                        .and_then(|dt| dt.to_date_time_with_tz(resolver.resolve_or_default(tz_id)))
                    {
                        exdates.insert(date.timestamp());
                    }
                }
            }
            _ => {}
        }
    }
    let dt_start = dt_start?;
    let start_tz = resolver.resolve_or_default(dt_start_tzid);
    let start = dt_start.to_date_time_with_tz(start_tz)?;
    let duration = if let Some(dt_end) = dt_end {
        dt_end.date_time - dt_start.date_time
    } else if component_is_all_day(component) {
        chrono::Duration::days(1)
    } else {
        chrono::Duration::zero()
    };
    let recurrence_id = recurrence_id
        .and_then(|dt| {
            dt.to_date_time_with_tz(resolver.resolve_or_default(recurrence_tzid.or(dt_start_tzid)))
        })
        .map(|dt| dt.timestamp());
    Some(RangeEventSeed {
        component,
        uid: component_uid(component, comp_index),
        start,
        duration,
        all_day: component_is_all_day(component),
        rrules,
        rdates,
        exdates,
        recurrence_id,
    })
}

fn value_partial_datetime(value: &ICalendarValue) -> Option<&calcard::common::PartialDateTime> {
    match value {
        ICalendarValue::PartialDateTime(dt) => Some(dt),
        ICalendarValue::Period(ICalendarPeriod::Range { start, .. }) => Some(start),
        ICalendarValue::Period(ICalendarPeriod::Duration { start, .. }) => Some(start),
        _ => None,
    }
}

fn expand_master_in_range(
    master: &RangeEventSeed<'_>,
    overrides: Option<&BTreeMap<i64, RangeEventSeed<'_>>>,
    range_min: OffsetDateTime,
    range_max: OffsetDateTime,
    emitted_override_ids: &mut std::collections::BTreeSet<String>,
    events: &mut Vec<IcsEvent>,
) -> Result<(), String> {
    if master.rrules.is_empty() && master.rdates.is_empty() {
        push_seed_occurrence_if_overlaps(master, master.start, range_min, range_max, events);
        return Ok(());
    }
    let mut starts = std::collections::BTreeMap::<i64, chrono::DateTime<Tz>>::new();
    starts.insert(master.start.timestamp(), master.start);
    for rdate in &master.rdates {
        starts.insert(rdate.timestamp(), *rdate);
    }
    if !master.rrules.is_empty() {
        let rrule_tz = rrule_timezone(master.start.timezone());
        let dt_start = rrule_datetime(master.start, rrule_tz)?;
        let after = offset_to_rrule_datetime(range_min, rrule_tz, -master.duration)?;
        let before = offset_to_rrule_datetime(range_max, rrule_tz, chrono::Duration::zero())?;
        let mut set = RRuleSet::new(dt_start).limit();
        for rule in &master.rrules {
            let rule = rule
                .parse::<RRule<Unvalidated>>()
                .map_err(|_| capped_range_error())?
                .validate(dt_start)
                .map_err(|_| capped_range_error())?;
            set = set.rrule(rule);
        }
        let mut scanned = 0_usize;
        let mut matched = 0_usize;
        for date in &set {
            scanned = scanned.saturating_add(1);
            if MAX_RANGE_ICS_SCAN_OCCURRENCES < scanned {
                return Err(capped_range_error());
            }
            if !rrule_datetime_is_before(date, before) {
                break;
            }
            if !rrule_datetime_is_before(after, date) {
                continue;
            }
            matched = matched.saturating_add(1);
            if MAX_RANGE_ICS_OCCURRENCES < matched {
                return Err(capped_range_error());
            }
            if let Some(start) =
                calcard_datetime_from_timestamp(date.timestamp(), master.start.timezone())
            {
                starts.insert(start.timestamp(), start);
            }
        }
    }
    for (timestamp, start) in starts {
        if master.exdates.contains(&timestamp) {
            continue;
        }
        if let Some(override_seed) = overrides.and_then(|overrides| overrides.get(&timestamp)) {
            emitted_override_ids.insert(override_event_key(&override_seed.uid, timestamp));
            if is_cancelled(override_seed.component) {
                continue;
            }
            push_seed_occurrence_if_overlaps(
                override_seed,
                override_seed.start,
                range_min,
                range_max,
                events,
            );
        } else {
            push_seed_occurrence_if_overlaps(master, start, range_min, range_max, events);
        }
    }
    Ok(())
}

fn push_seed_occurrence_if_overlaps(
    seed: &RangeEventSeed<'_>,
    start: chrono::DateTime<Tz>,
    range_min: OffsetDateTime,
    range_max: OffsetDateTime,
    events: &mut Vec<IcsEvent>,
) {
    let Some(start_utc) = chrono_to_offset_time(start) else {
        return;
    };
    let Some(end_utc) = OffsetDateTime::from_unix_timestamp(
        start
            .timestamp()
            .saturating_add(seed.duration.num_seconds()),
    )
    .ok() else {
        return;
    };
    if !event_times_overlap(start_utc, end_utc, range_min, range_max) {
        return;
    }
    let recurring =
        !seed.rrules.is_empty() || !seed.rdates.is_empty() || seed.recurrence_id.is_some();
    let id = if recurring {
        format!("{}#{}", seed.uid, start_utc.unix_timestamp())
    } else {
        seed.uid.clone()
    };
    events.push(IcsEvent {
        id,
        uid: seed.uid.clone(),
        summary: first_property_text(seed.component, &ICalendarProperty::Summary)
            .unwrap_or_else(|| "(untitled)".to_owned()),
        description: first_property_text(seed.component, &ICalendarProperty::Description),
        location: first_property_text(seed.component, &ICalendarProperty::Location),
        start: format_range_event_time(seed, start_utc, start),
        end: format_range_event_time(seed, end_utc, start + seed.duration),
        start_utc: Some(start_utc),
        end_utc: Some(end_utc),
        status: first_property_text(seed.component, &ICalendarProperty::Status),
        private: first_property_text(seed.component, &ICalendarProperty::Class).is_some_and(
            |class| {
                class.eq_ignore_ascii_case("PRIVATE") || class.eq_ignore_ascii_case("CONFIDENTIAL")
            },
        ),
        organizer: first_property_text(seed.component, &ICalendarProperty::Organizer),
        attendees: property_texts(seed.component, &ICalendarProperty::Attendee).collect(),
        recurring,
        time_unparsed: false,
    });
}

fn event_times_overlap(
    start: OffsetDateTime,
    end: OffsetDateTime,
    range_min: OffsetDateTime,
    range_max: OffsetDateTime,
) -> bool {
    is_before(range_min, end) && is_before(start, range_max)
}

fn format_range_event_time(
    seed: &RangeEventSeed<'_>,
    utc: OffsetDateTime,
    local: chrono::DateTime<Tz>,
) -> String {
    if seed.all_day {
        local.format("%Y-%m-%d").to_string()
    } else {
        format_offset_time(utc)
    }
}

fn is_cancelled(component: &ICalendarComponent) -> bool {
    first_property_text(component, &ICalendarProperty::Status)
        .is_some_and(|status| status.eq_ignore_ascii_case("CANCELLED"))
}

fn override_event_key(uid: &str, recurrence_id: i64) -> String {
    format!("{uid}#{recurrence_id}")
}

fn rrule_timezone(tz: Tz) -> rrule::Tz {
    tz.name()
        .as_deref()
        .and_then(|name| name.parse::<chrono_tz::Tz>().ok())
        .map(rrule::Tz::Tz)
        .unwrap_or(rrule::Tz::UTC)
}

fn rrule_datetime(
    date: chrono::DateTime<Tz>,
    timezone: rrule::Tz,
) -> Result<chrono::DateTime<rrule::Tz>, String> {
    timezone
        .timestamp_opt(date.timestamp(), date.timestamp_subsec_nanos())
        .single()
        .ok_or_else(capped_range_error)
}

fn offset_to_rrule_datetime(
    date: OffsetDateTime,
    timezone: rrule::Tz,
    offset: chrono::Duration,
) -> Result<chrono::DateTime<rrule::Tz>, String> {
    timezone
        .timestamp_opt(
            date.unix_timestamp().saturating_add(offset.num_seconds()),
            date.nanosecond(),
        )
        .single()
        .ok_or_else(capped_range_error)
}

fn calcard_datetime_from_timestamp(timestamp: i64, timezone: Tz) -> Option<chrono::DateTime<Tz>> {
    timezone.timestamp_opt(timestamp, 0).single()
}

fn rrule_datetime_is_before(
    left: chrono::DateTime<rrule::Tz>,
    right: chrono::DateTime<rrule::Tz>,
) -> bool {
    left.timestamp() < right.timestamp()
        || (left.timestamp() == right.timestamp()
            && left.timestamp_subsec_nanos() < right.timestamp_subsec_nanos())
}

fn capped_range_error() -> String {
    "iCalendar recurrence expansion for the requested range exceeded the safety limit; narrow the requested range".to_owned()
}

fn component_uid(component: &ICalendarComponent, index: usize) -> String {
    component
        .uid()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("ics-component-{index}"))
}

fn first_property_text(
    component: &ICalendarComponent,
    property: &ICalendarProperty,
) -> Option<String> {
    property_texts(component, property).next()
}

fn property_texts<'a>(
    component: &'a ICalendarComponent,
    property: &'a ICalendarProperty,
) -> impl Iterator<Item = String> + 'a {
    component.properties(property).filter_map(|entry| {
        entry
            .values
            .first()
            .and_then(ICalendarValue::as_text)
            .map(str::to_owned)
    })
}

fn component_is_all_day(component: &ICalendarComponent) -> bool {
    component
        .property(&ICalendarProperty::Dtstart)
        .and_then(|entry| entry.values.first())
        .and_then(ICalendarValue::as_partial_date_time)
        .is_some_and(|dt| dt.hour.is_none())
}

fn format_offset_time(time: OffsetDateTime) -> String {
    // Preserve this behavior; the structural alternative is not semantics-neutral
    // here. ast-grep-ignore: silent-unwrap-or-else
    time.format(&Rfc3339).unwrap_or_else(|_| time.to_string())
}

fn chrono_to_offset_time(time: chrono::DateTime<Tz>) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp(time.timestamp()).ok()
}

fn event_overlaps(event: &IcsEvent, range: TimeRange) -> bool {
    let (Some(start), Some(end)) = (event.start_utc, event.end_utc) else {
        return false;
    };
    if let Some(min) = range.min
        && !is_before(min, end)
    {
        return false;
    }
    if let Some(max) = range.max
        && !is_before(start, max)
    {
        return false;
    }
    true
}

fn event_sort_key(event: &IcsEvent) -> Option<OffsetDateTime> {
    event.start_utc
}

fn is_before(left: OffsetDateTime, right: OffsetDateTime) -> bool {
    left < right
}

#[cfg(test)]
mod tests;
