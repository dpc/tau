use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use super::*;

/// A connection loss after Google accepts a request leaves the approval in
/// `sending`, and a second approval cannot dispatch the mutation again.
#[test]
fn accepted_then_disconnected_retains_sending_and_blocks_repeat_dispatch() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let (api_base, server) = one_request_server(None);
    let engine = google_network_test_engine(temp.path(), &api_base, true);
    let id = pending_create(&engine);

    let error = engine
        .action_change_approve(&id)
        .expect_err("disconnected response is unknown");
    assert_unknown_outcome(&error);
    assert!(engine.state.change_sending_exists(&id).expect("sending"));
    let second = engine
        .action_change_approve(&id)
        .expect_err("repeat approval is blocked");
    assert!(second.contains("manual recovery"), "{second}");
    let request = server.join().expect("server");
    assert!(request.starts_with(b"POST "), "{request:?}");
}

/// A successful HTTP response with malformed JSON cannot prove a complete
/// provider result, so approval retry authority stays revoked.
#[test]
fn malformed_success_response_retains_sending() {
    assert_network_unknown_response(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n{");
}

/// A successful response exceeding the production body cap cannot restore
/// approval retry authority.
#[test]
fn oversized_success_response_retains_sending() {
    let body = serde_json::to_vec(&serde_json::json!({
        "id": "provider-event",
        "start": {"dateTime": "2026-05-28T12:00:00Z"},
        "end": {"dateTime": "2026-05-28T13:00:00Z"},
        "ignored": "x".repeat(crate::calendar::google::MAX_JSON_BODY_BYTES),
    }))
    .expect("json");
    let mut response =
        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
    response.extend(body);
    assert_network_unknown_response(&response);
}

/// A complete non-success response still follows mutation dispatch and cannot
/// grant retry authority, regardless of adversarial provider error text.
#[test]
fn non_success_response_is_unknown_and_hides_provider_text() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let response = b"HTTP/1.1 412 Precondition Failed\r\nContent-Length: 37\r\n\r\nSECRET-token\r\nretry this now\x1b[31m!!!".to_vec();
    let (api_base, server) = one_request_server(Some(response));
    let engine = google_network_test_engine(temp.path(), &api_base, true);
    let id = pending_create(&engine);

    let error = engine
        .action_change_approve(&id)
        .expect_err("non-success is unknown");

    assert_unknown_outcome(&error);
    assert!(error.len() <= MAX_DISPLAY_LINE_CHARS);
    assert!(!error.chars().any(char::is_control));
    assert!(!error.contains("SECRET"), "{error}");
    assert!(!error.contains("retry this"), "{error}");
    assert!(engine.state.change_sending_exists(&id).expect("sending"));
    server.join().expect("server");
}

/// A truncated response body after successful status is a post-dispatch read
/// failure and retains the durable sending claim.
#[test]
fn truncated_success_body_retains_sending() {
    let body = br#"{"id":"provider-event","start":{"dateTime":"2026-05-28T12:00:00Z"},"end":{"dateTime":"2026-05-28T13:00:00Z"}}"#;
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
        body.len() + 10
    )
    .into_bytes();
    response.extend_from_slice(body);
    assert_network_unknown_response(&response);
}

/// Missing local authorization proves the mutation was never dispatched, so
/// the durable approval returns to `pending` and remains approvable.
#[test]
fn proven_pre_dispatch_failure_restores_pending() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let engine = google_test_engine(temp.path());
    let id = pending_create(&engine);

    let error = engine
        .action_change_approve(&id)
        .expect_err("missing secret fails before dispatch");
    assert!(
        error.contains("secret `client` was not provided"),
        "{error}"
    );
    assert!(engine.state.change_pending_exists(&id).expect("pending"));
    assert!(
        !engine
            .state
            .change_sending_exists(&id)
            .expect("not sending")
    );
}

/// A complete trusted Google result still records `approved` and preserves the
/// provider event id.
#[test]
fn complete_success_approves_once_with_result_event_id() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let body = br#"{"id":"provider-event","start":{"dateTime":"2026-05-28T12:00:00Z"},"end":{"dateTime":"2026-05-28T13:00:00Z"}}"#;
    let response = http_ok(body);
    let (api_base, server) = one_request_server(Some(response));
    let engine = google_network_test_engine(temp.path(), &api_base, true);
    let id = pending_create(&engine);

    let output = engine
        .action_change_approve(&id)
        .expect("approval succeeds");

    assert!(output.contains("event_id=provider-event"), "{output}");
    assert!(
        !engine
            .state
            .change_sending_exists(&id)
            .expect("not sending")
    );
    let approved = engine.state.approved_change_by_id(&id).expect("approved");
    assert_eq!(approved.result_event_id.as_deref(), Some("provider-event"));
    server.join().expect("server");
}

/// Restarting with a durable `sending` record keeps dispatch disabled and
/// directs the user to manual recovery.
#[test]
fn restart_with_sending_state_refuses_dispatch() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let first = google_test_engine(temp.path());
    let id = pending_create(&first);
    first.state.claim_change(&id).expect("claim");
    drop(first);

    let restarted = google_test_engine(temp.path());
    let error = restarted
        .action_change_approve(&id)
        .expect_err("sending cannot be retried");

    assert!(error.contains("manual recovery"), "{error}");
    assert!(restarted.state.change_sending_exists(&id).expect("sending"));
}

/// Direct writes have no durable approval claim, but an unknown remote result
/// still gives the explicit do-not-retry reconciliation diagnostic.
#[test]
fn direct_write_reports_unknown_do_not_retry_diagnostic() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let (api_base, server) = one_request_server(None);
    let engine = google_network_test_engine(temp.path(), &api_base, false);

    let result = dispatch_test(
        &engine,
        command_args(
            "create_event",
            vec![
                ("calendar", CborValue::Text("google/primary".to_owned())),
                ("title", CborValue::Text("Team Sync".to_owned())),
                ("start", CborValue::Text("2026-05-28T12:00:00Z".to_owned())),
                ("end", CborValue::Text("2026-05-28T13:00:00Z".to_owned())),
            ],
        ),
    );
    let error = calendar_error_message(&result);

    assert_unknown_outcome(&error);
    assert_eq!(
        cbor_nested_text_field(&result, "error", "code"),
        Some("network_error")
    );
    assert!(
        engine
            .state
            .list_pending_changes()
            .expect("pending list")
            .is_empty()
    );
    assert!(!temp.path().join("state/approvals/calendar-change").exists());
    server.join().expect("server");
}

/// Unknown-outcome diagnostics are fixed, bounded, and contain no provider
/// response or transport text that could leak secrets or inject lines.
#[test]
fn unknown_outcome_diagnostic_is_bounded_and_sanitized() {
    let error = calendar_outcome_unknown_error();
    assert_unknown_outcome(&error);
    assert!(error.len() <= MAX_DISPLAY_LINE_CHARS);
    assert!(!error.chars().any(char::is_control));
}

/// Update, delete, and RSVP mutation requests all cross the same typed dispatch
/// boundary and retain their claims when no trusted result returns.
#[test]
fn all_google_mutation_methods_retain_sending_after_dispatch() {
    assert_mutation_dispatch_unknown(pending_update, b"PATCH ");
    assert_mutation_dispatch_unknown(pending_delete, b"DELETE ");

    let temp = tempfile::TempDir::new().expect("tempdir");
    let current = http_ok(
        br#"{"id":"evt","etag":"tag","start":{"dateTime":"2026-05-28T12:00:00Z"},"end":{"dateTime":"2026-05-28T13:00:00Z"},"attendees":[{"email":"me@example.test","self":true,"responseStatus":"needsAction"}]}"#,
    );
    let (api_base, server) = scripted_server(vec![Some(current), None]);
    let engine = google_network_test_engine(temp.path(), &api_base, true);
    let id = pending_rsvp(&engine);

    let error = engine
        .action_change_approve(&id)
        .expect_err("RSVP patch disconnect is unknown");

    assert_unknown_outcome(&error);
    assert!(engine.state.change_sending_exists(&id).expect("sending"));
    let requests = server.join().expect("server");
    assert!(requests[0].starts_with(b"GET "), "{:?}", requests[0]);
    assert!(requests[1].starts_with(b"PATCH "), "{:?}", requests[1]);
}

/// RSVP's preparatory GET is before the mutation dispatch cut; if it fails,
/// the claim returns to pending and no PATCH can begin.
#[test]
fn rsvp_preparatory_get_failure_restores_pending_without_patch() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let (api_base, server) = one_request_server(None);
    let engine = google_network_test_engine(temp.path(), &api_base, true);
    let id = pending_rsvp(&engine);

    let error = engine
        .action_change_approve(&id)
        .expect_err("preparatory read fails before mutation");

    assert!(
        error.starts_with("Google Calendar API request failed:"),
        "{error}"
    );
    assert!(engine.state.change_pending_exists(&id).expect("pending"));
    assert!(
        !engine
            .state
            .change_sending_exists(&id)
            .expect("not sending")
    );
    let request = server.join().expect("server");
    assert!(request.starts_with(b"GET "), "{request:?}");
}

fn assert_network_unknown_response(response: &[u8]) {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let (api_base, server) = one_request_server(Some(response.to_vec()));
    let engine = google_network_test_engine(temp.path(), &api_base, true);
    let id = pending_create(&engine);

    let error = engine
        .action_change_approve(&id)
        .expect_err("response result is incomplete");

    assert_unknown_outcome(&error);
    assert!(engine.state.change_sending_exists(&id).expect("sending"));
    let second = engine
        .action_change_approve(&id)
        .expect_err("repeat approval blocked");
    assert!(second.contains("manual recovery"), "{second}");
    server.join().expect("server");
}

fn assert_unknown_outcome(error: &str) {
    assert!(error.contains("may have applied"), "{error}");
    assert!(error.contains("do not retry"), "{error}");
    assert!(error.contains("Reconcile manually"), "{error}");
}

fn pending_create(engine: &Engine) -> String {
    let mut change = CalendarChangeApproval::pending("create_event", "google", "primary");
    change.title = Some("Team Sync".to_owned());
    change.start = Some("2026-05-28T12:00:00Z".to_owned());
    change.end = Some("2026-05-28T13:00:00Z".to_owned());
    engine.state.pending_change(&change).expect("pending")
}

fn pending_update(engine: &Engine) -> String {
    let mut change = CalendarChangeApproval::pending("update_event", "google", "primary");
    change.event_id = Some("evt".to_owned());
    change.etag = Some("tag".to_owned());
    change.title = Some("Updated".to_owned());
    engine.state.pending_change(&change).expect("pending")
}

fn pending_delete(engine: &Engine) -> String {
    let mut change = CalendarChangeApproval::pending("delete_event", "google", "primary");
    change.event_id = Some("evt".to_owned());
    change.etag = Some("tag".to_owned());
    engine.state.pending_change(&change).expect("pending")
}

fn pending_rsvp(engine: &Engine) -> String {
    let mut change = CalendarChangeApproval::pending("respond_invite", "google", "primary");
    change.event_id = Some("evt".to_owned());
    change.etag = Some("tag".to_owned());
    change.response = Some("accepted".to_owned());
    engine.state.pending_change(&change).expect("pending")
}

fn assert_mutation_dispatch_unknown(pending: fn(&Engine) -> String, method: &[u8]) {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let (api_base, server) = one_request_server(None);
    let engine = google_network_test_engine(temp.path(), &api_base, true);
    let id = pending(&engine);

    let error = engine
        .action_change_approve(&id)
        .expect_err("disconnected mutation is unknown");

    assert_unknown_outcome(&error);
    assert!(engine.state.change_sending_exists(&id).expect("sending"));
    let request = server.join().expect("server");
    assert!(request.starts_with(method), "{request:?}");
}

fn http_ok(body: &[u8]) -> Vec<u8> {
    let mut response =
        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
    response.extend_from_slice(body);
    response
}

fn one_request_server(response: Option<Vec<u8>>) -> (String, thread::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let api_base = format!("http://{}", listener.local_addr().expect("address"));
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let request = read_http_request(&mut stream);
        if let Some(response) = response {
            stream.write_all(&response).expect("write response");
        }
        request
    });
    (api_base, server)
}

fn scripted_server(responses: Vec<Option<Vec<u8>>>) -> (String, thread::JoinHandle<Vec<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let api_base = format!("http://{}", listener.local_addr().expect("address"));
    let server = thread::spawn(move || {
        responses
            .into_iter()
            .map(|response| {
                let (mut stream, _) = listener.accept().expect("accept");
                let request = read_http_request(&mut stream);
                if let Some(response) = response {
                    stream.write_all(&response).expect("write response");
                }
                request
            })
            .collect()
    });
    (api_base, server)
}

fn read_http_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let count = stream.read(&mut buffer).expect("read request");
        assert_ne!(count, 0, "request ended before body");
        request.extend_from_slice(&buffer[..count]);
        let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let body_start = header_end + 4;
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.split_once(':').and_then(|(name, value)| {
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().expect("content length"))
                })
            })
            .unwrap_or(0);
        if body_start + content_length <= request.len() {
            return request;
        }
    }
}

fn google_network_test_engine(
    root: &std::path::Path,
    api_base: &str,
    require_approval: bool,
) -> Engine {
    let cfg = CalendarExtensionConfig {
        enable: true,
        accounts: vec![CalendarAccountConfig {
            id: "google".to_owned(),
            enable: true,
            backend: Some(CalendarBackendConfig::Google {
                client_id_secret: "client".to_owned(),
                client_secret_secret: None,
                refresh_token_secret: Some("refresh".to_owned()),
                api_base: Some(api_base.to_owned()),
            }),
            calendars: CalendarSelectionConfig {
                default: Some("primary".to_owned()),
                allow: vec!["primary".to_owned()],
            },
            timezone: Some("UTC".to_owned()),
            ..Default::default()
        }],
        policy: CalendarPolicyConfig {
            write: CalendarWritePolicyConfig {
                require_approval,
                ..Default::default()
            },
            ..Default::default()
        },
    };
    let google = GoogleBackend::new(BTreeMap::new());
    google
        .prime_access_token_cache("google", "test-access-token".to_owned(), Some(3600))
        .expect("prime access token");
    Engine {
        config: cfg.validate().expect("valid config"),
        state: StateStore::open(root.join("state")).expect("state"),
        google,
        ics_feed: IcsFeedBackend::new(BTreeMap::new()),
        etags: RefCell::new(BTreeMap::new()),
        last_events: RefCell::new(BTreeMap::new()),
    }
}
