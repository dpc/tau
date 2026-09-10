//! Fake-only transport and original-preservation oracles.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, mpsc};
use std::thread;

use super::*;

/// Builds a transparent original entirely in memory, never from user files.
fn original() -> Vec<u8> {
    let image = image::RgbaImage::from_pixel(2, 2, image::Rgba([31, 47, 63, 0]));
    let mut bytes = Cursor::new(Vec::new());
    image
        .write_to(&mut bytes, image::ImageFormat::Png)
        .expect("encode fake PNG");
    bytes.into_inner()
}

/// Encodes the completed-response shape without injecting metadata into output.
fn response_body(bytes: &[u8]) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "created": 1,
        "data": [{"b64_json": base64::engine::general_purpose::STANDARD.encode(bytes)}]
    }))
    .expect("fake response")
}

/// Creates synthetic account material and deterministic no-proxy routing.
fn credentials() -> ResolvedCredentials {
    ResolvedCredentials::new(
        "fake-bearer-canary".to_owned(),
        Some("fake-account-canary".to_owned()),
    )
}

/// Reads the complete small fake HTTP request under a finite socket deadline.
fn read_request(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    let mut request = Vec::new();
    loop {
        let mut buffer = [0; 1024];
        let read = stream.read(&mut buffer).expect("read fake request");
        assert_ne!(read, 0);
        request.extend_from_slice(&buffer[..read]);
        let text = String::from_utf8_lossy(&request);
        if let Some((headers, body)) = text.split_once("\r\n\r\n") {
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|length| length.trim().parse::<usize>().ok())
                })
                .expect("content length");
            if body.len() >= length {
                return text.into_owned();
            }
        }
    }
}

/// Successful generation must preserve original bytes and bind endpoint, model,
/// bearer and account to exactly the supplied backend identity.
#[test]
fn fake_generation_preserves_png_and_selected_account() {
    let png = original();
    let body = response_body(&png);
    let listener = TcpListener::bind("127.0.0.1:0").expect("fake listener");
    let endpoint = format!(
        "http://{}/images/generations",
        listener.local_addr().expect("address")
    );
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("fake connection");
        let request = read_request(&mut stream);
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .expect("fake headers");
        stream.write_all(&body).expect("fake body");
        request
    });
    let network = tau_provider::OutboundNetworkPolicy::from_environment(BTreeMap::new(), None);
    let result = generate_at(
        credentials(),
        "transparent test",
        "fake-turn",
        &network,
        &GenerationCancellation::default(),
        &endpoint,
        Duration::from_secs(5),
    )
    .expect("one original");
    assert_eq!(result, png);
    let request = server.join().expect("fake server");
    assert!(request.starts_with("POST /images/generations "));
    let (headers, body) = request.split_once("\r\n\r\n").expect("request split");
    let headers = headers.to_ascii_lowercase();
    assert!(headers.contains("authorization: bearer fake-bearer-canary"));
    assert!(headers.contains("chatgpt-account-id: fake-account-canary"));
    assert!(headers.contains("x-codex-image-turn-id: fake-turn"));
    assert!(headers.contains("originator: tau"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(body).expect("request JSON"),
        serde_json::json!({"model":"gpt-image-2","prompt":"transparent test","n":1})
    );
}

/// Refusal/quota/auth/server errors must not retry or expose remote error
/// prose.
#[test]
fn fake_failures_are_single_attempt_and_byte_free() {
    for (status, expected) in [
        (400, GenerationError::Refused),
        (401, GenerationError::Authentication),
        (403, GenerationError::Authentication),
        (429, GenerationError::Quota),
        (503, GenerationError::Transport),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let retained = listener.try_clone().expect("retained listener");
        let endpoint = format!(
            "http://{}/images/generations",
            listener.local_addr().expect("address")
        );
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("connection");
            read_request(&mut stream);
            let body = b"remote-prose-and-image-canary";
            write!(
                stream,
                "HTTP/1.1 {status} Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .expect("headers");
            stream.write_all(body).expect("body");
        });
        let network = tau_provider::OutboundNetworkPolicy::from_environment(BTreeMap::new(), None);
        let error = generate_at(
            credentials(),
            "test",
            "turn",
            &network,
            &GenerationCancellation::default(),
            &endpoint,
            Duration::from_secs(5),
        )
        .expect_err("failure");
        assert_eq!(error, expected);
        assert!(!error.to_string().contains("canary"));
        server.join().expect("server");
        retained.set_nonblocking(true).expect("nonblocking");
        assert_eq!(
            retained.accept().expect_err("no repeated request").kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}

/// Cancellation and deadline must interrupt a stalled response, without waiting
/// for provider output or submitting a replacement request.
#[test]
fn stalled_generation_cancellation_and_timeout_are_uncertain() {
    for cancel in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let endpoint = format!(
            "http://{}/images/generations",
            listener.local_addr().expect("address")
        );
        let cancelled = Arc::new(GenerationCancellation::default());
        let server_cancelled = Arc::clone(&cancelled);
        let (finish, finished) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("connection");
            read_request(&mut stream);
            if cancel {
                server_cancelled.cancel();
            }
            finished
                .recv_timeout(Duration::from_secs(5))
                .expect("generation returned");
        });
        let network = tau_provider::OutboundNetworkPolicy::from_environment(BTreeMap::new(), None);
        let error = generate_at(
            credentials(),
            "test",
            "turn",
            &network,
            &cancelled,
            &endpoint,
            Duration::from_millis(150),
        )
        .expect_err("interrupted");
        assert_eq!(
            error,
            if cancel {
                GenerationError::Cancelled
            } else {
                GenerationError::Timeout
            }
        );
        assert!(error.to_string().contains("uncertain"));
        finish.send(()).expect("release server");
        server.join().expect("server");
    }
}

/// Empty, batch, malformed/base64, non-PNG and oversized originals never become
/// successful path results.
#[test]
fn original_response_requires_one_bounded_png() {
    assert_eq!(
        decode_original(br#"{"data":[]}"#),
        Err(GenerationError::InvalidImage)
    );
    assert_eq!(
        decode_original(br#"{"data":[{"b64_json":"!"}]}"#),
        Err(GenerationError::InvalidImage)
    );
    assert_eq!(
        decode_original(&response_body(b"not a PNG")),
        Err(GenerationError::InvalidImage)
    );
    let oversized = vec![0; MAX_IMAGE_BYTES + 1];
    assert_eq!(
        decode_original(&response_body(&oversized)),
        Err(GenerationError::TooLarge)
    );
    let image = serde_json::json!({"b64_json":base64::engine::general_purpose::STANDARD.encode(original())});
    let batch =
        serde_json::to_vec(&serde_json::json!({"data":[image.clone(),image]})).expect("batch");
    assert_eq!(decode_original(&batch), Err(GenerationError::InvalidImage));
}
