mod google_write_outcomes;

use std::collections::VecDeque;

use time::format_description as path_time_format_description;

use super::*;
use crate::calendar::config::{
    CalendarAccountConfig, CalendarBackendConfig, CalendarPolicyConfig, CalendarSelectionConfig,
    CalendarWritePolicyConfig, ValidatedReadPolicy, ValidatedWritePolicy,
};

/// Calendar ids in the first list column are opaque tokens for follow-up tool
/// calls. Encode lossy display characters reversibly instead of applying
/// display sanitization that would change spaces, percent signs, or slashes.
#[test]
fn calendar_ids_round_trip_model_visible_opaque_tokens() {
    let calendar_id = flatten_calendar_id("feed", "Team 100%/primary");
    assert_eq!(calendar_id, "feed/Team%20100%25%2Fprimary");

    let (account, calendar) =
        split_flattened_calendar_id(&calendar_id).expect("calendar id parses");

    assert_eq!(account, "feed");
    assert_eq!(calendar, "Team 100%/primary");
}

#[test]
fn omitted_calendar_account_defaults_to_first_enabled_account() {
    // Match email's default-scope behavior so weaker local models that omit
    // a calendar can continue. Calendar read outputs include a flattened
    // calendar id, so the selected default is visible to the model afterwards.
    let temp = tempfile::TempDir::new().expect("tempdir");
    let mut engine = test_engine(temp.path());
    engine.config.accounts.insert(
        "later".to_owned(),
        ValidatedAccount {
            id: "later".to_owned(),
            enable: true,
            display_name: Some("Later".to_owned()),
            backend: Some(ValidatedBackendConfig::IcsFeed {
                url_secret: None,
                url: Some("https://example.test/later.ics".to_owned()),
                allow_plain_http: false,
            }),
            default_calendar: Some("other".to_owned()),
            allowed_calendars: vec!["other".to_owned()],
            timezone: Some("UTC".to_owned()),
        },
    );
    engine.config.account_order.push("later".to_owned());

    let account = engine.single_account(None).expect("default account");
    assert_eq!(account.id, "feed");
    let (account, calendar) = engine.resolve_calendar_arg(None).expect("default calendar");
    assert_eq!(account.id, "feed");
    assert_eq!(calendar, "main");

    let invocation = ToolInvocation {
        command: CalendarCommand::ListEvents,
        args: Some(cbor_map(vec![(
            "start",
            CborValue::Text("2026-05-29".to_owned()),
        )])),
    };
    let result = ok_envelope(
        "list_events",
        "ok",
        cbor_map(vec![
            ("calendar", CborValue::Text("feed/main".to_owned())),
            ("events", CborValue::Array(Vec::new())),
        ]),
    );
    let entry = engine
        .calendar_log_entry(&invocation, &result)
        .expect("log entry");
    assert_eq!(entry.account.as_deref(), Some("feed"));
    assert_eq!(entry.calendar.as_deref(), Some("main"));
}

#[test]
fn calendar_log_records_tool_reads_and_action_lists_them() {
    // Calendar entries contain sensitive schedule metadata. Tool reads need
    // an audit trail that the user can review without exposing event bodies.
    let temp = tempfile::TempDir::new().expect("tempdir");
    let engine = test_engine(temp.path());

    let output = dispatch_test(&engine, command_args("list_calendars", vec![]));
    let data = cbor_field(&output, "data").expect("data");
    assert_eq!(cbor_text_field(data, "format"), Some(LIST_CALENDARS_FORMAT));
    assert_eq!(
        tau_proto::ToolResponse::from_cbor(&output).render(),
        "ok: true\ncommand: list_calendars\nstatus: ok\nformat: calendar_id flags display_name\n\nfeed/main read_only \"Feed\""
    );
    assert_eq!(success_display(&output).args, "list_calendars");

    let log = engine.action_log_last(10).expect("log output");

    assert!(log.contains("Last 1 calendar log entry(s):"), "{log}");
    assert!(log.contains("kind=tool"), "{log}");
    assert!(log.contains("command=list_calendars"), "{log}");
    assert!(log.contains("status=ok"), "{log}");
    assert!(log.contains("items=1"), "{log}");
}

#[test]
fn calendar_success_display_keeps_queued_event_target() {
    // Queued write results are the final model-visible status for default
    // approval policy, so keep the target event and range visible there too.
    let mut change = CalendarChangeApproval::pending("update_event", "google", "primary");
    change.event_id = Some("evt1".to_owned());
    change.start = Some("2026-05-29T10:00:00Z".to_owned());
    change.end = Some("2026-05-29T11:00:00Z".to_owned());

    let result = format_change_queued("change1", &change);

    let display = success_display(&result);
    assert_eq!(display.args, "update_event google/primary event=evt1");
    assert_eq!(
        display.range,
        Some(ToolUseRange {
            start: Some("2026-05-29T10:00".to_owned()),
            end: Some("2026-05-29T11:00".to_owned()),
        })
    );
}

#[test]
fn calendar_direct_write_result_uses_flattened_calendar_id() {
    // When write approval is disabled, mutation results are directly visible to
    // the model and must still hide the separate account concept.
    let mut change = CalendarChangeApproval::pending("delete_event", "google", "primary");
    change.event_id = Some("evt1".to_owned());

    let result =
        format_mutation_result_envelope("change1", &change, &CalendarMutationResult::Deleted);
    let data = cbor_field(&result, "data").expect("data");

    assert_eq!(cbor_text_field(data, "calendar"), Some("google/primary"));
    assert!(cbor_text_field(data, "account").is_none());
}
#[test]
fn calendar_initial_display_shows_scope_and_range() {
    // Calendar reads can be slow/networked; keep the live status chip useful by
    // showing the same scope/range information that matters for the result.
    let display = initial_display(&command_args(
        "list_events",
        vec![
            ("calendar", CborValue::Text("feed/main".to_owned())),
            ("start", CborValue::Text("2026-05-29".to_owned())),
            ("end", CborValue::Text("2026-05-30".to_owned())),
        ],
    ));

    assert_eq!(display.args, "list_events feed/main");
    assert_eq!(
        display.range,
        Some(ToolUseRange {
            start: Some("2026-05-29".to_owned()),
            end: Some("2026-05-30".to_owned()),
        })
    );
}

#[test]
fn calendar_display_preserves_non_midnight_range_times() {
    // Date-only and midnight bounds are compacted to dates, but meaningful
    // non-midnight times must remain visible for hourly reads and writes.
    let display = initial_display(&command_args(
        "list_events",
        vec![
            ("calendar", CborValue::Text("feed/main".to_owned())),
            (
                "start",
                CborValue::Text("2026-05-29T13:30:00-07:00".to_owned()),
            ),
            (
                "end",
                CborValue::Text("2026-05-29T15:00:00-07:00".to_owned()),
            ),
        ],
    ));

    assert_eq!(
        display.range,
        Some(ToolUseRange {
            start: Some("2026-05-29T13:30".to_owned()),
            end: Some("2026-05-29T15:00".to_owned()),
        })
    );
}

#[test]
fn calendar_display_does_not_panic_on_non_ascii_date_suffix() {
    // Initial display runs on raw invocation arguments before validation. A
    // value with an ISO-looking date prefix but non-ASCII suffix must not panic
    // while trying to compact it.
    let display = initial_display(&command_args(
        "list_events",
        vec![
            ("calendar", CborValue::Text("feed/main".to_owned())),
            ("start", CborValue::Text("2026-05-29éééééé".to_owned())),
            ("end", CborValue::Text("2026-05-30Tnot-a-date".to_owned())),
        ],
    ));

    assert_eq!(
        display.range,
        Some(ToolUseRange {
            start: Some("2026-05-29éééééé".to_owned()),
            end: Some("2026-05-30Tnot-a-date".to_owned()),
        })
    );
}

#[test]
fn calendar_error_display_keeps_range_separate_from_args() {
    // Error displays use invocation arguments rather than result data. Keep the
    // same range field there so failed ranged calls do not lose context.
    let arguments = command_args(
        "free_busy",
        vec![
            ("calendar", CborValue::Text("feed/main".to_owned())),
            ("start", CborValue::Text("2026-05-29".to_owned())),
            ("end", CborValue::Text("2026-05-30".to_owned())),
        ],
    );
    let details = cbor_map(vec![("command", CborValue::Text("free_busy".to_owned()))]);

    let display = error_display(&arguments, &details, "boom");

    assert_eq!(display.args, "free_busy feed/main");
    assert_eq!(
        display.range,
        Some(ToolUseRange {
            start: Some("2026-05-29".to_owned()),
            end: Some("2026-05-30".to_owned()),
        })
    );
}

#[test]
fn calendar_success_display_keeps_list_events_compact() {
    // List-event display already has generic item stats, so avoid repeating the
    // same count in labelled chips and keep the date range separate from the
    // calendar scope.
    let output = ok_envelope(
        "list_events",
        "ok",
        cbor_map(vec![
            ("calendar", CborValue::Text("proton/main".to_owned())),
            (
                "start",
                CborValue::Text("2026-06-10T00:00:00-07:00".to_owned()),
            ),
            (
                "end",
                CborValue::Text("2026-06-17T00:00:00-07:00".to_owned()),
            ),
            (
                "events",
                CborValue::Array(vec![
                    CborValue::Text("evt1".to_owned()),
                    CborValue::Text("evt2".to_owned()),
                ]),
            ),
            ("returned_events", CborValue::Integer(2.into())),
            ("scanned_events", CborValue::Integer(2.into())),
        ]),
    );

    let display = success_display(&output);

    assert_eq!(display.args, "list_events proton/main");
    assert_eq!(
        display.range,
        Some(ToolUseRange {
            start: Some("2026-06-10".to_owned()),
            end: Some("2026-06-17".to_owned()),
        })
    );
    assert_eq!(display.stats.matches, Some(2));
    assert_eq!(display.stats.lines, None);
    assert_eq!(display.stats.bytes, None);
    assert!(display.info_chips.is_empty());

    let filtered_output = ok_envelope(
        "list_events",
        "ok",
        cbor_map(vec![
            ("calendar", CborValue::Text("proton/main".to_owned())),
            ("title_filter", CborValue::Text("dpc".to_owned())),
            ("events", CborValue::Array(Vec::new())),
        ]),
    );
    assert_eq!(
        success_display(&filtered_output).args,
        "list_events proton/main title=dpc"
    );

    let filtered_invocation = cbor_map(vec![
        ("command", CborValue::Text("list_events".to_owned())),
        (
            "args",
            cbor_map(vec![
                ("calendar", CborValue::Text("proton/main".to_owned())),
                ("title", CborValue::Text("dpc".to_owned())),
            ]),
        ),
    ]);
    assert_eq!(
        initial_display(&filtered_invocation).args,
        "list_events proton/main title=dpc"
    );

    let empty_output = ok_envelope(
        "list_events",
        "ok",
        cbor_map(vec![("events", CborValue::Array(Vec::new()))]),
    );
    assert_eq!(success_display(&empty_output).stats.matches, Some(0));
}

#[test]
fn split_calendar_tool_displays_do_not_repeat_internal_command() {
    let output = ok_envelope(
        "list_events",
        "ok",
        cbor_map(vec![
            ("calendar", CborValue::Text("proton/main".to_owned())),
            ("title_filter", CborValue::Text("dpc".to_owned())),
            (
                "events",
                CborValue::Array(vec![CborValue::Text("evt1".to_owned())]),
            ),
        ]),
    );
    let event = finish_tool_result(
        invoke_with_command(tool_started(
            "calendar_search",
            vec![
                ("calendar", CborValue::Text("proton/main".to_owned())),
                ("title", CborValue::Text("dpc".to_owned())),
            ],
        )),
        output,
    );

    let Event::ToolResult(result) = event else {
        panic!("successful split calendar command should be a tool result")
    };
    let display = result.display.expect("display");
    assert_eq!(display.args, "proton/main title=dpc");
    assert_eq!(display.stats.matches, Some(1));

    let initial = initial_display_for_tool(
        "calendar_get",
        &cbor_map(vec![
            ("calendar", CborValue::Text("proton/main".to_owned())),
            ("event_id", CborValue::Text("evt1".to_owned())),
        ]),
    );
    assert_eq!(initial.args, "proton/main event_id=evt1");
}

/// Ensures operation-specific payload detail does not replace the canonical
/// successful tool-result display status.
#[test]
fn successful_calendar_display_status_is_canonical() {
    let output = ok_envelope(
        "create_event",
        "created",
        cbor_map(vec![("event_id", CborValue::Text("evt1".to_owned()))]),
    );
    let event = finish_tool_result(
        invoke_with_command(tool_started("calendar_create", Vec::new())),
        output,
    );

    let Event::ToolResult(result) = event else {
        panic!("successful split calendar command should be a tool result")
    };
    assert_eq!(cbor_text_field(&result.result, "status"), Some("created"));
    let display = result.display.expect("display");
    assert_eq!(display.status, ToolUseStatus::Success);
    assert_eq!(display.status_text, "ok");

    let pending = ok_envelope("create_event", "approval_required", cbor_map(Vec::new()));
    assert_eq!(
        success_display(&pending).status_text,
        "approval_required",
        "pending side effects retain their documented lifecycle status"
    );
}

#[test]
fn split_calendar_tool_error_display_uses_external_tool_name() {
    let event = finish_tool_result(
        invoke_with_command(tool_started(
            "calendar_get",
            vec![
                ("calendar", CborValue::Text("proton/main".to_owned())),
                ("event_id", CborValue::Text("evt1".to_owned())),
            ],
        )),
        error_envelope(Some("read_event"), "network_error", "backend failed"),
    );

    let Event::ToolError(error) = event else {
        panic!("failed split calendar command should be a tool error")
    };
    let display = error.display.expect("display");
    let expected = "calendar_get failed (network_error): backend failed";
    assert_eq!(error.message, expected);
    assert_eq!(display.status_text, expected);
    assert_eq!(display.args, "proton/main event_id=evt1");
}
#[test]
fn list_events_uses_start_end_range_names_and_rejects_old_names() {
    // Range reads now use the same `start`/`end` names as event payloads,
    // parsed through a command-specific struct. The old time_min/time_max
    // names must fail instead of being accepted as a second vocabulary.
    let invocation = ToolInvocation {
        command: CalendarCommand::ListEvents,
        args: Some(cbor_map(vec![
            ("calendar", CborValue::Text("feed/main".to_owned())),
            (
                "start",
                CborValue::Text("2026-05-29T00:00:00-07:00".to_owned()),
            ),
            (
                "end",
                CborValue::Text("2026-05-30T00:00:00-07:00".to_owned()),
            ),
        ])),
    };
    let args = parse_invocation_args::<CalendarRangeArgs>(&invocation).expect("range args");
    assert_eq!(args.start.as_deref(), Some("2026-05-29T00:00:00-07:00"));
    assert_eq!(args.end.as_deref(), Some("2026-05-30T00:00:00-07:00"));

    let temp = tempfile::TempDir::new().expect("tempdir");
    let engine = test_engine(temp.path());
    let output = dispatch_test(
        &engine,
        command_args(
            "list_events",
            vec![
                ("calendar", CborValue::Text("feed/main".to_owned())),
                (
                    "time_min",
                    CborValue::Text("2026-05-29T00:00:00Z".to_owned()),
                ),
            ],
        ),
    );

    assert_eq!(cbor_bool_field(&output, "ok"), Some(false));
    assert_eq!(cbor_text_field(&output, "command"), Some("list_events"));
    let message = cbor_nested_text_field(&output, "error", "message").expect("message");
    assert_eq!(message, "list_events does not accept `time_min`");
}

#[test]
fn free_busy_rejects_title_filter_instead_of_ignoring_it() {
    // `free_busy` should not leak title probing; use `list_events` for title
    // filters.
    let temp = tempfile::TempDir::new().expect("tempdir");
    let engine = test_engine(temp.path());

    let output = dispatch_test(
        &engine,
        command_args(
            "free_busy",
            vec![
                ("calendar", CborValue::Text("feed/main".to_owned())),
                ("start", CborValue::Text("2026-05-29".to_owned())),
                ("title", CborValue::Text("tau".to_owned())),
            ],
        ),
    );

    assert_eq!(cbor_bool_field(&output, "ok"), Some(false));
    assert_eq!(cbor_text_field(&output, "command"), Some("free_busy"));
    let message = cbor_nested_text_field(&output, "error", "message").expect("message");
    assert_eq!(
        message,
        "calendar_free_busy does not accept `title`; use calendar_search for title filtering"
    );
}

#[test]
fn calendar_range_args_accept_local_bounds_and_default_end() {
    // Agents often know the date but omit an offset. Range reads should
    // interpret local date/date-time values in the account timezone and
    // stay bounded even when `end` is omitted.
    let temp = tempfile::TempDir::new().expect("tempdir");
    let engine = test_engine(temp.path());
    let account = engine.config.accounts.get("feed").expect("account");

    let range = parse_range(
        &CalendarRangeArgs {
            start: Some("2026-05-30T12:34:56".to_owned()),
            ..Default::default()
        },
        account,
    )
    .expect("local datetime range");
    assert_eq!(
        range
            .min
            .expect("min")
            .format(&time::format_description::well_known::Rfc3339)
            .expect("format min"),
        "2026-05-30T12:34:56Z"
    );
    assert_eq!(
        range
            .max
            .expect("max")
            .format(&time::format_description::well_known::Rfc3339)
            .expect("format max"),
        "2026-06-06T12:34:56Z"
    );

    let range = parse_range(
        &CalendarRangeArgs {
            start: Some("2026-05-30".to_owned()),
            end: Some("2026-05-31".to_owned()),
            ..Default::default()
        },
        account,
    )
    .expect("local date range");
    assert_eq!(
        range
            .min
            .expect("min")
            .format(&time::format_description::well_known::Rfc3339)
            .expect("format min"),
        "2026-05-30T00:00:00Z"
    );
    assert_eq!(
        range
            .max
            .expect("max")
            .format(&time::format_description::well_known::Rfc3339)
            .expect("format max"),
        "2026-05-31T00:00:00Z"
    );

    let la_start = parse_read_bound("2026-05-30T00:00:00", "start", Some("America/Los_Angeles"))
        .expect("la local start");
    assert_eq!(
        la_start
            .format(&time::format_description::well_known::Rfc3339)
            .expect("format la start"),
        "2026-05-30T00:00:00-07:00"
    );

    let la_fall_start =
        parse_read_bound("2026-10-31T00:00:00", "start", Some("America/Los_Angeles"))
            .expect("la fall start");
    let la_fall_end = default_read_end_bound(
        "2026-10-31T00:00:00",
        la_fall_start,
        Some("America/Los_Angeles"),
    )
    .expect("la fall default end");
    assert_eq!(
        la_fall_end
            .format(&time::format_description::well_known::Rfc3339)
            .expect("format la fall end"),
        "2026-11-07T00:00:00-08:00"
    );
}

#[test]
fn list_events_ignores_blank_title_filter() {
    // Agents may include an empty `title` when they mean "no filter".
    // Treat whitespace-only filters as absent instead of failing the read.
    assert_eq!(
        optional_trimmed_line(Some(" \n\t "), "title").expect("blank title"),
        None
    );
    assert_eq!(
        optional_trimmed_line(Some("  project sync\n"), "title").expect("trimmed title"),
        Some("project sync".to_owned())
    );
}

#[test]
fn calendar_range_args_default_to_recent_week() {
    // Regression coverage for weak models that omit `start` after a calendar
    // error. Missing bounds must remain safe and bounded instead of failing
    // into an auto-retry loop or creating an unbounded read.
    let temp = tempfile::TempDir::new().expect("tempdir");
    let engine = test_engine(temp.path());
    let account = engine.config.accounts.get("feed").expect("account");
    let today_before = time::OffsetDateTime::now_utc().date();

    let range = parse_range(&CalendarRangeArgs::default(), account).expect("default range");

    let today_after = time::OffsetDateTime::now_utc().date();
    let min = range.min.expect("min");
    let max = range.max.expect("max");
    let expected_before =
        date_days_before(today_before, DEFAULT_READ_LOOKBACK_DAYS).expect("expected before date");
    let expected_after =
        date_days_before(today_after, DEFAULT_READ_LOOKBACK_DAYS).expect("expected after date");
    assert!(
        min.date() == expected_before || min.date() == expected_after,
        "min {min:?} should default to midnight two days before today"
    );
    assert_eq!(min.time(), time::Time::MIDNIGHT);
    assert_eq!(max - min, time::Duration::days(DEFAULT_READ_WINDOW_DAYS));
}

#[test]
fn calendar_log_prefers_effective_default_range_over_blank_args() {
    // Blank read bounds are treated like omission. The audit log should record
    // the effective default range returned by the tool, not the model's blank
    // raw arguments.
    let temp = tempfile::TempDir::new().expect("tempdir");
    let engine = test_engine(temp.path());
    let invocation = ToolInvocation {
        command: CalendarCommand::ListEvents,
        args: Some(cbor_map(vec![
            ("start", CborValue::Text(" \t".to_owned())),
            ("end", CborValue::Text("".to_owned())),
        ])),
    };
    let result = ok_envelope(
        "list_events",
        "ok",
        cbor_map(vec![
            ("calendar", CborValue::Text("feed/main".to_owned())),
            ("start", CborValue::Text("2026-05-30T00:00:00Z".to_owned())),
            ("end", CborValue::Text("2026-06-06T00:00:00Z".to_owned())),
            ("events", CborValue::Array(Vec::new())),
        ]),
    );

    let entry = engine
        .calendar_log_entry(&invocation, &result)
        .expect("log entry");

    assert_eq!(entry.start.as_deref(), Some("2026-05-30T00:00:00Z"));
    assert_eq!(entry.end.as_deref(), Some("2026-06-06T00:00:00Z"));
}

#[test]
fn calendar_log_records_failed_write_attempts_without_payloads() {
    // Write commands are still unsupported, but attempts should be visible
    // in the audit log before mutation approval plumbing is added.
    let temp = tempfile::TempDir::new().expect("tempdir");
    let engine = test_engine(temp.path());

    let err = dispatch_test(
        &engine,
        command_args(
            "create_event",
            vec![
                ("calendar", CborValue::Text("feed/main".to_owned())),
                ("title", CborValue::Text("private title".to_owned())),
            ],
        ),
    );
    assert_eq!(cbor_bool_field(&err, "ok"), Some(false));
    let err_text = calendar_error_message(&err);
    assert!(
        err_text.contains("does not support calendar writes"),
        "{err_text}"
    );

    let log = engine.action_log_last(10).expect("log output");

    assert!(log.contains("command=create_event"), "{log}");
    assert!(log.contains("status=invalid_input"), "{log}");
    assert!(log.contains("account=feed"), "{log}");
    assert!(log.contains("calendar=main"), "{log}");
    assert!(!log.contains("private title"), "{log}");
}

#[test]
fn calendar_approve_all_accepts_empty_pending_list() {
    // `:calendar change approve all` should be a valid convenience command
    // even when there is nothing queued.
    let temp = tempfile::TempDir::new().expect("tempdir");
    let engine = test_engine(temp.path());

    let output = engine
        .action_change_approve_args(&["all".to_owned()])
        .expect("approve all");

    assert_eq!(output, "No pending calendar changes to approve.");
}

#[test]
fn google_writes_queue_pending_calendar_changes() {
    // Calendar writes can send attendee notifications or alter the user's
    // schedule, so the default policy persists a pending change for review
    // instead of calling Google immediately.
    let temp = tempfile::TempDir::new().expect("tempdir");
    let cfg = CalendarExtensionConfig {
        enable: true,
        accounts: vec![CalendarAccountConfig {
            id: "google".to_owned(),
            enable: true,
            backend: Some(CalendarBackendConfig::Google {
                client_id_secret: "client".to_owned(),
                client_secret_secret: None,
                refresh_token_secret: Some("refresh".to_owned()),
                api_base: None,
            }),
            calendars: CalendarSelectionConfig {
                default: Some("primary".to_owned()),
                allow: vec!["primary".to_owned()],
            },
            ..Default::default()
        }],
        ..Default::default()
    };
    let engine = Engine {
        config: cfg.validate().expect("valid config"),
        state: StateStore::open(temp.path().join("state")).expect("state"),
        google: GoogleBackend::new(BTreeMap::new()),
        ics_feed: IcsFeedBackend::new(BTreeMap::new()),
        etags: RefCell::new(BTreeMap::new()),
        last_events: RefCell::new(BTreeMap::new()),
    };

    let output = dispatch_test(
        &engine,
        command_args(
            "create_event",
            vec![
                ("calendar", CborValue::Text("google/primary".to_owned())),
                ("title", CborValue::Text("Team Sync".to_owned())),
                ("start", CborValue::Text("2026-05-28T12:00:00Z".to_owned())),
                ("end", CborValue::Text("2026-05-28T13:00:00Z".to_owned())),
                (
                    "attendees",
                    CborValue::Array(vec![CborValue::Text("a@example.com".to_owned())]),
                ),
            ],
        ),
    );
    let data = cbor_field(&output, "data").expect("data");

    assert_eq!(
        cbor_text_field(&output, "status"),
        Some("approval_required")
    );
    assert_eq!(cbor_text_field(data, "approval_id"), Some("1"));
    let list = engine.action_change_list().expect("change list");
    assert!(list.contains("command=create_event"), "{list}");
    assert!(list.contains("title=Team Sync"), "{list}");
    let open = engine.action_change_open("1").expect("change open");
    assert!(open.contains("attendees: a@example.com"), "{open}");
    assert_eq!(
        engine.action_change_deny("1"),
        Ok("Denied calendar change 1.".to_owned())
    );
}

#[test]
fn create_event_defaults_missing_end() {
    // Small local models often omit `end` even when they identified a
    // concrete start. Queueing a safe default prevents an avoidable retry
    // loop while keeping the pending change visible for user approval.
    let (start, end) = create_event_time_pair(Some("2026-05-28T12:00:00Z"), None, Some("UTC"))
        .expect("default date-time end");
    assert_eq!(start, "2026-05-28T12:00:00Z");
    assert_eq!(end, "2026-05-28T13:00:00Z");

    let (start, end) =
        create_event_time_pair(Some("2026-05-28"), None, Some("UTC")).expect("default all-day end");
    assert_eq!(start, "2026-05-28");
    assert_eq!(end, "2026-05-29");

    let (start, end) = create_event_time_pair(
        Some("2026-05-28T12:00:00"),
        Some("2026-05-28T13:00:00"),
        Some("UTC"),
    )
    .expect("local date-times use account timezone");
    assert_eq!(start, "2026-05-28T12:00:00Z");
    assert_eq!(end, "2026-05-28T13:00:00Z");
}

#[test]
fn google_create_event_queues_pending_change_with_default_end() {
    // Calendar writes are still queued for approval; this only fills in a
    // low-risk default duration when the model omits `end`.
    let temp = tempfile::TempDir::new().expect("tempdir");
    let cfg = CalendarExtensionConfig {
        enable: true,
        accounts: vec![CalendarAccountConfig {
            id: "google".to_owned(),
            enable: true,
            backend: Some(CalendarBackendConfig::Google {
                client_id_secret: "client".to_owned(),
                client_secret_secret: None,
                refresh_token_secret: Some("refresh".to_owned()),
                api_base: None,
            }),
            calendars: CalendarSelectionConfig {
                default: Some("primary".to_owned()),
                allow: vec!["primary".to_owned()],
            },
            ..Default::default()
        }],
        ..Default::default()
    };
    let engine = Engine {
        config: cfg.validate().expect("valid config"),
        state: StateStore::open(temp.path().join("state")).expect("state"),
        google: GoogleBackend::new(BTreeMap::new()),
        ics_feed: IcsFeedBackend::new(BTreeMap::new()),
        etags: RefCell::new(BTreeMap::new()),
        last_events: RefCell::new(BTreeMap::new()),
    };

    let output = dispatch_test(
        &engine,
        command_args(
            "create_event",
            vec![
                ("title", CborValue::Text("Team Sync".to_owned())),
                ("start", CborValue::Text("2026-05-28T12:00:00Z".to_owned())),
            ],
        ),
    );
    let data = cbor_field(&output, "data").expect("data");

    assert_eq!(
        cbor_text_field(&output, "status"),
        Some("approval_required")
    );
    assert_eq!(cbor_text_field(data, "approval_id"), Some("1"));
    let open = engine.action_change_open("1").expect("change open");
    assert!(open.contains("start: 2026-05-28T12:00:00Z"), "{open}");
    assert!(open.contains("end: 2026-05-28T13:00:00Z"), "{open}");
}

/// Updating an event with only `start` should preserve the create-event default
/// duration behavior while still requiring the cached Google ETag precondition.
#[test]
fn google_update_event_builds_default_end_with_cached_etag() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let engine = google_test_engine(temp.path());
    engine.etags.borrow_mut().insert(
        EventKey::new(
            "google",
            ProviderCalendarId::new("primary"),
            EventId::new("evt1"),
        ),
        EventEtag::new("etag-1"),
    );

    let change = engine
        .build_change(
            CalendarCommand::UpdateEvent,
            ChangeArgs {
                calendar: Some("google/primary".to_owned()),
                event_id: Some("evt1".to_owned()),
                start: Some("2026-05-28T12:00:00Z".to_owned()),
                ..empty_change_args()
            },
        )
        .expect("update change");

    assert_eq!(change.etag.as_deref(), Some("etag-1"));
    assert_eq!(change.start.as_deref(), Some("2026-05-28T12:00:00Z"));
    assert_eq!(change.end.as_deref(), Some("2026-05-28T13:00:00Z"));
}

/// Update requests must not accept invite responses. This keeps the command
/// split explicit after moving build-change validation into command helpers.
#[test]
fn update_event_rejects_invite_response_argument() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let engine = google_test_engine(temp.path());
    engine.etags.borrow_mut().insert(
        EventKey::new(
            "google",
            ProviderCalendarId::new("primary"),
            EventId::new("evt1"),
        ),
        EventEtag::new("etag-1"),
    );

    let err = engine
        .build_change(
            CalendarCommand::UpdateEvent,
            ChangeArgs {
                calendar: Some("google/primary".to_owned()),
                event_id: Some("evt1".to_owned()),
                title: Some("Team Sync".to_owned()),
                response: Some("accepted".to_owned()),
                ..empty_change_args()
            },
        )
        .expect_err("response must be invite-only");

    assert_eq!(err, "response is only valid for respond_invite");
}

#[test]
fn google_reads_without_stored_auth_report_auth_error() {
    // Accounts that opt into action-owned OAuth should fail before any
    // network call until `:calendar auth google` stores a refresh token.
    let temp = tempfile::TempDir::new().expect("tempdir");
    let cfg = CalendarExtensionConfig {
        enable: true,
        accounts: vec![CalendarAccountConfig {
            id: "google".to_owned(),
            enable: true,
            backend: Some(CalendarBackendConfig::Google {
                client_id_secret: "client".to_owned(),
                client_secret_secret: None,
                refresh_token_secret: None,
                api_base: None,
            }),
            calendars: CalendarSelectionConfig {
                default: Some("primary".to_owned()),
                allow: vec!["primary".to_owned()],
            },
            ..Default::default()
        }],
        ..Default::default()
    };
    let engine = Engine {
        config: cfg.validate().expect("valid config"),
        state: StateStore::open(temp.path().join("state")).expect("state"),
        google: GoogleBackend::new(BTreeMap::new()),
        ics_feed: IcsFeedBackend::new(BTreeMap::new()),
        etags: RefCell::new(BTreeMap::new()),
        last_events: RefCell::new(BTreeMap::new()),
    };

    let output = dispatch_test(&engine, command_args("list_calendars", vec![]));

    assert_eq!(cbor_bool_field(&output, "ok"), Some(false));
    assert_eq!(
        cbor_nested_text_field(&output, "error", "code"),
        Some("auth_error")
    );
    assert!(
        calendar_error_message(&output).contains(":calendar auth google start google"),
        "{}",
        calendar_error_message(&output)
    );
}

/// Ensures the live Google Calendar authorization response teaches the colon
/// finish command so its device code cannot be pasted as ordinary prompt text.
#[test]
fn google_auth_start_response_uses_colon_finish_command() {
    let response = format_google_auth_started(
        "google",
        &crate::google_oauth::GoogleDeviceAuthStart {
            device_code: "device-secret".to_owned(),
            user_code: "USER-CODE".to_owned(),
            verification_uri: "https://example.test/device".to_owned(),
            expires_in_secs: 900,
            interval_secs: 5,
        },
    );

    assert!(response.contains("\n:calendar auth google finish google\n"));
    assert!(!response.contains("\n/calendar auth google finish"));
    assert!(!response.contains("device-secret"));
}

#[test]
fn private_event_details_are_busy_only_by_default() {
    // Provider-private events should not leak summaries or descriptions to
    // the model unless policy explicitly opts into details.
    let account = ValidatedAccount {
        id: "google".to_owned(),
        enable: true,
        display_name: None,
        backend: Some(ValidatedBackendConfig::Google {
            client_id_secret: "client".to_owned(),
            client_secret_secret: None,
            refresh_token_secret: Some("refresh".to_owned()),
            api_base: None,
        }),
        default_calendar: Some("primary".to_owned()),
        allowed_calendars: vec!["primary".to_owned()],
        timezone: Some("UTC".to_owned()),
    };
    let event = BackendEvent::Google(GoogleEventRecord {
        id: EventId::new("evt".to_owned()),
        etag: Some(EventEtag::new("abc")),
        i_cal_uid: None,
        summary: "Private title".to_owned(),
        description: Some("private body".to_owned()),
        location: Some("Secret room".to_owned()),
        start: "2026-05-28T12:00:00Z".to_owned(),
        end: "2026-05-28T13:00:00Z".to_owned(),
        status: Some("confirmed".to_owned()),
        visibility: Some("private".to_owned()),
        transparency: None,
        organizer: Some("org@example.com".to_owned()),
        attendees: vec!["a@example.com".to_owned()],
        self_response_status: None,
        recurring: false,
    });
    let policy = ValidatedPolicy {
        read: ValidatedReadPolicy {
            private_events: PrivateEventsPolicy::BusyOnly,
            descriptions: DescriptionPolicy::ApprovedOnly,
        },
        write: ValidatedWritePolicy {
            require_approval: true,
            max_attendees: 50,
        },
    };

    let detail = format_event_detail(
        &policy,
        &account,
        "primary",
        timezone_for_read("event output", account.timezone.as_deref()).expect("timezone"),
        &event,
    )
    .join("\n");

    assert!(detail.contains("summary (private)"), "{detail}");
    assert!(
        detail.contains("flags read_only,private_busy_only"),
        "{detail}"
    );
    assert!(!detail.contains("Private title"), "{detail}");
    assert!(!detail.contains("private body"), "{detail}");
    assert!(!detail.contains("Secret room"), "{detail}");
}

#[test]
fn google_event_details_hide_etag_from_agent() {
    // Google read responses keep ETags internally for conditional writes;
    // model-visible event details should stay focused on user data.
    let account = ValidatedAccount {
        id: "google".to_owned(),
        enable: true,
        display_name: None,
        backend: Some(ValidatedBackendConfig::Google {
            client_id_secret: "client".to_owned(),
            client_secret_secret: None,
            refresh_token_secret: Some("refresh".to_owned()),
            api_base: None,
        }),
        default_calendar: Some("primary".to_owned()),
        allowed_calendars: vec!["primary".to_owned()],
        timezone: Some("UTC".to_owned()),
    };
    let event = BackendEvent::Google(GoogleEventRecord {
        id: EventId::new("evt".to_owned()),
        etag: Some(EventEtag::new("abc")),
        i_cal_uid: Some(ICalUid::new("uid@example.com")),
        summary: "Team Sync".to_owned(),
        description: Some("line 1\nline 2".to_owned()),
        location: Some("Room 1".to_owned()),
        start: "2026-05-28T12:00:00Z".to_owned(),
        end: "2026-05-28T13:00:00Z".to_owned(),
        status: Some("confirmed".to_owned()),
        visibility: None,
        transparency: None,
        organizer: Some("org@example.com".to_owned()),
        attendees: vec!["a@example.com".to_owned(), "b@example.com".to_owned()],
        self_response_status: None,
        recurring: true,
    });
    let policy = ValidatedPolicy {
        read: ValidatedReadPolicy {
            private_events: PrivateEventsPolicy::BusyOnly,
            descriptions: DescriptionPolicy::Always,
        },
        write: ValidatedWritePolicy {
            require_approval: true,
            max_attendees: 50,
        },
    };

    assert_eq!(
        format_event_detail(
            &policy,
            &account,
            "primary",
            timezone_for_read("event output", account.timezone.as_deref()).expect("timezone"),
            &event,
        )
        .join("\n"),
        "calendar google/primary\nevent_id evt\nstart 2026-05-28T12:00:00Z\nend 2026-05-28T13:00:00Z\nflags read_only,recurring\nsummary Team_Sync\nuid uid@example.com\nstatus confirmed\nlocation Room_1\norganizer org@example.com\nattendees a@example.com,b@example.com\ndescription line 1 line 2"
    );
}

#[test]
fn calendar_event_output_uses_account_timezone() {
    // Calendar reads already interpret date-only query bounds in the account
    // timezone. Event rows should use the same timezone instead of handing UTC
    // rows to the model and expecting it to convert them correctly.
    let account = ValidatedAccount {
        id: "feed".to_owned(),
        enable: true,
        display_name: None,
        backend: Some(ValidatedBackendConfig::IcsFeed {
            url_secret: None,
            url: Some("https://example.test/calendar.ics".to_owned()),
            allow_plain_http: false,
        }),
        default_calendar: Some("main".to_owned()),
        allowed_calendars: vec!["main".to_owned()],
        timezone: Some("America/Los_Angeles".to_owned()),
    };
    let event = BackendEvent::Ics(IcsEventRecord {
        id: EventId::new("evt".to_owned()),
        uid: ICalUid::new("uid".to_owned()),
        summary: "Local meeting".to_owned(),
        description: None,
        location: None,
        start: "2026-06-04T16:00:00Z".to_owned(),
        end: "2026-06-04T17:00:00Z".to_owned(),
        start_utc: Some(
            time::OffsetDateTime::parse(
                "2026-06-04T16:00:00Z",
                &path_time_format_description::well_known::Rfc3339,
            )
            .expect("time"),
        ),
        end_utc: Some(
            time::OffsetDateTime::parse(
                "2026-06-04T17:00:00Z",
                &path_time_format_description::well_known::Rfc3339,
            )
            .expect("time"),
        ),
        status: Some("confirmed".to_owned()),
        organizer: None,
        attendees: Vec::new(),
        private: false,
        recurring: false,
        time_unparsed: false,
    });
    let policy = ValidatedPolicy {
        read: ValidatedReadPolicy {
            private_events: PrivateEventsPolicy::BusyOnly,
            descriptions: DescriptionPolicy::ApprovedOnly,
        },
        write: ValidatedWritePolicy {
            require_approval: true,
            max_attendees: 50,
        },
    };
    let timezone =
        timezone_for_read("event output", account.timezone.as_deref()).expect("timezone");

    let line = format_event_line(&policy, timezone, &event);
    let detail = format_event_detail(&policy, &account, "main", timezone, &event).join("\n");

    assert!(line.contains("2026-06-04T09:00:00-07:00"), "{line}");
    assert!(line.contains("2026-06-04T10:00:00-07:00"), "{line}");
    assert!(
        detail.contains("start 2026-06-04T09:00:00-07:00"),
        "{detail}"
    );
}

#[test]
fn read_event_validates_output_timezone_before_backend_access() {
    // If configured output timezone is broken, fail before touching provider
    // state or network. This keeps configuration errors deterministic and
    // avoids partial side effects such as ETag cache updates.
    let temp = tempfile::TempDir::new().expect("tempdir");
    let mut engine = test_engine(temp.path());
    engine.config.accounts.insert(
        "bad-tz".to_owned(),
        ValidatedAccount {
            id: "bad-tz".to_owned(),
            enable: true,
            display_name: None,
            backend: Some(ValidatedBackendConfig::IcsFeed {
                url_secret: None,
                url: Some("not a url".to_owned()),
                allow_plain_http: false,
            }),
            default_calendar: Some("main".to_owned()),
            allowed_calendars: vec!["main".to_owned()],
            timezone: Some("Not/AZone".to_owned()),
        },
    );
    engine.config.account_order.push("bad-tz".to_owned());

    let err = engine
        .read_event(
            &ReadEventArgs {
                calendar: Some("bad-tz/main".to_owned()),
                event_id: Some("evt".to_owned()),
            },
            &AgentId::parse("agent").expect("agent id"),
        )
        .expect_err("invalid timezone should fail before backend access");

    assert!(err.contains("account timezone `Not/AZone`"), "{err}");
    assert!(!err.contains("iCalendar feed URL"), "{err}");
}

#[test]
fn calendar_change_detail_hides_internal_etag() {
    // Approval details may be echoed into agent-visible transcripts. Keep
    // provider precondition tokens internal even though pending changes
    // persist them for later approval execution.
    let mut change = CalendarChangeApproval::pending("update_event", "google", "primary");
    change.id = "1".to_owned();
    change.event_id = Some("evt".to_owned());
    change.etag = Some("abc".to_owned());
    change.title = Some("Team Sync".to_owned());

    let detail = format_change_detail(&change);

    assert!(detail.contains("event_id: evt"), "{detail}");
    assert!(!detail.contains("etag"), "{detail}");
    assert!(!detail.contains("abc"), "{detail}");
}

#[test]
fn google_event_etag_cache_is_cleared_by_missing_provider_etag() {
    // A malformed or degraded provider response without an ETag must fail
    // closed instead of leaving an older precondition token active.
    let temp = tempfile::TempDir::new().expect("tempdir");
    let engine = test_engine(temp.path());
    let account = ValidatedAccount {
        id: "google".to_owned(),
        enable: true,
        display_name: None,
        backend: Some(ValidatedBackendConfig::Google {
            client_id_secret: "client".to_owned(),
            client_secret_secret: None,
            refresh_token_secret: Some("refresh".to_owned()),
            api_base: None,
        }),
        default_calendar: Some("primary".to_owned()),
        allowed_calendars: vec!["primary".to_owned()],
        timezone: Some("UTC".to_owned()),
    };
    let mut event = BackendEvent::Google(GoogleEventRecord {
        id: EventId::new("evt".to_owned()),
        etag: Some(EventEtag::new("abc")),
        i_cal_uid: None,
        summary: "Team Sync".to_owned(),
        description: None,
        location: None,
        start: "2026-05-28T12:00:00Z".to_owned(),
        end: "2026-05-28T13:00:00Z".to_owned(),
        status: Some("confirmed".to_owned()),
        visibility: None,
        transparency: None,
        organizer: None,
        attendees: Vec::new(),
        self_response_status: None,
        recurring: false,
    });
    let mut change = CalendarChangeApproval::pending("update_event", "google", "primary");
    change.event_id = Some("evt".to_owned());

    engine.remember_event_etag(&account, "primary", &event);
    assert_eq!(
        engine.cached_etag_for_change(&change).expect("cached etag"),
        "abc"
    );

    if let BackendEvent::Google(event) = &mut event {
        event.etag = None;
    }
    engine.remember_event_etag(&account, "primary", &event);

    assert!(engine.cached_etag_for_change(&change).is_err());
}

/// ETag cache lookup must retain account, calendar, and event namespaces so an
/// identical provider event string cannot authorize a mutation elsewhere.
#[test]
fn google_event_etag_cache_rejects_cross_namespace_reuse() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let engine = test_engine(temp.path());
    engine.etags.borrow_mut().insert(
        EventKey::new(
            "work",
            ProviderCalendarId::new("primary"),
            EventId::new("shared"),
        ),
        EventEtag::new("etag-work-primary"),
    );

    let change = |account: &str, calendar: &str| {
        let mut change = CalendarChangeApproval::pending("update_event", account, calendar);
        change.event_id = Some("shared".to_owned());
        change
    };

    assert_eq!(
        engine
            .cached_etag_for_change(&change("work", "primary"))
            .expect("exact namespace matches"),
        "etag-work-primary"
    );
    assert!(
        engine
            .cached_etag_for_change(&change("personal", "primary"))
            .is_err()
    );
    assert!(
        engine
            .cached_etag_for_change(&change("work", "secondary"))
            .is_err()
    );
    assert!(
        engine
            .cached_etag_for_change(&{
                let mut other_event = change("work", "primary");
                other_event.event_id = Some("other".to_owned());
                other_event
            })
            .is_err()
    );
}

#[test]
fn title_filter_matches_visible_event_summaries() {
    let events = [
        BackendEvent::Google(GoogleEventRecord {
            id: EventId::new("evt1".to_owned()),
            etag: None,
            i_cal_uid: None,
            summary: "Tau Testing Party".to_owned(),
            description: None,
            location: None,
            start: "2026-05-28".to_owned(),
            end: "2026-05-29".to_owned(),
            status: Some("confirmed".to_owned()),
            visibility: None,
            transparency: None,
            organizer: None,
            attendees: Vec::new(),
            self_response_status: None,
            recurring: false,
        }),
        BackendEvent::Google(GoogleEventRecord {
            id: EventId::new("evt2".to_owned()),
            etag: None,
            i_cal_uid: None,
            summary: "Lunch".to_owned(),
            description: None,
            location: None,
            start: "2026-05-28".to_owned(),
            end: "2026-05-29".to_owned(),
            status: Some("confirmed".to_owned()),
            visibility: None,
            transparency: None,
            organizer: None,
            attendees: Vec::new(),
            self_response_status: None,
            recurring: false,
        }),
    ];
    let policy = ValidatedPolicy {
        read: ValidatedReadPolicy {
            private_events: PrivateEventsPolicy::BusyOnly,
            descriptions: DescriptionPolicy::Always,
        },
        write: ValidatedWritePolicy {
            require_approval: true,
            max_attendees: 50,
        },
    };

    let filtered = events
        .iter()
        .filter(|event| event_is_visible(&policy, event, Some("tau"), EventVisibility::Active))
        .collect::<Vec<_>>();

    assert_eq!(filtered.len(), 1);
    assert_eq!(event_id(filtered[0]).as_str(), "evt1");
}

/// Ordinary searches must defensively hide cancelled provider rows, whereas a
/// deliberate discovery search may retain them for cancellation investigation.
#[test]
fn search_visibility_filters_cancelled_rows_case_insensitively() {
    let events = [
        test_google_event("active", Some("confirmed"), None, None),
        test_google_event("cancelled", Some("cAnCeLlEd"), None, None),
    ];
    let policy = test_policy();

    let active = events
        .iter()
        .filter(|event| event_is_visible(&policy, event, None, EventVisibility::Active))
        .collect::<Vec<_>>();
    let discovery = events
        .iter()
        .filter(|event| event_is_visible(&policy, event, None, EventVisibility::ActiveAndCancelled))
        .collect::<Vec<_>>();

    assert_eq!(active.len(), 1);
    assert_eq!(event_id(active[0]).as_str(), "active");
    assert_eq!(discovery.len(), 2);
}

/// Free/busy must leave only active, blocking entries so stale cancellations,
/// transparent holds, and declined invitations cannot consume availability.
#[test]
fn free_busy_excludes_cancelled_transparent_and_self_declined_events() {
    let events = [
        test_google_event("busy", Some("confirmed"), None, None),
        test_google_event("cancelled", Some("cancelled"), None, None),
        test_google_event("transparent", Some("confirmed"), Some("transparent"), None),
        test_google_event("declined", Some("confirmed"), None, Some("declined")),
        test_google_event("tentative", Some("tentative"), None, Some("tentative")),
    ];
    let policy = test_policy();
    let busy = events
        .iter()
        .filter(|event| event_is_visible(&policy, event, None, EventVisibility::Active))
        .filter(|event| event_blocks_time(event))
        .map(|event| event_id(event).as_str())
        .collect::<Vec<_>>();

    assert_eq!(busy, vec!["busy", "tentative"]);
}

/// Title filtering must consume later provider pages until it fills the
/// semantic page and return the cursor after every consumed provider row.
#[test]
fn title_filter_fills_page_across_provider_pages() {
    let policy = test_policy();
    let mut pages = VecDeque::from([
        (
            2,
            None,
            test_backend_page(
                vec![
                    test_google_event("other", None, None, None),
                    test_google_event("planning-one", None, None, None),
                ],
                Some("google:p1"),
            ),
        ),
        (
            1,
            Some("google:p1"),
            test_backend_page(
                vec![test_google_event("planning-two", None, None, None)],
                Some("google:p2"),
            ),
        ),
    ]);

    let page = collect_semantic_page(
        2,
        None,
        SEMANTIC_PAGE_BUDGET,
        |limit, cursor| {
            let (expected_limit, expected_cursor, page) =
                pages.pop_front().expect("scripted provider page");
            assert_eq!(limit, expected_limit);
            assert_eq!(cursor, expected_cursor);
            Ok(page)
        },
        |event| event_is_visible(&policy, event, Some("planning"), EventVisibility::Active),
    )
    .expect("semantic page");

    assert_eq!(
        page.events
            .iter()
            .map(|event| event_id(event).as_str())
            .collect::<Vec<_>>(),
        vec!["planning-one", "planning-two"]
    );
    assert_eq!(page.next_cursor.as_deref(), Some("google:p2"));
    assert!(pages.is_empty());
}

/// A transparent event must not produce an empty free/busy page when a
/// blocking event exists on the provider's next page.
#[test]
fn free_busy_fills_page_after_transparent_provider_row() {
    let page = collect_blocking_page(
        test_google_event("transparent", None, Some("transparent"), None),
        test_google_event("busy", None, None, None),
    );

    assert_eq!(
        page.events
            .iter()
            .map(|event| event_id(event).as_str())
            .collect::<Vec<_>>(),
        vec!["busy"]
    );
    assert_eq!(page.next_cursor.as_deref(), Some("google:p2"));
}

/// A self-declined event must not produce an empty free/busy page when a
/// blocking event exists on the provider's next page.
#[test]
fn free_busy_fills_page_after_self_declined_provider_row() {
    let page = collect_blocking_page(
        test_google_event("declined", None, None, Some("declined")),
        test_google_event("busy", None, None, None),
    );

    assert_eq!(
        page.events
            .iter()
            .map(|event| event_id(event).as_str())
            .collect::<Vec<_>>(),
        vec!["busy"]
    );
    assert_eq!(page.next_cursor.as_deref(), Some("google:p2"));
}

/// Advancing empty provider pages must stop at the request budget instead of
/// issuing an unbounded sequence of provider calls.
#[test]
fn semantic_page_rejects_advancing_empty_pages_after_budget() {
    let mut request = 0;
    let error = semantic_page_error(collect_semantic_page(
        1,
        None,
        SemanticPageBudget {
            max_provider_requests: 2,
            max_provider_rows: 10,
        },
        |_limit, _cursor| {
            request += 1;
            Ok(test_backend_page(
                Vec::new(),
                Some(&format!("google:p{request}")),
            ))
        },
        |_| true,
    ));

    assert_eq!(request, 2);
    assert!(error.contains("provider page budget"), "{error}");
}

/// Provider rows rejected by semantic filters must still count toward the row
/// budget so dense irrelevant results cannot bypass the scan bound.
#[test]
fn semantic_page_rejects_provider_row_budget_exhaustion() {
    let error = semantic_page_error(collect_semantic_page(
        2,
        None,
        SemanticPageBudget {
            max_provider_requests: 2,
            max_provider_rows: 1,
        },
        |_limit, _cursor| {
            Ok(test_backend_page(
                vec![
                    test_google_event("filtered-one", None, None, None),
                    test_google_event("filtered-two", None, None, None),
                ],
                Some("google:next"),
            ))
        },
        |_| false,
    ));

    assert!(error.contains("provider row budget"), "{error}");
}

/// A provider cursor cycle must fail as soon as any prior token repeats, not
/// only when a token repeats on adjacent pages.
#[test]
fn semantic_page_rejects_two_token_cursor_cycle() {
    let cursors = ["google:a", "google:b", "google:a"];
    let mut request = 0;
    let error = semantic_page_error(collect_semantic_page(
        1,
        None,
        SEMANTIC_PAGE_BUDGET,
        |_limit, _cursor| {
            let next = cursors[request];
            request += 1;
            Ok(test_backend_page(Vec::new(), Some(next)))
        },
        |_| true,
    ));

    assert_eq!(request, 3);
    assert!(error.contains("repeated pagination cursor"), "{error}");
}

/// Cursor-only continuation must retain the normalized query and reject a
/// second query field, preventing models from changing visibility mid-stream.
#[test]
fn cursor_round_trips_query_and_rejects_mixed_arguments() {
    let account = ValidatedAccount {
        id: "feed".to_owned(),
        enable: true,
        display_name: None,
        backend: None,
        default_calendar: Some("main".to_owned()),
        allowed_calendars: vec!["main".to_owned()],
        timezone: Some("UTC".to_owned()),
    };
    let calendar = flatten_calendar_id(&account.id, "main");
    let cursor = CalendarCursor::encode_next(
        Some("ics:20".to_owned()),
        &CalendarCursorQuery::search(
            &calendar,
            "2026-06-02T00:00:00Z",
            "2026-06-03T00:00:00Z",
            20,
            Some("planning"),
            true,
        )
        .expect("valid search cursor query"),
    )
    .expect("cursor serializes")
    .expect("next cursor");
    let args = CalendarRangeArgs {
        cursor: Some(cursor.clone()),
        ..Default::default()
    };
    let continuation = calendar_continuation(&args, CalendarCursorSelector::Search)
        .expect("cursor parses")
        .expect("continuation");
    let reconstructed = continuation.continuation_args();
    assert_eq!(reconstructed.calendar.as_deref(), Some("feed/main"));
    assert_eq!(reconstructed.start.as_deref(), Some("2026-06-02T00:00:00Z"));
    assert_eq!(reconstructed.end.as_deref(), Some("2026-06-03T00:00:00Z"));
    assert_eq!(reconstructed.limit, Some(20));
    assert_eq!(reconstructed.cursor.as_deref(), Some("ics:20"));
    assert_eq!(reconstructed.title.as_deref(), Some("planning"));
    assert_eq!(reconstructed.include_cancelled, Some(true));
    let wrong_command = calendar_continuation(
        &CalendarRangeArgs {
            cursor: Some(cursor),
            ..Default::default()
        },
        CalendarCursorSelector::FreeBusy,
    )
    .expect_err("search cursor cannot continue free/busy");
    assert!(wrong_command.contains("different calendar query"));

    let free_busy_cursor = CalendarCursor::encode_next(
        Some("ics:20".to_owned()),
        &CalendarCursorQuery::free_busy(
            &calendar,
            "2026-06-02T00:00:00Z",
            "2026-06-03T00:00:00Z",
            20,
        )
        .expect("valid free/busy cursor query"),
    )
    .expect("free/busy cursor serializes")
    .expect("free/busy next cursor");
    let free_busy = calendar_continuation(
        &CalendarRangeArgs {
            cursor: Some(free_busy_cursor),
            ..Default::default()
        },
        CalendarCursorSelector::FreeBusy,
    )
    .expect("free/busy cursor parses")
    .expect("free/busy continuation");
    let free_busy_args = free_busy.continuation_args();
    assert_eq!(free_busy_args.calendar.as_deref(), Some("feed/main"));
    assert_eq!(
        free_busy_args.start.as_deref(),
        Some("2026-06-02T00:00:00Z")
    );
    assert_eq!(free_busy_args.end.as_deref(), Some("2026-06-03T00:00:00Z"));
    assert_eq!(free_busy_args.limit, Some(20));
    assert_eq!(free_busy_args.cursor.as_deref(), Some("ics:20"));
    assert_eq!(free_busy_args.title, None);
    assert_eq!(free_busy_args.include_cancelled, None);

    let mixed = CalendarRangeArgs {
        cursor: args.cursor,
        limit: Some(100),
        ..Default::default()
    };
    let error = calendar_continuation(&mixed, CalendarCursorSelector::Search)
        .expect_err("mixed cursor rejected");
    assert!(error.contains("retry with cursor only"));
}

/// Calendar command arguments must be named maps; serde's positional struct
/// representation is not part of the model-visible API.
#[test]
fn range_args_reject_positional_arrays() {
    let invocation = ToolInvocation {
        command: CalendarCommand::ListEvents,
        args: Some(CborValue::Array(vec![CborValue::Text(
            "feed/main".to_owned(),
        )])),
    };

    let error =
        parse_invocation_args::<CalendarRangeArgs>(&invocation).expect_err("array is rejected");
    assert!(error.contains("expected a map of named fields"), "{error}");

    let temp = tempfile::TempDir::new().expect("tempdir");
    let output = dispatch_test(
        &test_engine(temp.path()),
        CborValue::Array(vec![CborValue::Text("list_events".to_owned())]),
    );
    assert_eq!(cbor_bool_field(&output, "ok"), Some(false));
    assert_eq!(
        cbor_nested_text_field(&output, "error", "message"),
        Some("invalid calendar tool arguments: expected a map of named fields")
    );
}

/// A mistyped cancellation-discovery flag must return the contract's short,
/// actionable Boolean repair message.
#[test]
fn range_args_use_exact_include_cancelled_type_repair() {
    let invocation = ToolInvocation {
        command: CalendarCommand::ListEvents,
        args: Some(cbor_map(vec![(
            "include_cancelled",
            CborValue::Text("yes".to_owned()),
        )])),
    };

    let error = match parse_invocation_args::<CalendarRangeArgs>(&invocation) {
        Ok(_) => panic!("non-Boolean flag was accepted"),
        Err(error) => error,
    };
    assert_eq!(error, "include_cancelled must be true or false");
}

/// Runtime normalization must match the advertised 20-row default and reject
/// requests above the explicit 100-row maximum.
#[test]
fn range_limit_defaults_to_twenty_and_rejects_above_one_hundred() {
    assert_eq!(normalized_limit(None).expect("default limit"), 20);
    assert_eq!(normalized_limit(Some(100)).expect("maximum limit"), 100);
    assert_eq!(
        normalized_limit(Some(101)),
        Err("limit must be at most 100".to_owned())
    );
}

/// Build a compact Google event for pure calendar visibility regressions.
fn test_google_event(
    id: &str,
    status: Option<&str>,
    transparency: Option<&str>,
    self_response_status: Option<&str>,
) -> BackendEvent {
    BackendEvent::Google(GoogleEventRecord {
        id: EventId::new(id.to_owned()),
        etag: None,
        i_cal_uid: None,
        summary: id.to_owned(),
        description: None,
        location: None,
        start: "2026-06-02T09:00:00Z".to_owned(),
        end: "2026-06-02T10:00:00Z".to_owned(),
        status: status.map(str::to_owned),
        visibility: None,
        transparency: transparency.map(str::to_owned),
        organizer: None,
        attendees: Vec::new(),
        self_response_status: self_response_status.map(str::to_owned),
        recurring: false,
    })
}

/// Extract a semantic-page error without requiring successful pages to
/// implement debug formatting.
fn semantic_page_error(result: Result<BackendEventPage, String>) -> String {
    match result {
        Ok(_) => panic!("semantic page unexpectedly succeeded"),
        Err(error) => error,
    }
}

/// Build one scripted provider page for semantic pagination tests.
fn test_backend_page(events: Vec<BackendEvent>, next_cursor: Option<&str>) -> BackendEventPage {
    let scanned_events = events.len();
    BackendEventPage {
        events,
        next_cursor: next_cursor.map(str::to_owned),
        truncated: next_cursor.is_some(),
        scanned_events,
    }
}

/// Exercise free/busy filtering across two deterministic provider pages.
fn collect_blocking_page(filtered: BackendEvent, busy: BackendEvent) -> BackendEventPage {
    let policy = test_policy();
    let mut pages = VecDeque::from([
        (None, test_backend_page(vec![filtered], Some("google:p1"))),
        (
            Some("google:p1"),
            test_backend_page(vec![busy], Some("google:p2")),
        ),
    ]);
    let page = collect_semantic_page(
        1,
        None,
        SEMANTIC_PAGE_BUDGET,
        |limit, cursor| {
            assert_eq!(limit, 1);
            let (expected_cursor, page) = pages.pop_front().expect("scripted provider page");
            assert_eq!(cursor, expected_cursor);
            Ok(page)
        },
        |event| {
            event_is_visible(&policy, event, None, EventVisibility::Active)
                && event_blocks_time(event)
        },
    )
    .expect("semantic free/busy page");
    assert!(pages.is_empty());
    page
}

/// Return the ordinary permissive read policy used by visibility tests.
fn test_policy() -> ValidatedPolicy {
    ValidatedPolicy {
        read: ValidatedReadPolicy {
            private_events: PrivateEventsPolicy::BusyOnly,
            descriptions: DescriptionPolicy::Always,
        },
        write: ValidatedWritePolicy {
            require_approval: true,
            max_attendees: 50,
        },
    }
}

#[test]
fn read_event_can_use_single_recent_event_for_agent() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let engine = test_engine(temp.path());
    let agent_id = AgentId::parse("agent").expect("agent id");
    let account = ValidatedAccount {
        id: "feed".to_owned(),
        enable: true,
        display_name: None,
        backend: None,
        default_calendar: Some("main".to_owned()),
        allowed_calendars: vec!["main".to_owned()],
        timezone: Some("UTC".to_owned()),
    };
    let event = BackendEvent::Ics(IcsEventRecord {
        id: EventId::new("evt".to_owned()),
        uid: ICalUid::new("uid".to_owned()),
        summary: "Tau Testing Party".to_owned(),
        description: None,
        location: None,
        start: "2026-05-28".to_owned(),
        end: "2026-05-29".to_owned(),
        start_utc: None,
        end_utc: None,
        status: None,
        organizer: None,
        attendees: Vec::new(),
        private: false,
        recurring: false,
        time_unparsed: false,
    });
    engine.remember_visible_events(&agent_id, &account, "main", &[&event]);

    let event_id = engine
        .resolve_read_event_id(&agent_id, &account, "main", None)
        .expect("single recent event id");

    assert_eq!(event_id, "evt");
}

/// Implicit recent-event resolution must scope identical raw values by account
/// and calendar, and must behave identically for Google and ICS records.
#[test]
fn recent_event_resolution_retains_namespace_and_provider_parity() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let engine = test_engine(temp.path());
    let agent_id = AgentId::parse("agent").expect("agent id");
    let account = |id: &str, calendar: &str| ValidatedAccount {
        id: id.to_owned(),
        enable: true,
        display_name: None,
        backend: None,
        default_calendar: Some(calendar.to_owned()),
        allowed_calendars: vec![calendar.to_owned()],
        timezone: Some("UTC".to_owned()),
    };
    let feed = account("feed", "main");
    let other_feed = account("other-feed", "main");
    let google = account("google", "primary");
    let ics = BackendEvent::Ics(IcsEventRecord {
        id: EventId::new("shared"),
        uid: ICalUid::new("shared"),
        summary: "ICS event".to_owned(),
        description: None,
        location: None,
        start: "2026-05-28".to_owned(),
        end: "2026-05-29".to_owned(),
        start_utc: None,
        end_utc: None,
        status: None,
        organizer: None,
        attendees: Vec::new(),
        private: false,
        recurring: false,
        time_unparsed: false,
    });

    engine.remember_visible_events(&agent_id, &feed, "main", &[&ics]);
    assert_eq!(
        engine
            .resolve_read_event_id(&agent_id, &feed, "main", None)
            .expect("ICS namespace matches"),
        "shared"
    );
    assert!(
        engine
            .resolve_read_event_id(&agent_id, &other_feed, "main", None)
            .is_err()
    );
    assert!(
        engine
            .resolve_read_event_id(&agent_id, &feed, "secondary", None)
            .is_err()
    );

    let google_event = test_google_event("shared", Some("confirmed"), None, None);
    engine.remember_visible_events(&agent_id, &google, "primary", &[&google_event]);
    assert_eq!(
        engine
            .resolve_read_event_id(&agent_id, &google, "primary", None)
            .expect("Google namespace matches"),
        "shared"
    );
    assert!(
        engine
            .resolve_read_event_id(&agent_id, &feed, "main", None)
            .is_err()
    );
}

/// Persisted wildcard ETags must be rejected before any mutation execution can
/// reach the provider backend.
#[test]
fn persisted_wildcard_etag_is_rejected_before_execution() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let engine = google_test_engine(temp.path());
    let mut change = CalendarChangeApproval::pending("delete_event", "google", "primary");
    change.event_id = Some("evt".to_owned());
    change.etag = Some("*".to_owned());

    let error = engine
        .validate_persisted_change(&change)
        .expect_err("wildcard must be rejected");

    assert_eq!(error, "calendar change contains unsafe wildcard etag");
}

#[test]
fn natural_date_bounds_are_accepted_without_configured_timezone() {
    parse_range(
        &CalendarRangeArgs {
            start: Some("2 days".to_owned()),
            ..Default::default()
        },
        &ValidatedAccount {
            id: "google".to_owned(),
            enable: true,
            display_name: None,
            backend: None,
            default_calendar: Some("primary".to_owned()),
            allowed_calendars: vec!["primary".to_owned()],
            timezone: None,
        },
    )
    .expect("natural date without configured timezone");
}

#[test]
fn duplicate_account_ids_are_rejected() {
    let cfg = CalendarExtensionConfig {
        enable: true,
        accounts: vec![
            CalendarAccountConfig {
                id: "work".to_owned(),
                ..Default::default()
            },
            CalendarAccountConfig {
                id: "work".to_owned(),
                ..Default::default()
            },
        ],
        ..Default::default()
    };

    let err = match cfg.validate() {
        Ok(_) => panic!("duplicate ids should fail"),
        Err(err) => err,
    };
    assert!(err.contains("duplicate calendar account id"), "{err}");

    let slash_cfg = CalendarExtensionConfig {
        enable: true,
        accounts: vec![CalendarAccountConfig {
            id: "work/account".to_owned(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let slash_err = slash_cfg.validate().err().expect("slash id rejected");
    assert!(
        slash_err.contains("calendar account id must not contain `/`"),
        "{slash_err}"
    );
}

#[test]
fn ics_feed_requires_exactly_one_url_source() {
    let cfg = CalendarExtensionConfig {
        enable: true,
        accounts: vec![CalendarAccountConfig {
            id: "feed".to_owned(),
            backend: Some(CalendarBackendConfig::IcsFeed {
                url_secret: None,
                url: None,
                allow_plain_http: false,
            }),
            ..Default::default()
        }],
        ..Default::default()
    };

    let err = match cfg.validate() {
        Ok(_) => panic!("missing feed source should fail"),
        Err(err) => err,
    };
    assert!(err.contains("requires exactly one"), "{err}");
}

/// Literal iCalendar feed URLs should fail configuration early when they use
/// non-loopback plain HTTP, while still permitting loopback test feeds and
/// explicit dangerous opt-in.
#[test]
fn ics_feed_plain_http_requires_loopback_or_opt_in() {
    let cfg = |url: &str, allow_plain_http| CalendarExtensionConfig {
        enable: true,
        accounts: vec![CalendarAccountConfig {
            id: "feed".to_owned(),
            backend: Some(CalendarBackendConfig::IcsFeed {
                url_secret: None,
                url: Some(url.to_owned()),
                allow_plain_http,
            }),
            ..Default::default()
        }],
        ..Default::default()
    };

    assert!(
        cfg("http://example.test/calendar.ics", false)
            .validate()
            .is_err()
    );
    cfg("http://127.0.0.1/calendar.ics", false)
        .validate()
        .expect("loopback HTTP is accepted for tests");
    cfg("http://example.test/calendar.ics", true)
        .validate()
        .expect("explicit opt-in accepts plain HTTP");
}

#[test]
fn calendar_config_secrets_are_checked_at_configure_time() {
    // Calendar provider credentials and private feed URLs must fail during
    // Configure, not later when a model invokes a tool and gets a partial
    // runtime-specific error.
    let cfg = CalendarExtensionConfig {
        enable: true,
        accounts: vec![CalendarAccountConfig {
            id: "google".to_owned(),
            enable: true,
            backend: Some(CalendarBackendConfig::Google {
                client_id_secret: "client".to_owned(),
                client_secret_secret: Some("client_secret".to_owned()),
                refresh_token_secret: Some("refresh".to_owned()),
                api_base: None,
            }),
            calendars: CalendarSelectionConfig {
                default: Some("primary".to_owned()),
                allow: vec!["primary".to_owned()],
            },
            ..Default::default()
        }],
        ..Default::default()
    };
    let config = cfg.validate().expect("shape validates");
    let mut secrets = BTreeMap::new();
    secrets.insert("client".to_owned(), SecretValue::new("client-id"));

    let err = validate_config_secrets(&config, &secrets).expect_err("missing secrets reject");

    assert!(err.contains("client_secret"), "{err}");
}

#[test]
fn calendar_secret_feed_url_is_validated_at_configure_time() {
    // A secret-backed feed URL is still a provider endpoint carrying private
    // bearer-like data, so apply the same URL policy before accepting config.
    let cfg = CalendarExtensionConfig {
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
    let config = cfg.validate().expect("shape validates");
    let mut secrets = BTreeMap::new();
    secrets.insert(
        "feed_url".to_owned(),
        SecretValue::new("http://example.test/private.ics"),
    );

    let err = validate_config_secrets(&config, &secrets).expect_err("unsafe secret URL rejects");

    assert!(err.contains("https:// or webcal://"), "{err}");
}

#[test]
fn denied_calendar_change_tombstone_blocks_stale_pending_approval() {
    // Denial tombstones are fail-closed. If pending deletion previously failed
    // after writing a denial, approval must not execute the stale pending record.
    let temp = tempfile::TempDir::new().expect("tempdir");
    let engine = google_test_engine(temp.path());
    let mut change = CalendarChangeApproval::pending("delete_event", "google", "primary");
    change.event_id = Some("evt".to_owned());
    let id = engine
        .state
        .pending_change(&change)
        .expect("pending change");
    let pending_path = temp
        .path()
        .join("state/approvals/calendar-change/pending")
        .join(format!("{id}.json"));
    let pending_bytes = std::fs::read(&pending_path).expect("read pending");

    engine.state.deny_change(&id).expect("deny change");
    std::fs::write(&pending_path, pending_bytes).expect("restore stale pending");

    let err = engine
        .action_change_approve(&id)
        .expect_err("denied tombstone wins");

    assert!(err.contains("was denied"), "{err}");
    assert!(
        engine
            .state
            .change_pending_exists(&id)
            .expect("pending remains")
    );
    assert!(engine.state.claim_change(&id).is_err());
}

fn test_engine(root: &std::path::Path) -> Engine {
    let cfg = CalendarExtensionConfig {
        enable: true,
        accounts: vec![CalendarAccountConfig {
            id: "feed".to_owned(),
            enable: true,
            display_name: Some("Feed".to_owned()),
            backend: Some(CalendarBackendConfig::IcsFeed {
                url_secret: None,
                url: Some("https://example.test/calendar.ics".to_owned()),
                allow_plain_http: false,
            }),
            calendars: CalendarSelectionConfig {
                default: Some("main".to_owned()),
                allow: vec!["main".to_owned()],
            },
            timezone: Some("UTC".to_owned()),
        }],
        ..Default::default()
    };
    Engine {
        config: cfg.validate().expect("valid config"),
        state: StateStore::open(root.join("state")).expect("state"),
        google: GoogleBackend::new(BTreeMap::new()),
        ics_feed: IcsFeedBackend::new(BTreeMap::new()),
        etags: RefCell::new(BTreeMap::new()),
        last_events: RefCell::new(BTreeMap::new()),
    }
}

fn google_test_engine(root: &std::path::Path) -> Engine {
    let cfg = CalendarExtensionConfig {
        enable: true,
        accounts: vec![CalendarAccountConfig {
            id: "google".to_owned(),
            enable: true,
            backend: Some(CalendarBackendConfig::Google {
                client_id_secret: "client".to_owned(),
                client_secret_secret: None,
                refresh_token_secret: Some("refresh".to_owned()),
                api_base: None,
            }),
            calendars: CalendarSelectionConfig {
                default: Some("primary".to_owned()),
                allow: vec!["primary".to_owned()],
            },
            timezone: Some("UTC".to_owned()),
            ..Default::default()
        }],
        ..Default::default()
    };
    Engine {
        config: cfg.validate().expect("valid config"),
        state: StateStore::open(root.join("state")).expect("state"),
        google: GoogleBackend::new(BTreeMap::new()),
        ics_feed: IcsFeedBackend::new(BTreeMap::new()),
        etags: RefCell::new(BTreeMap::new()),
        last_events: RefCell::new(BTreeMap::new()),
    }
}

fn empty_change_args() -> ChangeArgs {
    ChangeArgs {
        calendar: None,
        event_id: None,
        title: None,
        description: None,
        location: None,
        start: None,
        end: None,
        attendees: None,
        response: None,
    }
}

fn dispatch_test(engine: &Engine, arguments: CborValue) -> CborValue {
    engine.dispatch(&arguments, &AgentId::parse("test-agent").expect("agent id"))
}

fn command_args(command: &str, args: Vec<(&str, CborValue)>) -> CborValue {
    cbor_map(vec![
        ("command", CborValue::Text(command.to_owned())),
        ("args", cbor_map(args)),
    ])
}

fn tool_started(tool_name: &str, args: Vec<(&str, CborValue)>) -> ToolStarted {
    ToolStarted {
        invocation_policy: tau_proto::ToolInvocationPolicy::default(),
        call_id: tau_proto::ToolCallId::from("call-1"),
        tool_name: tau_proto::ToolName::new(tool_name),
        arguments: cbor_map(args),
        agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
        originator: tau_proto::PromptOriginator::User,
    }
}
fn cbor_map(entries: Vec<(&str, CborValue)>) -> CborValue {
    CborValue::Map(
        entries
            .into_iter()
            .map(|(key, value)| (CborValue::Text(key.to_owned()), value))
            .collect(),
    )
}
