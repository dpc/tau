//! Synthetic documented catalog replies; no production credentials or traffic.

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

use super::*;

/// Optional values stay unknown; zero prices and future selectors stay exact.
#[test]
fn catalog_preserves_exact_metadata_without_inventing_capabilities() {
    let models = parse(br#"{"object":"list","data":[
        {"id":"reasoner","aliases":["reasoner-alias"],"context_length":256000,
         "prompt_text_token_price":20000,"cached_prompt_text_token_price":2000,
         "completion_text_token_price":80000,"prompt_image_token_price":0,
         "long_context_threshold":128000,"prompt_text_token_price_long_context":40000,
         "capabilities":{"reasoning_effort":["low","high","future"],"default_reasoning_effort":"high"}},
        {"id":"image-model","aliases":[],"image_price":200000000},
        {"id":"unknown","aliases":[],"context_length":null}
    ]}"#).expect("catalog");
    assert_eq!(models[0].aliases, ["reasoner-alias"]);
    assert_eq!(models[0].context_length, Some(256_000));
    assert_eq!(models[0].prompt_image_token_price, Some(0));
    assert_eq!(models[0].prompt_text_token_price, Some(20_000));
    assert_eq!(models[0].long_context_threshold, Some(128_000));
    assert_eq!(models[0].prompt_text_token_price_long_context, Some(40_000));
    assert_eq!(
        models[0]
            .capabilities
            .as_ref()
            .expect("capabilities")
            .reasoning_effort,
        ["low", "high", "future"]
    );
    assert!(models[1].prompt_text_token_price.is_none());
    assert!(models[1].capabilities.is_none());
    assert!(models[2].context_length.is_none());
}

/// Invalid successful replies cannot turn malformed metadata into usable
/// models.
#[test]
fn catalog_rejects_invalid_envelopes_identifiers_and_prices() {
    for body in [
        r#"{"object":"model","data":[]}"#,
        r#"{"object":"list","data":[{"id":"","aliases":[]}]}"#,
        r#"{"object":"list","data":[{"id":"bad\nid","aliases":[]}]}"#,
        r#"{"object":"list","data":[{"id":"ok","aliases":["foreign/model"]}]}"#,
        r#"{"object":"list","data":[{"id":"ok","aliases":[],"prompt_text_token_price":-1}]}"#,
        r#"{"object":"list","data":[{"id":"ok","aliases":[],"context_length":1.5}]}"#,
        r#"{"object":"list","data":[{"id":"ok","aliases":[],"capabilities":{}}]}"#,
    ] {
        assert_eq!(parse(body.as_bytes()), Err(Error::InvalidResponse));
    }
}

/// Exercise production HTTP construction against a bounded one-request server.
async fn fetch_reply(status: u16, body: &str) -> (Result<Vec<Model>, Error>, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listen");
    let origin = format!("http://{}", listener.local_addr().expect("address"));
    let policy = tau_provider::OutboundNetworkPolicy::from_environment(Default::default(), None);
    let catalog = Catalog::at_origin(&policy, &origin).expect("catalog");
    let server = async {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).await.expect("header byte");
            request.extend(byte);
            assert!(request.len() < 8192);
        }
        let reply = format!(
            "HTTP/1.1 {status} fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(reply.as_bytes()).await.expect("headers");
        // Rejected responses may close without reading their body.
        let _ = stream.write_all(body.as_bytes()).await;
        String::from_utf8(request).expect("request UTF-8")
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(catalog.fetch("synthetic-token"), server)
    })
    .await
    .expect("bounded exchange")
}

/// Discovery uses only public bearer GET and never impersonates Grok Build.
#[tokio::test]
async fn discovery_uses_public_route_and_content_free_failures() {
    let (result, request) = fetch_reply(200, r#"{"object":"list","data":[]}"#).await;
    assert_eq!(result, Ok(Vec::new()));
    let request = request.to_ascii_lowercase();
    assert!(request.starts_with("get /v1/models http/1.1\r\n"));
    assert!(request.contains("authorization: bearer synthetic-token\r\n"));
    assert!(request.contains(concat!("user-agent: tau/", env!("CARGO_PKG_VERSION"))));
    assert!(!request.contains("grok-build"));
    for (status, error) in [
        (401, Error::Unauthorized),
        (403, Error::Rejected),
        (302, Error::Rejected),
        (503, Error::Rejected),
    ] {
        let (result, _) = fetch_reply(status, "provider-body-secret-canary").await;
        assert_eq!(result, Err(error));
        assert!(!error.to_string().contains("canary"));
    }
    let (result, _) = fetch_reply(200, &"x".repeat(MAX_BODY_BYTES + 1)).await;
    assert_eq!(result, Err(Error::InvalidResponse));
}
