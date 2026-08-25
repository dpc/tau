use std::cell::Cell;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

use super::*;

#[derive(Clone, Copy)]
/// Final behavior selected for one deterministic loopback SMTP transaction.
enum ScriptedSmtpResult {
    /// Accept the complete DATA block.
    Accept,
    /// Reject MAIL before the client can enter DATA.
    RejectBeforeData,
    /// Reject the complete DATA block with a conclusive negative reply.
    RejectAfterData,
    /// Read the complete DATA block, then close without its final reply.
    DisconnectAfterData,
    /// Reject password or XOAUTH2 authentication before DATA.
    RejectAuth,
}

/// Provider-side effects observed by the scripted SMTP server.
struct SmtpObservation {
    /// Number of complete DATA blocks received from the client.
    data_blocks: usize,
    /// Authentication mechanisms selected without retaining credential
    /// payloads.
    auth_commands: Vec<String>,
}

fn spawn_scripted_smtp(result: ScriptedSmtpResult) -> (u16, thread::JoinHandle<SmtpObservation>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind SMTP server");
    let port = listener.local_addr().expect("SMTP address").port();
    let handle = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept SMTP client");
        run_smtp_script(stream, result)
    });
    (port, handle)
}

fn spawn_oauth_retry_smtp(
    submission_result: ScriptedSmtpResult,
) -> (u16, thread::JoinHandle<Vec<SmtpObservation>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind SMTP server");
    let port = listener.local_addr().expect("SMTP address").port();
    let handle = thread::spawn(move || {
        [ScriptedSmtpResult::RejectAuth, submission_result]
            .into_iter()
            .map(|result| {
                let (stream, _) = listener.accept().expect("accept SMTP client");
                run_smtp_script(stream, result)
            })
            .collect()
    });
    (port, handle)
}

fn run_smtp_script(stream: TcpStream, result: ScriptedSmtpResult) -> SmtpObservation {
    let mut writer = stream.try_clone().expect("clone SMTP stream");
    let mut reader = BufReader::new(stream);
    writer
        .write_all(b"220 localhost ready\r\n")
        .expect("greeting");
    writer.flush().expect("flush greeting");
    let mut data_blocks = 0;
    let mut auth_commands = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).expect("read SMTP command") == 0 {
            break;
        }
        let command = line.trim_end_matches(['\r', '\n']);
        if command.starts_with("EHLO ") {
            writer
                .write_all(b"250-localhost\r\n250 AUTH PLAIN LOGIN XOAUTH2\r\n")
                .expect("EHLO reply");
        } else if command.starts_with("AUTH ") {
            let mechanism = command
                .split_ascii_whitespace()
                .nth(1)
                .unwrap_or("<missing>");
            auth_commands.push(format!("AUTH {mechanism}"));
            if matches!(result, ScriptedSmtpResult::RejectAuth) {
                writer
                    .write_all(b"535 5.7.8 authentication rejected\r\n")
                    .expect("AUTH rejection");
                writer.flush().expect("flush AUTH rejection");
                break;
            } else {
                writer
                    .write_all(b"235 2.7.0 authenticated\r\n")
                    .expect("AUTH");
            }
        } else if command.starts_with("MAIL FROM:") {
            if matches!(result, ScriptedSmtpResult::RejectBeforeData) {
                writer
                    .write_all(b"550 5.1.0 sender rejected\r\n")
                    .expect("MAIL rejection");
            } else {
                writer.write_all(b"250 2.1.0 sender ok\r\n").expect("MAIL");
            }
        } else if command.starts_with("RCPT TO:") {
            writer
                .write_all(b"250 2.1.5 recipient ok\r\n")
                .expect("RCPT");
        } else if command == "DATA" {
            writer
                .write_all(b"354 send message\r\n")
                .expect("DATA reply");
            writer.flush().expect("flush DATA reply");
            loop {
                let mut data_line = String::new();
                assert_ne!(
                    reader.read_line(&mut data_line).expect("read DATA"),
                    0,
                    "client disconnected inside DATA"
                );
                if data_line == ".\r\n" {
                    break;
                }
            }
            data_blocks += 1;
            match result {
                ScriptedSmtpResult::Accept => writer
                    .write_all(b"250 2.0.0 queued\r\n")
                    .expect("accept DATA"),
                ScriptedSmtpResult::RejectAfterData => writer
                    .write_all(b"550 5.7.1 message rejected\r\n")
                    .expect("reject DATA"),
                ScriptedSmtpResult::DisconnectAfterData => break,
                ScriptedSmtpResult::RejectBeforeData | ScriptedSmtpResult::RejectAuth => {
                    panic!("unexpected DATA")
                }
            }
        } else if command == "QUIT" {
            writer.write_all(b"221 bye\r\n").expect("QUIT");
            writer.flush().expect("flush QUIT");
            break;
        } else {
            panic!("unexpected SMTP command: {command:?}");
        }
        writer.flush().expect("flush SMTP reply");
    }
    SmtpObservation {
        data_blocks,
        auth_commands,
    }
}

fn smtp_backend(
    temp: &tempfile::TempDir,
    port: u16,
    auth: Option<ValidatedAuthConfig>,
    secrets: BTreeMap<String, tau_proto::SecretValue>,
) -> RealEmailBackend {
    let state = StateStore::open(temp.path().join("state")).expect("state");
    let oauth = Arc::new(GoogleOauthClient::new(secrets.clone()));
    let account = RealAccount {
        id: "work".to_owned(),
        imap: None,
        smtp: Some(ValidatedSmtpConfig {
            host: "127.0.0.1".to_owned(),
            port,
            tls: TlsMode::None,
            login: "alice@example.com".to_owned(),
            timeout_seconds: 5,
        }),
        auth,
        secrets: Arc::new(secrets),
        state,
        oauth: Arc::clone(&oauth),
    };
    RealEmailBackend {
        accounts: BTreeMap::from([("work".to_owned(), account)]),
        runtime: Runtime::new().expect("runtime"),
        oauth,
    }
}

fn outgoing_message() -> OutgoingMessage {
    OutgoingMessage {
        account: "work".to_owned(),
        from: "alice@example.com".to_owned(),
        to: vec!["bob@example.com".to_owned()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: "production boundary".to_owned(),
        body_text: "one body".to_owned(),
        reply_to: None,
        in_reply_to: None,
    }
}

/// Ensures a rolling cutoff produces an early-enough IMAP calendar-date
/// superset instead of restoring the old UTC-calendar-day duration semantics.
#[test]
fn recent_search_date_is_conservative_for_rolling_cutoff() {
    let now: SystemTime = chrono::DateTime::parse_from_rfc3339("2026-05-24T00:30:00Z")
        .expect("test timestamp")
        .into();
    let cutoff = recent_cutoff(now, 1).expect("cutoff");

    assert_eq!(imap_since_date(cutoff).expect("IMAP date"), "22-May-2026");
}

/// Ensures stale candidates cannot force unbounded IMAP metadata requests or
/// rows while a recent listing searches for a full filtered page.
#[test]
fn recent_search_budget_rejects_request_and_candidate_exhaustion() {
    let mut request_budget = RecentSearchBudget::new(1, 1_000);
    assert_eq!(
        request_budget
            .next_fetch_end(0, 101)
            .expect("first fetch fits"),
        100
    );
    let request_error = request_budget
        .next_fetch_end(100, 101)
        .expect_err("second fetch exceeds request budget");
    assert!(request_error.contains("fetch budget"), "{request_error}");

    let mut candidate_budget = RecentSearchBudget::new(10, 100);
    assert_eq!(
        candidate_budget
            .next_fetch_end(0, 101)
            .expect("first fetch fits"),
        100
    );
    let candidate_error = candidate_budget
        .next_fetch_end(100, 101)
        .expect_err("second fetch exceeds candidate budget");
    assert!(
        candidate_error.contains("candidate-row budget"),
        "{candidate_error}"
    );
}

/// A server that reads the complete DATA block and then drops the connection
/// exercises the real SMTP path's ambiguous acceptance boundary without sleeps.
#[test]
fn accepted_data_disconnect_is_outcome_unknown_without_resend() {
    let (port, server) = spawn_scripted_smtp(ScriptedSmtpResult::DisconnectAfterData);
    let temp = tempfile::TempDir::new().expect("tempdir");
    let mut backend = smtp_backend(&temp, port, None, BTreeMap::new());

    let failure = backend
        .send_message(&outgoing_message())
        .expect_err("lost final reply");

    assert!(matches!(failure, EmailSendFailure::OutcomeUnknown(_)));
    assert_eq!(server.join().expect("SMTP server").data_blocks, 1);
}

/// Complete negative SMTP replies prove non-acceptance both before DATA and
/// after a submitted body, while complete success remains successful.
#[test]
fn smtp_completion_replies_classify_proven_outcomes() {
    for (result, expected_data_blocks, succeeds) in [
        (ScriptedSmtpResult::RejectBeforeData, 0, false),
        (ScriptedSmtpResult::RejectAfterData, 1, false),
        (ScriptedSmtpResult::Accept, 1, true),
    ] {
        let (port, server) = spawn_scripted_smtp(result);
        let temp = tempfile::TempDir::new().expect("tempdir");
        let mut backend = smtp_backend(&temp, port, None, BTreeMap::new());
        let result = backend.send_message(&outgoing_message());
        if succeeds {
            assert!(result.is_ok(), "{result:?}");
        } else {
            assert!(matches!(result, Err(EmailSendFailure::NotDispatched(_))));
        }
        assert_eq!(
            server.join().expect("SMTP server").data_blocks,
            expected_data_blocks
        );
    }
}

/// Password authentication rejection happens before submission and therefore
/// remains safely retryable with no DATA bytes sent.
#[test]
fn password_auth_rejection_is_not_dispatched() {
    let (port, server) = spawn_scripted_smtp(ScriptedSmtpResult::RejectAuth);
    let temp = tempfile::TempDir::new().expect("tempdir");
    let secrets = BTreeMap::from([("password".to_owned(), tau_proto::SecretValue::new("secret"))]);
    let auth = ValidatedAuthConfig {
        method: AuthMethod::Password,
        password_secret: Some("password".to_owned()),
        provider: None,
        client_id_secret: None,
        client_secret_secret: None,
        refresh_token_secret: None,
    };
    let mut backend = smtp_backend(&temp, port, Some(auth), secrets);

    let failure = backend
        .send_message(&outgoing_message())
        .expect_err("AUTH rejection");

    assert!(matches!(failure, EmailSendFailure::NotDispatched(_)));
    assert_eq!(server.join().expect("SMTP server").data_blocks, 0);
}

/// OAuth retries only rejected XOAUTH2 authentication on a fresh connection,
/// then performs one message submission and never retries ambiguous DATA.
#[test]
fn oauth_auth_retry_precedes_single_submission() {
    for submission_result in [
        ScriptedSmtpResult::Accept,
        ScriptedSmtpResult::DisconnectAfterData,
    ] {
        let (port, server) = spawn_oauth_retry_smtp(submission_result);
        let temp = tempfile::TempDir::new().expect("tempdir");
        let backend = smtp_backend(&temp, port, None, BTreeMap::new());
        let account = backend.account("work").expect("account");
        let message =
            build_lettre_message(&outgoing_message(), "<test@localhost>").expect("message");
        let stage = SmtpSubmissionStage::default();
        let refreshes = Cell::new(0);

        let result = backend
            .runtime
            .block_on(send_message_oauth2_with_token_refresh(
                &account,
                &message,
                &stage,
                "expired-token".to_owned(),
                || {
                    refreshes.set(refreshes.get() + 1);
                    Ok("fresh-token".to_owned())
                },
            ));

        assert_eq!(refreshes.get(), 1);
        if matches!(submission_result, ScriptedSmtpResult::Accept) {
            assert!(result.is_ok(), "{result:?}");
        } else {
            assert!(matches!(result, Err(EmailSendFailure::OutcomeUnknown(_))));
        }
        let observations = server.join().expect("SMTP server");
        assert_eq!(
            observations
                .iter()
                .flat_map(|observation| observation.auth_commands.iter())
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["AUTH XOAUTH2", "AUTH XOAUTH2"]
        );
        assert_eq!(
            observations
                .iter()
                .map(|observation| observation.data_blocks)
                .sum::<usize>(),
            1
        );
    }
}

/// The outer production deadline uses the same submission cut as transport
/// errors without relying on wall-clock sleeps in the regression suite.
#[test]
fn smtp_timeout_classification_tracks_submission_stage() {
    let (port, server) = spawn_scripted_smtp(ScriptedSmtpResult::DisconnectAfterData);
    let temp = tempfile::TempDir::new().expect("tempdir");
    let backend = smtp_backend(&temp, port, None, BTreeMap::new());
    let account = backend.account("work").expect("account");
    let smtp = account.smtp_config().expect("SMTP config");
    let message = build_lettre_message(&outgoing_message(), "<test@localhost>").expect("message");
    let stage = SmtpSubmissionStage::default();
    assert!(matches!(
        stage.timeout_failure(),
        EmailSendFailure::NotDispatched(_)
    ));
    let mut conn = backend
        .runtime
        .block_on(connect_smtp_for_auth(&account))
        .expect("connect SMTP");
    let result = backend
        .runtime
        .block_on(submit_smtp_message(&mut conn, &message, smtp, &stage));
    assert!(matches!(result, Err(EmailSendFailure::OutcomeUnknown(_))));
    assert!(matches!(
        stage.timeout_failure(),
        EmailSendFailure::OutcomeUnknown(_)
    ));
    assert_eq!(server.join().expect("SMTP server").data_blocks, 1);
}

/// Ensures the IMAP XOAUTH2 SASL payload matches Gmail's documented
/// `user=` and bearer-token control-A format exactly.
#[test]
fn xoauth2_payload_uses_gmail_sasl_format() {
    assert_eq!(
        xoauth2_payload("alice@example.com", "access-token"),
        "user=alice@example.com\x01auth=Bearer access-token\x01\x01"
    );
}

/// Ensures SMTP diagnostics redact the exact bearer token before they can
/// reach action/tool errors or logs.
#[test]
fn smtp_error_sanitizer_redacts_access_token() {
    let sanitized = sanitized_backend_error_redacting(
        "server rejected bearer ya29.secret-token during auth",
        "ya29.secret-token",
    );
    assert_eq!(sanitized, "server rejected bearer [redacted] during auth");
    assert!(!sanitized.contains("ya29.secret-token"));
}
