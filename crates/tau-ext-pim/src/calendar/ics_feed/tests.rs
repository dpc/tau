use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use super::*;
use crate::calendar::config::{
    CalendarAccountConfig, CalendarBackendConfig, CalendarExtensionConfig, CalendarSelectionConfig,
};

#[test]
fn parser_unfolds_and_extracts_basic_events() {
    let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:abc\r\nSUMMARY:Hello\r\n world\r\nDTSTART:20260528T120000Z\r\nDTEND:20260528T130000Z\r\nLOCATION:Room\\, 1\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    let events = parse_ics_events(ics).expect("ics parses");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id, "abc");
    assert_eq!(events[0].summary, "Helloworld");
    assert_eq!(events[0].location.as_deref(), Some("Room, 1"));
    assert!(events[0].start_utc.is_some());
}

#[test]
fn parser_resolves_tzid_times() {
    // Proton and Google feeds commonly use TZID-qualified local times. These
    // must be converted to concrete instants instead of being treated as
    // unfilterable events that match every queried range.
    let ics = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:abc\nSUMMARY:Local\nDTSTART;TZID=America/Chicago:20260528T120000\nDTEND;TZID=America/Chicago:20260528T130000\nEND:VEVENT\nEND:VCALENDAR\n";

    let events = parse_ics_events(ics).expect("ics parses");

    assert_eq!(events[0].start, "2026-05-28T17:00:00Z");
    assert!(!events[0].time_unparsed);
}

#[test]
fn range_filter_excludes_old_tzid_events() {
    // Regression coverage for the stale-calendar leak: old local-time ICS
    // events must not pass a modern range filter just because their original
    // DTSTART was not UTC-suffixed.
    let ics = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:old\nSUMMARY:Old\nDTSTART;TZID=America/Los_Angeles:20210811T093000\nDTEND;TZID=America/Los_Angeles:20210811T103000\nEND:VEVENT\nEND:VCALENDAR\n";
    let event = parse_ics_events(ics)
        .expect("ics parses")
        .into_iter()
        .next()
        .expect("event");
    let start = OffsetDateTime::parse("2026-06-02T00:00:00Z", &Rfc3339).expect("time");
    let end = OffsetDateTime::parse("2026-06-09T00:00:00Z", &Rfc3339).expect("time");

    assert!(!event_overlaps(
        &event,
        TimeRange {
            min: Some(start),
            max: Some(end)
        }
    ));
}

#[test]
fn parser_preserves_all_day_dates_in_account_timezone() {
    // All-day ICS events are date values, not midnight UTC timed events. The
    // list output should keep date-shaped values so models do not invent a
    // time of day, and range checks should use the configured account timezone.
    let ics = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:all-day\nSUMMARY:All Day\nDTSTART;VALUE=DATE:20260608\nDTEND;VALUE=DATE:20260609\nEND:VEVENT\nEND:VCALENDAR\n";
    let timezone = "America/Los_Angeles".parse::<Tz>().expect("timezone");
    let event = parse_ics_events_in_range(
        ics,
        timezone,
        TimeRange {
            min: Some(OffsetDateTime::parse("2026-06-08T00:00:00Z", &Rfc3339).expect("time")),
            max: Some(OffsetDateTime::parse("2026-06-09T00:00:00Z", &Rfc3339).expect("time")),
        },
    )
    .expect("ics parses")
    .into_iter()
    .next()
    .expect("event");
    let start = OffsetDateTime::parse("2026-06-09T07:00:00Z", &Rfc3339).expect("time");
    let end = OffsetDateTime::parse("2026-06-10T07:00:00Z", &Rfc3339).expect("time");

    assert_eq!(event.start, "2026-06-08");
    assert_eq!(event.end, "2026-06-09");
    assert!(!event_overlaps(
        &event,
        TimeRange {
            min: Some(start),
            max: Some(end)
        }
    ));
}

#[test]
fn bounded_range_expands_old_daily_recurrence() {
    let ics = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:forever\nSUMMARY:Forever\nDTSTART:19000101T000000Z\nDTEND:19000101T010000Z\nRRULE:FREQ=DAILY\nEND:VEVENT\nEND:VCALENDAR\n";
    let start = OffsetDateTime::parse("2026-06-02T00:00:00Z", &Rfc3339).expect("time");
    let end = OffsetDateTime::parse("2026-06-03T00:00:00Z", &Rfc3339).expect("time");

    let events = parse_ics_events_in_range(
        ics,
        Tz::UTC,
        TimeRange {
            min: Some(start),
            max: Some(end),
        },
    )
    .expect("bounded recurrence expansion should reach requested day");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].start, "2026-06-02T00:00:00Z");
}

#[test]
fn bounded_range_expands_old_hourly_recurrence_without_global_prefix_error() {
    // Regression for real Proton feeds: a high-frequency recurrence that began
    // years ago must be expanded for the requested range instead of spending a
    // calendar-global prefix cap before reaching modern dates.
    let ics = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:hourly\nSUMMARY:Hourly\nDTSTART:19000101T000000Z\nDTEND:19000101T010000Z\nRRULE:FREQ=HOURLY\nEND:VEVENT\nEND:VCALENDAR\n";
    let start = OffsetDateTime::parse("2026-06-02T00:00:00Z", &Rfc3339).expect("time");
    let end = OffsetDateTime::parse("2026-06-03T00:00:00Z", &Rfc3339).expect("time");

    let events = parse_ics_events_in_range(
        ics,
        Tz::UTC,
        TimeRange {
            min: Some(start),
            max: Some(end),
        },
    )
    .expect("bounded recurrence expansion should not hit historical prefix cap");

    assert_eq!(events.len(), 24);
    assert_eq!(events[0].start, "2026-06-02T00:00:00Z");
}

#[test]
fn old_minutely_recurrence_before_range_fails_fast() {
    // The rrule crate does not fast-forward to `after` for iterator use. Guard
    // historical high-frequency feeds with a pre-range scan cap instead of
    // hanging while trying to reach the requested range.
    let ics = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:minutely\nSUMMARY:Private old dense event\nDTSTART:19000101T000000Z\nDTEND:19000101T000100Z\nRRULE:FREQ=MINUTELY\nEND:VEVENT\nEND:VCALENDAR\n";
    let start = OffsetDateTime::parse("2026-06-02T00:00:00Z", &Rfc3339).expect("time");
    let end = OffsetDateTime::parse("2026-06-02T01:00:00Z", &Rfc3339).expect("time");

    let err = parse_ics_events_in_range(
        ics,
        Tz::UTC,
        TimeRange {
            min: Some(start),
            max: Some(end),
        },
    )
    .expect_err("old dense recurrence should fail visibly instead of hanging");

    assert!(err.contains("requested range"));
    assert!(!err.contains("Private old dense event"));
    assert!(!err.contains("minutely"));
}

#[test]
fn dense_recurrence_inside_requested_range_is_visible_error() {
    // Range-bounded expansion should fail only when the requested range itself
    // is too dense to expand safely, and the error must stay sanitized.
    let ics = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:secondly\nSUMMARY:Private dense event\nDTSTART:20260602T000000Z\nDTEND:20260602T000001Z\nRRULE:FREQ=SECONDLY\nEND:VEVENT\nEND:VCALENDAR\n";
    let start = OffsetDateTime::parse("2026-06-02T00:00:00Z", &Rfc3339).expect("time");
    let end = OffsetDateTime::parse("2026-06-03T00:00:00Z", &Rfc3339).expect("time");

    let err = parse_ics_events_in_range(
        ics,
        Tz::UTC,
        TimeRange {
            min: Some(start),
            max: Some(end),
        },
    )
    .expect_err("dense in-range recurrence should fail visibly");

    assert!(err.contains("requested range"));
    assert!(!err.contains("Private dense event"));
    assert!(!err.contains("secondly"));
}

#[test]
fn cancelled_orphan_override_is_not_emitted() {
    let ics = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:cancelled\nSUMMARY:Cancelled\nSTATUS:CANCELLED\nRECURRENCE-ID:20260602T120000Z\nDTSTART:20260602T120000Z\nDTEND:20260602T130000Z\nEND:VEVENT\nEND:VCALENDAR\n";
    let start = OffsetDateTime::parse("2026-06-02T00:00:00Z", &Rfc3339).expect("time");
    let end = OffsetDateTime::parse("2026-06-03T00:00:00Z", &Rfc3339).expect("time");

    let events = parse_ics_events_in_range(
        ics,
        Tz::UTC,
        TimeRange {
            min: Some(start),
            max: Some(end),
        },
    )
    .expect("ics parses");

    assert!(events.is_empty());
}

#[test]
fn range_filter_uses_exclusive_event_overlap() {
    let ics = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:abc\nSUMMARY:UTC\nDTSTART:20260528T120000Z\nDTEND:20260528T130000Z\nEND:VEVENT\nEND:VCALENDAR\n";
    let event = parse_ics_events(ics)
        .expect("ics parses")
        .into_iter()
        .next()
        .expect("event");
    let before = OffsetDateTime::parse("2026-05-28T13:00:00Z", &Rfc3339).expect("time");
    let after = OffsetDateTime::parse("2026-05-28T14:00:00Z", &Rfc3339).expect("time");

    assert!(!event_overlaps(
        &event,
        TimeRange {
            min: Some(before),
            max: Some(after)
        }
    ));
}

#[test]
fn backend_fetches_and_lists_http_feed_events() {
    let ics = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:abc\nSUMMARY:UTC\nDTSTART:20260528T120000Z\nDTEND:20260528T130000Z\nEND:VEVENT\nEND:VCALENDAR\n";
    let (url, handle) = serve_ics_once(ics);
    let cfg = CalendarExtensionConfig {
        enable: true,
        accounts: vec![CalendarAccountConfig {
            id: "feed".to_owned(),
            enable: true,
            backend: Some(CalendarBackendConfig::IcsFeed {
                url_secret: None,
                url: Some(url),
                allow_plain_http: false,
            }),
            calendars: CalendarSelectionConfig {
                default: Some("main".to_owned()),
                allow: vec!["main".to_owned()],
            },
            ..Default::default()
        }],
        ..Default::default()
    };
    let config = cfg.validate().expect("valid config");
    let account = config.accounts.get("feed").expect("feed account");
    let backend = IcsFeedBackend::new(BTreeMap::new());

    let events = backend
        .list_events(
            account,
            "main",
            TimeRange {
                min: Some(OffsetDateTime::parse("2026-05-28T00:00:00Z", &Rfc3339).expect("time")),
                max: Some(OffsetDateTime::parse("2026-05-29T00:00:00Z", &Rfc3339).expect("time")),
            },
            10,
        )
        .expect("events list");
    handle.join().expect("server exits");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].summary, "UTC");
}

/// Plain HTTP calendar feeds can leak private feed tokens and allow event
/// injection. Keep non-loopback HTTP blocked unless the account explicitly
/// opts into that dangerous transport for a trusted deployment.
#[test]
fn non_loopback_http_feed_requires_explicit_opt_in() {
    assert!(normalize_feed_url("http://example.test/calendar.ics", false).is_err());
    assert_eq!(
        normalize_feed_url("http://example.test/calendar.ics", true).expect("opted in"),
        "http://example.test/calendar.ics"
    );
    assert_eq!(
        normalize_feed_url("http://127.0.0.1/calendar.ics", false).expect("loopback"),
        "http://127.0.0.1/calendar.ics"
    );
}

/// The feed fetcher must not follow redirects without re-validating the target
/// URL. Reject redirects outright so an allowed HTTPS/loopback/secret URL
/// cannot downgrade to non-loopback plain HTTP behind the extension's policy
/// checks.
#[test]
fn feed_fetch_rejects_redirects_for_literal_and_secret_urls() {
    let (literal_url, literal_handle) = serve_redirect_once("http://example.test/calendar.ics");
    let literal_cfg = CalendarExtensionConfig {
        enable: true,
        accounts: vec![CalendarAccountConfig {
            id: "feed".to_owned(),
            enable: true,
            backend: Some(CalendarBackendConfig::IcsFeed {
                url_secret: None,
                url: Some(literal_url),
                allow_plain_http: false,
            }),
            calendars: CalendarSelectionConfig {
                default: Some("main".to_owned()),
                allow: vec!["main".to_owned()],
            },
            ..Default::default()
        }],
        ..Default::default()
    };
    let literal_config = literal_cfg.validate().expect("literal config");
    let literal_account = literal_config.accounts.get("feed").expect("literal feed");
    let literal_backend = IcsFeedBackend::new(BTreeMap::new());

    let err = literal_backend
        .list_events(literal_account, "main", TimeRange::default(), 10)
        .expect_err("redirect is rejected");
    literal_handle.join().expect("literal server exits");
    assert!(err.contains("HTTP 302"), "{err}");

    let (secret_url, secret_handle) = serve_redirect_once("http://example.test/calendar.ics");
    let secret_cfg = CalendarExtensionConfig {
        enable: true,
        accounts: vec![CalendarAccountConfig {
            id: "feed".to_owned(),
            enable: true,
            backend: Some(CalendarBackendConfig::IcsFeed {
                url_secret: Some("feed_url".to_owned()),
                url: None,
                allow_plain_http: false,
            }),
            calendars: CalendarSelectionConfig {
                default: Some("main".to_owned()),
                allow: vec!["main".to_owned()],
            },
            ..Default::default()
        }],
        ..Default::default()
    };
    let secret_config = secret_cfg.validate().expect("secret config");
    let secret_account = secret_config.accounts.get("feed").expect("secret feed");
    let mut secrets = BTreeMap::new();
    secrets.insert("feed_url".to_owned(), SecretValue::new(secret_url));
    let secret_backend = IcsFeedBackend::new(secrets);

    let err = secret_backend
        .list_events(secret_account, "main", TimeRange::default(), 10)
        .expect_err("secret redirect is rejected");
    secret_handle.join().expect("secret server exits");
    assert!(err.contains("HTTP 302"), "{err}");
}

#[test]
fn backend_lists_ics_events_with_cursor_pages() {
    // Cursor paging should let the model continue a bounded calendar read
    // without asking for an unbounded feed dump.
    let ics = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:first\nSUMMARY:First\nDTSTART:20260528T120000Z\nDTEND:20260528T130000Z\nEND:VEVENT\nBEGIN:VEVENT\nUID:second\nSUMMARY:Second\nDTSTART:20260529T120000Z\nDTEND:20260529T130000Z\nEND:VEVENT\nEND:VCALENDAR\n";
    let (url, handle) = serve_ics_once(ics);
    let cfg = CalendarExtensionConfig {
        enable: true,
        accounts: vec![CalendarAccountConfig {
            id: "feed".to_owned(),
            enable: true,
            backend: Some(CalendarBackendConfig::IcsFeed {
                url_secret: None,
                url: Some(url),
                allow_plain_http: false,
            }),
            calendars: CalendarSelectionConfig {
                default: Some("main".to_owned()),
                allow: vec!["main".to_owned()],
            },
            ..Default::default()
        }],
        ..Default::default()
    };
    let config = cfg.validate().expect("valid config");
    let account = config.accounts.get("feed").expect("feed account");
    let backend = IcsFeedBackend::new(BTreeMap::new());

    let page = backend
        .list_events_page(
            account,
            "main",
            TimeRange {
                min: Some(OffsetDateTime::parse("2026-05-28T00:00:00Z", &Rfc3339).expect("time")),
                max: Some(OffsetDateTime::parse("2026-05-30T00:00:00Z", &Rfc3339).expect("time")),
            },
            1,
            None,
        )
        .expect("events page");
    handle.join().expect("server exits");
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].summary, "First");
    assert_eq!(page.next_cursor.as_deref(), Some("ics:1"));
    assert!(page.truncated);
}

#[test]
fn ics_cursor_rejects_other_backend_cursors() {
    // Cursor values are intentionally backend-prefixed so agents cannot
    // accidentally replay a Google page token into an ICS feed query.
    assert_eq!(parse_ics_cursor(None), Ok(0));
    assert_eq!(parse_ics_cursor(Some("ics:12")), Ok(12));
    assert!(parse_ics_cursor(Some("google:token")).is_err());
}

#[test]
fn read_event_prefers_static_id_before_recurring_suffix_parse() {
    let ics = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:foo#123\nSUMMARY:Static with hash\nDTSTART:20260528T120000Z\nDTEND:20260528T130000Z\nEND:VEVENT\nEND:VCALENDAR\n";
    let (url, handle) = serve_ics_once(ics);
    let cfg = CalendarExtensionConfig {
        enable: true,
        accounts: vec![CalendarAccountConfig {
            id: "feed".to_owned(),
            enable: true,
            backend: Some(CalendarBackendConfig::IcsFeed {
                url_secret: None,
                url: Some(url),
                allow_plain_http: false,
            }),
            calendars: CalendarSelectionConfig {
                default: Some("main".to_owned()),
                allow: vec!["main".to_owned()],
            },
            ..Default::default()
        }],
        ..Default::default()
    };
    let config = cfg.validate().expect("valid config");
    let account = config.accounts.get("feed").expect("feed account");
    let backend = IcsFeedBackend::new(BTreeMap::new());

    let event = backend
        .read_event(account, "main", "foo#123")
        .expect("event reads by exact static id");
    handle.join().expect("server exits");

    assert_eq!(event.summary, "Static with hash");
}

fn serve_ics_once(body: &'static str) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let addr = listener.local_addr().expect("local addr");
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept request");
        let mut buf = [0_u8; 1024];
        // This call is intentionally best-effort; preserve the existing discarded
        // result. ast-grep-ignore: let-underscore-call
        let _ = stream.read(&mut buf);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/calendar\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream
            .write_all(response.as_bytes())
            .expect("write response");
    });
    (format!("http://{addr}/calendar.ics"), handle)
}

fn serve_redirect_once(location: &'static str) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let addr = listener.local_addr().expect("local addr");
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept request");
        let mut buf = [0_u8; 1024];
        // This call is intentionally best-effort; preserve the existing discarded
        // result. ast-grep-ignore: let-underscore-call
        let _ = stream.read(&mut buf);
        let response =
            format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nConnection: close\r\n\r\n");
        stream
            .write_all(response.as_bytes())
            .expect("write response");
    });
    (format!("http://{addr}/calendar.ics"), handle)
}
