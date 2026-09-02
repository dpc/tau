use std::io::Cursor;

use super::*;

/// Provider diagnostics redact every exact credential echo while preserving
/// ordinary text and the existing final body bound.
#[test]
fn error_body_formatter_redacts_repeated_credentials_before_bounding() {
    let token = "test-access-token";
    let endpoint = "http://127.0.0.1:1234/private";
    let body =
        format!("ordinary {token} middle {endpoint} repeated {token} and {endpoint}\nsecond line");
    let redaction = ErrorBodyRedaction::new(token, Some(endpoint));

    let formatted = redaction.format(body.as_bytes());

    assert_eq!(
        formatted,
        "ordinary <redacted> middle <redacted-endpoint> repeated <redacted> and <redacted-endpoint> second line"
    );
    assert!(!formatted.contains(token), "{formatted}");
    assert!(!formatted.contains(endpoint), "{formatted}");
    assert!(formatted.len() <= MAX_ERROR_BODY_BYTES);
}

/// A token or custom endpoint that begins inside the retained 4 KiB source
/// prefix must be fully recognized even when its tail lies beyond that cut.
#[test]
fn error_body_formatter_redacts_each_credential_across_the_source_cut() {
    let token = "test-access-token";
    let endpoint = "http://127.0.0.1:1234/private";
    let redaction = ErrorBodyRedaction::new(token, Some(endpoint));
    for (secret, expected_marker) in [(token, "<redacted>"), (endpoint, "<redacted-endpoint>")] {
        let prefix_len = MAX_ERROR_BODY_BYTES - secret.len() + 1;
        let body = format!("{}{secret} trailing diagnostic", "x".repeat(prefix_len));
        let formatted = redaction.read_and_format(Cursor::new(body));

        assert!(!formatted.contains(secret), "{formatted}");
        assert!(formatted.contains(expected_marker), "{formatted}");
        assert!(formatted.len() <= MAX_ERROR_BODY_BYTES);
    }
}

/// Overlapping exact credentials form one redacted range, regardless of which
/// value starts first or whether both begin at the same byte.
#[test]
fn error_body_formatter_coalesces_overlapping_credentials() {
    for (token, endpoint, body) in [
        (
            "https",
            "https://private.example",
            "https://private.example",
        ),
        ("abcde", "cdefg", "abcdefg"),
        ("cdefg", "abcde", "abcdefg"),
    ] {
        let redaction = ErrorBodyRedaction::new(token, Some(endpoint));

        let formatted = redaction.format(body.as_bytes());

        assert_eq!(formatted, "<redacted>");
        assert!(!formatted.contains(token), "{formatted}");
        assert!(!formatted.contains(endpoint), "{formatted}");
    }
}

/// Google list reads must explicitly choose deleted-event visibility instead of
/// inheriting a provider default that could change or be bypassed by test data.
#[test]
fn event_list_query_explicitly_selects_deleted_visibility() {
    let active = event_list_query(&GoogleEventListQuery {
        range: TimeRange::default(),
        limit: 20,
        cursor: None,
        include_cancelled: false,
    })
    .expect("active query");
    let discovery = event_list_query(&GoogleEventListQuery {
        range: TimeRange::default(),
        limit: 20,
        cursor: None,
        include_cancelled: true,
    })
    .expect("discovery query");

    let active_pairs = form_urlencoded::parse(active.as_bytes()).collect::<Vec<_>>();
    let discovery_pairs = form_urlencoded::parse(discovery.as_bytes()).collect::<Vec<_>>();
    assert_eq!(
        active_pairs
            .iter()
            .filter(|(key, _)| key == "showDeleted")
            .count(),
        1
    );
    assert!(
        active_pairs
            .iter()
            .any(|(key, value)| key == "showDeleted" && value == "false")
    );
    assert_eq!(
        discovery_pairs
            .iter()
            .filter(|(key, _)| key == "showDeleted")
            .count(),
        1
    );
    assert!(
        discovery_pairs
            .iter()
            .any(|(key, value)| key == "showDeleted" && value == "true")
    );
}

#[test]
fn parses_calendar_list_items() {
    let json = serde_json::json!({
        "id": "primary",
        "summary": "Primary",
        "accessRole": "reader"
    });

    let calendar = parse_calendar(&json).expect("calendar parses");

    assert_eq!(calendar.id.as_str(), "primary");
    assert!(calendar.read_only);
}

#[test]
fn primary_alias_is_tool_facing_when_allowed() {
    // Google calendarList returns the primary calendar's email-like id, but
    // the events API accepts the stable `primary` alias. Keep list output
    // consistent with configs that allow only that alias.
    let account = google_account(vec!["primary"]);
    let json = serde_json::json!({
        "id": "user@example.com",
        "summary": "Personal",
        "primary": true,
        "accessRole": "owner"
    });

    let calendar = allowed_google_calendar(&account, parse_calendar(&json).expect("calendar"))
        .expect("primary alias allowed");

    assert_eq!(calendar.id.as_str(), "primary");
    assert_eq!(calendar.summary, "Personal");
}

#[test]
fn calendar_summary_does_not_grant_google_access() {
    // Display names are mutable and not unique. Access checks intentionally
    // use Google ids plus the explicit `primary` alias only.
    let account = google_account(vec!["Work"]);
    let json = serde_json::json!({
        "id": "work@example.com",
        "summary": "Work",
        "accessRole": "reader"
    });

    let calendar = allowed_google_calendar(&account, parse_calendar(&json).expect("calendar"));

    assert!(calendar.is_none());
}

#[test]
fn google_event_page_cursor_is_backend_prefixed() {
    // Google page tokens are opaque provider data. Keep the model-visible
    // cursor namespaced so it cannot be confused with other backends.
    let json = serde_json::json!({
        "nextPageToken": "abc-123"
    });

    assert_eq!(
        google_next_cursor(&json).expect("next cursor").as_deref(),
        Some("google:abc-123")
    );
    assert_eq!(
        parse_google_cursor(Some("google:abc-123")).expect("cursor"),
        Some("abc-123")
    );
    assert!(parse_google_cursor(Some("ics:1")).is_err());
}

#[test]
fn google_event_page_cursor_rejects_control_characters() {
    let json = serde_json::json!({
        "nextPageToken": "abc\n123"
    });

    assert!(google_next_cursor(&json).is_err());
    assert!(parse_google_cursor(Some("google:abc\n123")).is_err());
}

#[test]
fn parses_event_date_times_dates_and_attendees() {
    let json = serde_json::json!({
        "id": "evt",
        "etag": "abc",
        "iCalUID": "uid@example.test",
        "summary": "Meeting",
        "visibility": "private",
        "transparency": "transparent",
        "start": { "dateTime": "2026-05-28T12:00:00Z" },
        "end": { "date": "2026-05-29" },
        "attendees": [
            { "email": "a@example.com" },
            { "email": "me@example.com", "self": true, "responseStatus": "accepted" }
        ],
        "recurringEventId": "series"
    });

    let event = parse_event(&json).expect("event parses");

    assert_eq!(event.id.as_str(), "evt");
    assert_eq!(event.etag.as_ref().map(EventEtag::as_str), Some("abc"));
    assert_eq!(
        event.i_cal_uid.as_ref().map(ICalUid::as_str),
        Some("uid@example.test")
    );
    assert_eq!(event.end, "2026-05-29");
    assert_eq!(event.attendees, vec!["a@example.com", "me@example.com"]);
    assert_eq!(event.visibility.as_deref(), Some("private"));
    assert_eq!(event.transparency.as_deref(), Some("transparent"));
    assert_eq!(event.self_response_status.as_deref(), Some("accepted"));
    assert!(event.recurring);
}

#[test]
fn event_write_body_supports_all_day_and_timed_events() {
    let attendees = vec!["a@example.com".to_owned(), "b@example.com".to_owned()];
    let body = google_event_body(
        &GoogleEventWrite {
            title: Some("Trip"),
            description: Some("desc"),
            location: Some("There"),
            start: Some("2026-05-28"),
            end: Some("2026-05-29"),
            timezone: None,
            clear_opposite_time_kind: false,
            attendees: Some(&attendees),
        },
        None,
    )
    .expect("body");

    assert_eq!(body["summary"], "Trip");
    assert_eq!(body["start"], json!({ "date": "2026-05-28" }));
    assert_eq!(body["end"], json!({ "date": "2026-05-29" }));
    assert_eq!(body["attendees"][0]["email"], "a@example.com");

    let body = google_event_body(
        &GoogleEventWrite {
            start: Some("2026-05-28T12:00:00Z"),
            end: Some("2026-05-28T13:00:00Z"),
            timezone: Some("UTC"),
            ..Default::default()
        },
        None,
    )
    .expect("timed body");

    assert_eq!(body["start"]["dateTime"], "2026-05-28T12:00:00Z");
    assert_eq!(body["start"]["timeZone"], "UTC");
    assert!(body["start"].get("date").is_none());

    let body = google_event_body(
        &GoogleEventWrite {
            start: Some("2026-05-28T12:00:00Z"),
            end: Some("2026-05-28T13:00:00Z"),
            clear_opposite_time_kind: true,
            ..Default::default()
        },
        None,
    )
    .expect("timed patch body");
    assert_eq!(body["start"]["date"], Value::Null);
    assert_eq!(body["end"]["date"], Value::Null);

    let body = google_event_body(
        &GoogleEventWrite {
            start: Some("2026-05-28"),
            end: Some("2026-05-29"),
            clear_opposite_time_kind: true,
            ..Default::default()
        },
        None,
    )
    .expect("all-day patch body");
    assert_eq!(body["start"]["dateTime"], Value::Null);
    assert_eq!(body["end"]["dateTime"], Value::Null);
}

/// Runtime lowering must consume the validated range without requiring
/// duplicate raw start/end fields and must emit the same provider bytes.
#[test]
fn event_write_body_consumes_validated_time_range() {
    let event_time = EventTimeRange::from_exact(
        "2026-05-28T14:00:00+02:00".to_owned(),
        "2026-05-28T15:00:00+02:00".to_owned(),
    )
    .expect("validated range");

    let body = google_event_body(
        &GoogleEventWrite {
            timezone: Some("Europe/Warsaw"),
            clear_opposite_time_kind: true,
            ..Default::default()
        },
        Some(&event_time),
    )
    .expect("typed body");

    assert_eq!(
        body,
        json!({
            "start": {
                "date": null,
                "dateTime": "2026-05-28T14:00:00+02:00",
                "timeZone": "Europe/Warsaw"
            },
            "end": {
                "date": null,
                "dateTime": "2026-05-28T15:00:00+02:00",
                "timeZone": "Europe/Warsaw"
            }
        })
    );
}

#[test]
fn event_write_body_rejects_invalid_time_pairs() {
    let err = google_event_body(
        &GoogleEventWrite {
            start: Some("2026-05-29"),
            end: Some("2026-05-28"),
            ..Default::default()
        },
        None,
    )
    .expect_err("inverted date is invalid");

    assert!(err.contains("before"), "{err}");
}

#[test]
fn attendee_response_patch_preserves_other_attendees() {
    // Google patch replaces array fields wholesale, so RSVP support must
    // first read the full attendee list and then change only the self row.
    let event = json!({
        "attendees": [
            { "email": "a@example.com", "responseStatus": "needsAction" },
            { "email": "me@example.com", "self": true, "responseStatus": "needsAction" }
        ]
    });

    for (response, expected) in [
        (InviteResponse::Accepted, "accepted"),
        (InviteResponse::Tentative, "tentative"),
        (InviteResponse::Declined, "declined"),
    ] {
        let patch = attendee_response_patch(&event, response).expect("patch");

        assert_eq!(patch["attendees"][0], event["attendees"][0]);
        assert_eq!(patch["attendees"][1]["responseStatus"], expected);
    }
}

/// The public raw RSVP compatibility API must preserve preflight diagnostic
/// precedence when both the calendar and response are invalid.
#[test]
fn public_rsvp_checks_calendar_before_raw_response() {
    let backend = GoogleBackend::new(BTreeMap::new());
    let account = google_account(vec!["primary"]);

    let error =
        match backend.respond_invite(&account, None, "forbidden", "event", "etag", "needsAction") {
            Ok(_) => panic!("disallowed calendar must reject before response"),
            Err(error) => error,
        };

    assert_eq!(
        error,
        "calendar `forbidden` is not allowed for account `google`"
    );
}

fn google_account(allowed_calendars: Vec<&str>) -> ValidatedAccount {
    ValidatedAccount {
        id: CalendarAccountId::test("google"),
        enable: true,
        display_name: None,
        backend: Some(ValidatedBackendConfig::Google {
            client_id_secret: "client".to_owned(),
            client_secret_secret: None,
            refresh_token_secret: Some("refresh".to_owned()),
            api_base: None,
        }),
        default_calendar: None,
        allowed_calendars: allowed_calendars.into_iter().map(str::to_owned).collect(),
        timezone: None,
    }
}
