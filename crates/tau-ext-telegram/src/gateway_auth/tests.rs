use super::*;

fn fields<'a>(
    key_id: &'a GatewayKeyId,
    gateway: &'a GatewayGeneration,
    client: &'a ClientGeneration,
    client_nonce: &'a ClientNonce,
    server_nonce: &'a ServerNonce,
) -> AuthFields<'a> {
    AuthFields {
        key_id,
        gateway_generation: gateway,
        client_generation: client,
        client_nonce,
        server_nonce,
    }
}

/// Secret parsing rejects noncanonical encodings, and diagnostics never reveal
/// decoded key material.
#[test]
fn keys_are_strict_and_debug_is_redacted() {
    let key = GatewayAuthKey::parse(&"01".repeat(32)).expect("canonical test key");
    assert_eq!(key.key_id().as_str().len(), 32);
    assert_eq!(format!("{key:?}"), "GatewayAuthKey(<redacted>)");
    assert!(GatewayAuthKey::parse(&"AB".repeat(32)).is_err());
    assert!(GatewayAuthKey::parse("short").is_err());
}

/// Client and server proofs are not interchangeable, and malformed proof
/// encodings never reach constant-time comparison.
#[test]
fn proofs_are_role_separated_and_verify_exactly() {
    let key = GatewayAuthKey::parse(&"23".repeat(32)).expect("canonical test key");
    let key_id = key.key_id();
    let gateway = GatewayGeneration::parse("34".repeat(32)).expect("valid test value");
    let client = ClientGeneration::parse("45".repeat(32)).expect("valid test value");
    let client_nonce = ClientNonce::parse("56".repeat(32)).expect("valid test value");
    let server_nonce = ServerNonce::parse("67".repeat(32)).expect("valid test value");
    let fields = fields(&key_id, &gateway, &client, &client_nonce, &server_nonce);
    let server = key.server_proof(&fields);
    let client = key.client_proof(&fields);
    assert!(!key.verify_proof(&server, &client));
    assert!(key.verify_proof(&server, &server));
    assert!(AuthProof::parse("bad").is_err());
}
