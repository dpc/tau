//! Fake-only transport and original-preservation oracles.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, mpsc};
use std::thread;

use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;

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
    for edit in [false, true] {
        for (status, expected) in [
            (400, GenerationError::Refused),
            (401, GenerationError::Authentication),
            (403, GenerationError::Authentication),
            (422, GenerationError::Refused),
            (429, GenerationError::Quota),
            (302, GenerationError::Transport),
            (503, GenerationError::Transport),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
            let retained = listener.try_clone().expect("retained listener");
            let endpoint = format!(
                "http://{}/images/{}",
                listener.local_addr().expect("address"),
                if edit { "edits" } else { "generations" }
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
            let network =
                tau_provider::OutboundNetworkPolicy::from_environment(BTreeMap::new(), None);
            let error = generate_images_at(
                credentials(),
                "test",
                "turn",
                &network,
                &GenerationCancellation::default(),
                if edit {
                    vec![InputImage::validate(original()).expect("input")]
                } else {
                    Vec::new()
                },
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
}

/// Adds a synthetic PNG chunk with its real CRC so animation detection is
/// exercised independently of malformed-container rejection.
fn png_chunk(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut chunk = (body.len() as u32).to_be_bytes().to_vec();
    chunk.extend_from_slice(kind);
    chunk.extend_from_slice(body);
    let mut crc = !0u32;
    for byte in &chunk[4..] {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    chunk.extend_from_slice(&(!crc).to_be_bytes());
    chunk
}

/// Builds finite RIFF chunks used by a fully synthetic animated WebP fixture.
fn riff_chunk(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut chunk = kind.to_vec();
    chunk.extend_from_slice(&(body.len() as u32).to_le_bytes());
    chunk.extend_from_slice(body);
    if !body.len().is_multiple_of(2) {
        chunk.push(0);
    }
    chunk
}

/// Animation must not silently flatten to the first frame; valid decodable
/// oversized images must also fail before they can become edit inputs.
#[test]
fn animated_and_oversized_edit_inputs_are_rejected() {
    let mut apng = original();
    let animation = png_chunk(b"acTL", &[0, 0, 0, 1, 0, 0, 0, 0]);
    apng.splice(33..33, animation);
    let decoder = PngDecoder::new(Cursor::new(&apng)).expect("APNG header");
    assert!(decoder.is_apng().expect("animation"));
    assert!(matches!(
        InputImage::validate(apng),
        Err(GenerationError::InvalidInput)
    ));

    let image = image::RgbImage::from_pixel(2, 2, image::Rgb([1, 3, 7]));
    let mut webp = Cursor::new(Vec::new());
    image
        .write_to(&mut webp, image::ImageFormat::WebP)
        .expect("WebP");
    let mut chunks = riff_chunk(b"VP8X", &[2, 0, 0, 0, 1, 0, 0, 1, 0, 0]);
    chunks.extend(riff_chunk(b"ANIM", &[0; 6]));
    let mut frame = vec![0; 16];
    frame[6] = 1;
    frame[9] = 1;
    frame.extend_from_slice(&webp.into_inner()[12..]);
    chunks.extend(riff_chunk(b"ANMF", &frame));
    let mut animated = b"RIFF".to_vec();
    animated.extend_from_slice(&((chunks.len() + 4) as u32).to_le_bytes());
    animated.extend_from_slice(b"WEBP");
    animated.extend(chunks);
    let decoder = WebPDecoder::new(Cursor::new(&animated)).expect("animated WebP");
    assert!(decoder.has_animation());
    assert!(matches!(
        InputImage::validate(animated),
        Err(GenerationError::InvalidInput)
    ));

    let mut oversized = Cursor::new(Vec::new());
    image::RgbImage::new(8193, 1)
        .write_to(&mut oversized, image::ImageFormat::Png)
        .expect("wide PNG");
    assert!(matches!(
        InputImage::validate(oversized.into_inner()),
        Err(GenerationError::InvalidInput)
    ));
}

/// Dropping a response connection has an uncertain outcome, never a second
/// request or a fallback to generation.
#[test]
fn edit_connection_loss_does_not_retry() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let retained = listener.try_clone().expect("retained");
    let endpoint = format!(
        "http://{}/images/edits",
        listener.local_addr().expect("address")
    );
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("connection");
        assert!(read_request(&mut stream).starts_with("POST /images/edits "));
    });
    let network = tau_provider::OutboundNetworkPolicy::from_environment(BTreeMap::new(), None);
    assert_eq!(
        generate_images_at(
            credentials(),
            "test",
            "turn",
            &network,
            &GenerationCancellation::default(),
            vec![InputImage::validate(original()).expect("input")],
            &endpoint,
            Duration::from_secs(5)
        ),
        Err(GenerationError::Transport)
    );
    server.join().expect("server");
    retained.set_nonblocking(true).expect("nonblocking");
    assert_eq!(
        retained.accept().expect_err("no retry").kind(),
        std::io::ErrorKind::WouldBlock
    );
}

/// Cancellation and deadline must interrupt a stalled response, without waiting
/// for provider output or submitting a replacement request.
#[test]
fn stalled_generation_cancellation_and_timeout_are_uncertain() {
    for edit in [false, true] {
        for cancel in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
            let endpoint = format!(
                "http://{}/images/{}",
                listener.local_addr().expect("address"),
                if edit { "edits" } else { "generations" }
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
            let network =
                tau_provider::OutboundNetworkPolicy::from_environment(BTreeMap::new(), None);
            let error = generate_images_at(
                credentials(),
                "test",
                "turn",
                &network,
                &cancelled,
                if edit {
                    vec![InputImage::validate(original()).expect("input")]
                } else {
                    Vec::new()
                },
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
}

/// Edits preserve every ordered original occurrence and the generation response
/// contract, without exposing arbitrary URL or file-upload authority.
#[test]
fn fake_edits_preserve_order_mime_originals_and_selected_account() {
    let png = original();
    let image = image::RgbImage::from_pixel(2, 2, image::Rgb([7, 11, 19]));
    let mut jpeg = Cursor::new(Vec::new());
    image
        .write_to(&mut jpeg, image::ImageFormat::Jpeg)
        .expect("JPEG");
    let mut webp = Cursor::new(Vec::new());
    image
        .write_to(&mut webp, image::ImageFormat::WebP)
        .expect("WebP");
    let originals = [
        png.clone(),
        jpeg.into_inner(),
        webp.into_inner(),
        png.clone(),
    ];
    for count in [1, 4] {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let endpoint = format!(
            "http://{}/images/edits",
            listener.local_addr().expect("address")
        );
        let body = response_body(&png);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("connection");
            let request = read_request(&mut stream);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .expect("headers");
            stream.write_all(&body).expect("body");
            request
        });
        let network = tau_provider::OutboundNetworkPolicy::from_environment(BTreeMap::new(), None);
        let result = generate_images_at(
            credentials(),
            "use image 1",
            "edit-turn",
            &network,
            &GenerationCancellation::default(),
            originals[..count]
                .iter()
                .cloned()
                .map(|bytes| InputImage::validate(bytes).expect("input"))
                .collect(),
            &endpoint,
            Duration::from_secs(5),
        )
        .expect("edit");
        assert_eq!(result, png);
        let request = server.join().expect("server");
        assert!(request.starts_with("POST /images/edits "));
        let (headers, body) = request.split_once("\r\n\r\n").expect("split");
        let headers = headers.to_ascii_lowercase();
        for header in [
            "authorization: bearer fake-bearer-canary",
            "chatgpt-account-id: fake-account-canary",
            "x-codex-image-turn-id: edit-turn",
            "originator: tau",
        ] {
            assert!(headers.contains(header));
        }
        let value: serde_json::Value = serde_json::from_str(body).expect("JSON");
        assert_eq!(value["model"], "gpt-image-2");
        assert_eq!(value["n"], 1);
        assert_eq!(value.as_object().expect("object").len(), 4);
        let images = value["images"].as_array().expect("images");
        assert_eq!(images.len(), count);
        for (index, image) in images.iter().enumerate() {
            let mime = ["image/png", "image/jpeg", "image/webp", "image/png"][index];
            assert_eq!(
                image,
                &serde_json::json!({"image_url":format!("data:{mime};base64,{}", STANDARD.encode(&originals[index]))})
            );
        }
    }
}

/// Invalid originals and pre-cancellation fail locally, before a network
/// connection can disclose content or incur provider work.
#[test]
fn invalid_edit_inputs_and_cancellation_never_connect() {
    let mut truncated = original();
    truncated.truncate(truncated.len() / 2);
    for bytes in [b"not an image".to_vec(), b"GIF89a".to_vec(), truncated] {
        assert!(matches!(
            InputImage::validate(bytes),
            Err(GenerationError::InvalidInput)
        ));
    }
    assert!(matches!(
        InputImage::validate(vec![0; MAX_IMAGE_BYTES + 1]),
        Err(GenerationError::TooLarge)
    ));
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let endpoint = format!(
        "http://{}/images/edits",
        listener.local_addr().expect("address")
    );
    let network = tau_provider::OutboundNetworkPolicy::from_environment(BTreeMap::new(), None);
    let cancel = GenerationCancellation::default();
    cancel.cancel();
    assert_eq!(
        generate_images_at(
            credentials(),
            "test",
            "turn",
            &network,
            &cancel,
            vec![InputImage::validate(original()).expect("input")],
            &endpoint,
            Duration::from_secs(1)
        ),
        Err(GenerationError::Cancelled)
    );
    listener.set_nonblocking(true).expect("nonblocking");
    assert_eq!(
        listener.accept().expect_err("no connection").kind(),
        std::io::ErrorKind::WouldBlock
    );
}

/// Local raw/count and independently padded data-URL limits apply before send,
/// including a 24-MiB raw boundary which cannot fit its URL prefixes.
#[test]
fn edit_request_budgets_count_padding_prefixes_and_overflow() {
    assert_eq!(data_url_size(0, "image/png").expect("empty"), 22);
    assert_eq!(data_url_size(1, "image/png").expect("one"), 26);
    assert_eq!(data_url_size(3, "image/png").expect("three"), 26);
    assert_eq!(
        data_url_size(usize::MAX, "image/png"),
        Err(GenerationError::TooLarge)
    );
    let cancel = GenerationCancellation::default();
    let synthetic = |size| InputImage {
        mime: "image/png",
        bytes: vec![0; size],
    };
    // Two multiples of three fit the URL budget exactly (including prefixes).
    let first = MAX_IMAGE_BYTES / 3 * 3;
    let raw_boundary = (MAX_DATA_URL_BYTES - 2 * 22) / 4 * 3;
    let boundary = request_body(
        &"\u{0000}".repeat(MAX_PROMPT_BYTES),
        vec![synthetic(first), synthetic(raw_boundary - first)],
        &cancel,
    )
    .expect("exact encoded boundary and worst-case escaped prompt");
    assert!(boundary.len() <= MAX_REQUEST_BYTES);
    drop(boundary);
    assert_eq!(
        request_body(
            "test",
            vec![synthetic(first), synthetic(raw_boundary - first + 1)],
            &cancel,
        ),
        Err(GenerationError::TooLarge)
    );
    assert_eq!(
        request_body("test", (0..6).map(|_| synthetic(1)).collect(), &cancel),
        Err(GenerationError::TooLarge)
    );
    assert_eq!(
        request_body(
            "test",
            vec![
                synthetic(MAX_IMAGE_BYTES),
                synthetic(MAX_INPUT_BYTES - MAX_IMAGE_BYTES)
            ],
            &cancel
        ),
        Err(GenerationError::TooLarge)
    );
    assert_eq!(
        request_body(
            "test",
            vec![synthetic(MAX_IMAGE_BYTES), synthetic(MAX_IMAGE_BYTES)],
            &cancel
        ),
        Err(GenerationError::TooLarge)
    );
    let body = request_body("test", (0..5).map(|_| synthetic(1)).collect(), &cancel).expect("five");
    assert!(body.len() < MAX_REQUEST_BYTES);
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
