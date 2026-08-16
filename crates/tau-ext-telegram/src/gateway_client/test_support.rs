//! Shared fixtures for gateway-client tests.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use super::*;

impl GatewayClientConfig {
    /// Build deterministic authentication configuration for socket fixtures.
    pub(crate) fn for_test(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            auth_key: GatewayAuthKey::parse(&"11".repeat(32)).expect("test key"),
            client_generation: ClientGeneration::parse("22".repeat(32)).expect("test generation"),
        }
    }
}

/// Complete the server half of the deterministic authenticated protocol test
/// handshake.
pub(crate) fn authenticate_test_gateway(
    stream: &mut UnixStream,
    gateway_generation: &str,
) -> serde_json::Value {
    let mut reader = BufReader::new(stream.try_clone().expect("clone test gateway stream"));
    let mut hello_line = String::new();
    reader
        .read_line(&mut hello_line)
        .expect("read test gateway hello");
    let hello: serde_json::Value =
        serde_json::from_str(&hello_line).expect("test gateway hello JSON");
    finish_test_gateway_authentication(stream, gateway_generation, &hello);
    hello
}

/// Finish a deterministic server handshake after a fixture has inspected hello.
pub(crate) fn finish_test_gateway_authentication(
    stream: &mut UnixStream,
    gateway_generation: &str,
    hello: &serde_json::Value,
) {
    let key = GatewayAuthKey::parse(&"11".repeat(32)).expect("test gateway key");
    let key_id = GatewayKeyId::parse(hello["key_id"].as_str().expect("hello key id").to_owned())
        .expect("valid hello key id");
    let client_generation = ClientGeneration::parse(
        hello["client_generation"]
            .as_str()
            .expect("hello client generation")
            .to_owned(),
    )
    .expect("valid client generation");
    let client_nonce = ClientNonce::parse(
        hello["client_nonce"]
            .as_str()
            .expect("hello client nonce")
            .to_owned(),
    )
    .expect("valid client nonce");
    assert_eq!(hello["protocol_version"], SOCKET_PROTOCOL_VERSION);
    assert_eq!(hello["kind"], "hello");
    assert_eq!(hello["extension_instance"], EXTENSION_INSTANCE);
    assert_eq!(key_id, key.key_id());
    let gateway_generation =
        GatewayGeneration::parse(gateway_generation.to_owned()).expect("valid gateway generation");
    let server_nonce = ServerNonce::parse("33".repeat(32)).expect("valid server nonce");
    let fields = AuthFields {
        key_id: &key_id,
        gateway_generation: &gateway_generation,
        client_generation: &client_generation,
        client_nonce: &client_nonce,
        server_nonce: &server_nonce,
    };
    writeln!(
        stream,
        "{}",
        serde_json::json!({
            "protocol_version": SOCKET_PROTOCOL_VERSION,
            "ok": true,
            "kind": "challenge",
            "gateway_generation": gateway_generation.as_str(),
            "server_nonce": server_nonce.as_str(),
            "server_mac": key.server_proof(&fields).encode(),
        })
    )
    .expect("write test gateway challenge");
    stream.flush().expect("flush test gateway challenge");
    let mut reader = BufReader::new(stream.try_clone().expect("clone test gateway stream"));
    let mut authenticate_line = String::new();
    reader
        .read_line(&mut authenticate_line)
        .expect("read test gateway authenticate");
    let authenticate: serde_json::Value =
        serde_json::from_str(&authenticate_line).expect("test gateway authenticate JSON");
    assert_eq!(authenticate["protocol_version"], SOCKET_PROTOCOL_VERSION);
    assert_eq!(authenticate["kind"], "authenticate");
    let client_proof = AuthProof::parse(authenticate["client_mac"].as_str().unwrap_or_default())
        .expect("valid client proof");
    assert!(key.verify_proof(&key.client_proof(&fields), &client_proof));
    writeln!(
        stream,
        "{}",
        serde_json::json!({
            "protocol_version": SOCKET_PROTOCOL_VERSION,
            "ok": true,
            "kind": "authenticated",
            "gateway_generation": gateway_generation.as_str(),
            "heartbeat_interval_seconds": 10,
            "registration_lease_seconds": 30,
            "reannounce_required": true,
            "deliveries": [],
        })
    )
    .expect("write test gateway authenticated response");
    stream
        .flush()
        .expect("flush test gateway authenticated response");
}
