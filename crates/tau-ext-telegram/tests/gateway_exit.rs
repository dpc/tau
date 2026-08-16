//! Subprocess coverage for the gateway's stable service-manager exit contract.

use std::io::{ErrorKind, Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// Child process that is always killed and reaped if a fixture assertion
/// unwinds before explicit shutdown.
struct ChildGuard {
    /// Spawned gateway process killed and reaped when the guard drops.
    child: Option<Child>,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Serialize subprocess fixtures because every test launches the same Cargo
/// binary and several intentionally keep a gateway process alive.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().expect("gateway subprocess test lock")
}

/// Build a gateway command with isolated filesystem roots and one valid user.
fn command(temp: &TempDir) -> Command {
    let client_secret = temp.path().join("gateway-client-secret");
    std::fs::write(&client_secret, "11".repeat(32)).expect("write gateway client secret");
    let mut command = Command::new(env!("CARGO_BIN_EXE_tau-telegram-gateway"));
    command
        .env("TELEGRAM_BOT_TOKEN", "secret-token")
        .arg("--allowed-user-id")
        .arg("1")
        .arg("--client-secret-file")
        .arg(client_secret)
        .arg("--state-dir")
        .arg(temp.path().join("state"))
        .arg("--runtime-dir")
        .arg(temp.path().join("runtime"));
    command
}

/// Run one command and require its numeric exit status.
fn assert_exit(mut command: Command, expected: i32) -> Output {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = ChildGuard {
        child: Some(command.spawn().expect("run gateway subprocess")),
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child
            .child
            .as_mut()
            .expect("guarded gateway subprocess")
            .try_wait()
            .expect("inspect gateway subprocess")
            .is_some()
        {
            break;
        }
        if deadline <= Instant::now() {
            child
                .child
                .as_mut()
                .expect("guarded gateway subprocess")
                .kill()
                .expect("kill timed-out gateway subprocess");
            child
                .child
                .as_mut()
                .expect("guarded gateway subprocess")
                .wait()
                .expect("reap timed-out gateway subprocess");
            panic!("gateway subprocess did not exit before deadline");
        }
        thread::sleep(Duration::from_millis(1));
    }
    let output = child
        .child
        .take()
        .expect("guarded gateway subprocess")
        .wait_with_output()
        .expect("collect gateway subprocess");
    assert_eq!(output.status.code(), Some(expected), "{output:?}");
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("secret-token"),
        "stderr leaked the token: {output:?}"
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains(&"11".repeat(32)),
        "stderr leaked the gateway client secret: {output:?}"
    );
    output
}

/// Start a bounded loopback HTTP server with one response per connection.
fn server(responses: Vec<(u16, &'static str)>) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback server");
    listener
        .set_nonblocking(true)
        .expect("make loopback server nonblocking");
    let address = listener.local_addr().expect("loopback address");
    let worker = thread::spawn(move || {
        for (status, body) in responses {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "Bot API request deadline expired"
                        );
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("accept Bot API request: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("set read timeout");
            read_request(&mut stream);
            write!(
                stream,
                "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: application/json\r\n\r\n{body}",
                body.len()
            )
            .expect("write Bot API response");
        }
    });
    (format!("http://{address}"), worker)
}

/// Start a one-shot server that emits an exact raw response for malformed-body
/// boundary coverage.
fn raw_server(response: Vec<u8>) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind raw loopback server");
    listener
        .set_nonblocking(true)
        .expect("make raw loopback server nonblocking");
    let address = listener.local_addr().expect("raw loopback address");
    let worker = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "raw Bot API request deadline expired"
                    );
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("accept raw Bot API request: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set raw read timeout");
        read_request(&mut stream);
        stream.write_all(&response).expect("write raw response");
    });
    (format!("http://{address}"), worker)
}

/// Consume one complete bounded HTTP request before closing the connection.
fn read_request(stream: &mut std::net::TcpStream) {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read = stream.read(&mut chunk).expect("read Bot API request");
        request.extend_from_slice(&chunk[..read]);
        let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
            assert!(request.len() < 8192, "oversized fixture request");
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(str::trim)
                    .map(str::parse::<usize>)
            })
            .transpose()
            .expect("valid content length")
            .unwrap_or(0);
        if header_end + 4 + content_length <= request.len() {
            return;
        }
    }
}

/// Help is successful, while malformed flags and missing token values use
/// EX_USAGE rather than Rust's generic termination status.
#[test]
fn help_and_usage_exit_codes_are_stable() {
    let _serial = serial();
    let temp = tempfile::tempdir().expect("tempdir");
    let mut help = Command::new(env!("CARGO_BIN_EXE_tau-telegram-gateway"));
    help.arg("--help");
    assert_exit(help, 0);

    let mut bad = command(&temp);
    bad.arg("--unknown");
    assert_exit(bad, 64);

    let mut missing_token = command(&temp);
    missing_token.env_remove("TELEGRAM_BOT_TOKEN");
    assert_exit(missing_token, 64);

    let mut empty_token = command(&temp);
    empty_token.env("TELEGRAM_BOT_TOKEN", " \t ");
    assert_exit(empty_token, 64);
}

/// Semantic endpoint, allowlist, and poll configuration errors use EX_CONFIG.
#[test]
fn semantic_configuration_uses_ex_config() {
    let _serial = serial();
    let temp = tempfile::tempdir().expect("tempdir");
    let mut bad_api = command(&temp);
    bad_api.arg("--api-base").arg("not a URL");
    assert_exit(bad_api, 78);

    let mut missing_allowlist = Command::new(env!("CARGO_BIN_EXE_tau-telegram-gateway"));
    missing_allowlist
        .env("TELEGRAM_BOT_TOKEN", "secret-token")
        .arg("--state-dir")
        .arg(temp.path().join("state-no-allowlist"))
        .arg("--runtime-dir")
        .arg(temp.path().join("runtime-no-allowlist"));
    assert_exit(missing_allowlist, 78);

    let mut bad_poll = command(&temp);
    bad_poll.arg("--poll-timeout-seconds").arg("0");
    assert_exit(bad_poll, 78);

    for status in [401, 409] {
        let temp = tempfile::tempdir().expect("tempdir");
        let (base, worker) = server(vec![(status, "secret-token")]);
        let mut rejected = command(&temp);
        rejected.arg("--api-base").arg(base);
        assert_exit(rejected, 78);
        worker.join().expect("join permanent-rejection server");
    }

    for response in [
        b"HTTP/1.1 401 Test\r\nContent-Length: 2\r\nConnection: close\r\n\r\n\xff\xfe".to_vec(),
        b"HTTP/1.1 401 Test\r\nContent-Length: 20\r\nConnection: close\r\n\r\nshort".to_vec(),
    ] {
        let temp = tempfile::tempdir().expect("tempdir");
        let (base, worker) = raw_server(response);
        let mut rejected = command(&temp);
        rejected.arg("--api-base").arg(base);
        assert_exit(rejected, 78);
        worker.join().expect("join malformed-body server");
    }
}

/// Current and previous credential slots load from exact files, reject invalid
/// rotation state as EX_CONFIG, and never echo credential contents.
#[test]
fn gateway_authentication_credential_configuration_is_strict_and_redacted() {
    let _serial = serial();

    for contents in ["short-secret".to_owned(), format!("{}\n", "22".repeat(32))] {
        let temp = tempfile::tempdir().expect("tempdir");
        let previous = temp.path().join("previous-client-secret");
        std::fs::write(&previous, &contents).expect("write malformed previous credential");
        let mut malformed = command(&temp);
        malformed
            .arg("--previous-client-secret-file")
            .arg(&previous);
        let output = assert_exit(malformed, 78);
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains(&contents),
            "stderr leaked malformed credential contents: {output:?}"
        );
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let malformed_current = temp.path().join("malformed-current-secret");
    std::fs::write(&malformed_current, "not-a-key").expect("write malformed current credential");
    let mut invalid_current = command(&temp);
    invalid_current
        .arg("--client-secret-file")
        .arg(&malformed_current);
    let output = assert_exit(invalid_current, 78);
    assert!(!String::from_utf8_lossy(&output.stderr).contains("not-a-key"));

    let temp = tempfile::tempdir().expect("tempdir");
    let mut unreadable = command(&temp);
    unreadable
        .arg("--previous-client-secret-file")
        .arg(temp.path().join("does-not-exist"));
    assert_exit(unreadable, 78);

    let temp = tempfile::tempdir().expect("tempdir");
    let duplicate = temp.path().join("duplicate-client-secret");
    std::fs::write(&duplicate, "11".repeat(32)).expect("write duplicate credential");
    let mut duplicate_slots = command(&temp);
    duplicate_slots
        .arg("--previous-client-secret-file")
        .arg(&duplicate);
    assert_exit(duplicate_slots, 78);

    let temp = tempfile::tempdir().expect("tempdir");
    let previous = temp.path().join("previous-client-secret");
    std::fs::write(&previous, "77".repeat(32)).expect("write previous credential");
    let (base, worker) = server(vec![(401, "rejected")]);
    let mut distinct_slots = command(&temp);
    distinct_slots
        .arg("--previous-client-secret-file")
        .arg(&previous)
        .arg("--api-base")
        .arg(base);
    let output = assert_exit(distinct_slots, 78);
    assert!(!String::from_utf8_lossy(&output.stderr).contains(&"77".repeat(32)));
    worker.join().expect("join credential preflight server");
}

/// Webhook preflight maps transient HTTP statuses and refused transport to
/// EX_TEMPFAIL, and redacts tokens copied into remote response bodies.
#[test]
fn webhook_preflight_transient_failures_use_ex_tempfail() {
    let _serial = serial();
    for status in [408, 425, 429, 500] {
        let temp = tempfile::tempdir().expect("tempdir");
        let body = if status == 500 {
            Box::leak(format!("\0\rsecret-token{}", "é".repeat(1024)).into_boxed_str())
        } else {
            "secret-token"
        };
        let (base, worker) = server(vec![(status, body)]);
        let mut gateway = command(&temp);
        gateway.arg("--api-base").arg(base);
        let output = assert_exit(gateway, 75);
        if status == 500 {
            assert!(output.stderr.len() <= 1200, "stderr was not bounded");
            assert!(!output.stderr.contains(&0), "stderr retained NUL");
            assert!(
                !output.stderr.contains(&b'\r'),
                "stderr retained carriage return"
            );
        }
        worker.join().expect("join loopback server");
    }

    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve refused port");
    let address = listener.local_addr().expect("reserved address");
    drop(listener);
    let temp = tempfile::tempdir().expect("tempdir");
    let mut gateway = command(&temp);
    gateway.arg("--api-base").arg(format!("http://{address}"));
    assert_exit(gateway, 75);
}

/// Active webhooks and post-preflight getUpdates HTTP 409 are explicit
/// EX_UNAVAILABLE ownership failures. The active-webhook fixture also prevents
/// successful remote diagnostics that reflect the bot token from leaking it to
/// gateway stderr while retaining useful ownership-error context.
#[test]
fn webhook_and_runtime_conflict_use_ex_unavailable() {
    let _serial = serial();
    let temp = tempfile::tempdir().expect("tempdir");
    let (base, worker) = server(vec![(
        200,
        r#"{"ok":true,"result":{"url":"https://example.invalid/hook","last_error_message":"delivery failed for secret-token after retry"}}"#,
    )]);
    let mut webhook = command(&temp);
    webhook.arg("--api-base").arg(base);
    let output = assert_exit(webhook, 69);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("delivery failed for <redacted> after retry"),
        "{stderr}"
    );
    assert!(!stderr.contains("secret-token"), "{stderr}");
    worker.join().expect("join webhook server");

    let temp = tempfile::tempdir().expect("tempdir");
    let large_body = format!(
        r#"{{"ok":true,"padding":"{}","result":{{"url":"https://example.invalid/hook"}}}}"#,
        "x".repeat(2048)
    );
    let large_body = Box::leak(large_body.into_boxed_str());
    let (base, worker) = server(vec![(200, large_body)]);
    let mut webhook = command(&temp);
    webhook.arg("--api-base").arg(base);
    assert_exit(webhook, 69);
    worker.join().expect("join large webhook server");

    let temp = tempfile::tempdir().expect("tempdir");
    let (base, worker) = server(vec![
        (200, r#"{"ok":true,"result":{"url":""}}"#),
        (409, "secret-token"),
    ]);
    let mut conflict = command(&temp);
    conflict.arg("--api-base").arg(base);
    assert_exit(conflict, 69);
    worker.join().expect("join conflict server");
}

/// A malformed successful Bot API response is an unexpected software/protocol
/// failure rather than a retryable transport or configuration failure.
#[test]
fn malformed_preflight_response_uses_ex_software() {
    let _serial = serial();
    let temp = tempfile::tempdir().expect("tempdir");
    let (base, worker) = server(vec![(200, "not-json secret-token")]);
    let mut gateway = command(&temp);
    gateway.arg("--api-base").arg(base);
    assert_exit(gateway, 70);
    worker.join().expect("join malformed-response server");
}

/// A second process for one stream fails with EX_UNAVAILABLE while the first
/// process retains the advisory lock.
#[test]
fn second_stream_owner_uses_ex_unavailable() {
    let _serial = serial();
    let temp = tempfile::tempdir().expect("tempdir");
    let (base, worker) = server(vec![(200, r#"{"ok":true,"result":{"url":""}}"#)]);
    let mut first_command = command(&temp);
    first_command
        .arg("--api-base")
        .arg(&base)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut first = ChildGuard {
        child: Some(first_command.spawn().expect("start first gateway")),
    };
    wait_for_lock(&temp, first.child.as_mut().expect("guarded first gateway"));
    worker.join().expect("join preflight server");

    let mut second = command(&temp);
    second.arg("--api-base").arg(base);
    assert_exit(second, 69);
    first
        .child
        .as_mut()
        .expect("guarded first gateway")
        .kill()
        .expect("stop first gateway");
    first
        .child
        .as_mut()
        .expect("guarded first gateway")
        .wait()
        .expect("reap first gateway");
}

/// Corrupt state and unusable runtime paths use EX_IOERR.
#[test]
fn local_filesystem_failures_use_ex_ioerr() {
    let _serial = serial();
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("state"), b"not a directory").expect("write blocking path");
    let gateway = command(&temp);
    assert_exit(gateway, 74);
}

/// Wait until the first gateway creates its stream-lock file without using a
/// fixed scheduling sleep.
fn wait_for_lock(temp: &TempDir, child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(child.try_wait().expect("inspect child").is_none());
        if walk_contains_lock(temp.path()) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "gateway did not acquire stream lock"
        );
        thread::yield_now();
    }
}

/// Recursively identify the non-secret stream lock fixture.
fn walk_contains_lock(path: &std::path::Path) -> bool {
    let Ok(entries) = std::fs::read_dir(path) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        path.extension()
            .is_some_and(|extension| extension == "lock")
            || (path.is_dir() && walk_contains_lock(&path))
    })
}
