//! Deterministic OpenRouter discovery and cache acceptance.

use std::collections as path_std_collections;

mod scripted_http_server;

use scripted_http_server::ScriptedHttpServer;

use super::*;

/// One valid bounded OpenRouter response used by cache tests.
const MODELS: &str = r#"{"data":[{"id":"vendor/model","name":"Fixture","context_length":1234,"supported_parameters":["reasoning"]}]}"#;

/// Builds a direct-only provider policy without ambient discovery.
fn network() -> tau_provider::OutboundNetworkPolicy {
    tau_provider::OutboundNetworkPolicy::from_environment(
        path_std_collections::BTreeMap::new(),
        None,
    )
}

/// Ensures successful discovery sends bearer auth, normalizes models, and
/// refreshes usable cache data.
#[test]
fn authenticated_discovery_normalizes_models_and_refreshes_cache() {
    let server = ScriptedHttpServer::spawn(200, MODELS);
    let directory = tempfile::tempdir().expect("cache directory");
    let cache = directory.path().join("models.json");
    let models = fetch_openrouter_models_from(
        "openrouter-key-canary",
        &network(),
        &server.url(),
        Some(&cache),
    )
    .expect("discovery");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id.as_str(), "vendor/model");
    assert_eq!(models[0].context_window, 1234);
    assert!(models[0].compat.as_ref().expect("compat").reasoning_effort);
    let request = server.finish();
    assert!(request.starts_with("GET /models HTTP/1.1\r\n"));
    assert!(request.contains("authorization: Bearer openrouter-key-canary\r\n"));
    let cached: Vec<ChatCompletionsModel> =
        serde_json::from_reader(fs::File::open(cache).expect("cache file")).expect("cache JSON");
    assert_eq!(cached.len(), 1);
}

/// Ensures only transport/status failures may use a prior non-empty cache;
/// successful malformed data fails closed and leaves the old cache unchanged.
#[test]
fn cache_fallback_is_limited_to_transport_or_status_failure() {
    let directory = tempfile::tempdir().expect("cache directory");
    let cache = directory.path().join("models.json");
    let seed = ScriptedHttpServer::spawn(200, MODELS);
    fetch_openrouter_models_from("", &network(), &seed.url(), Some(&cache)).expect("seed cache");
    seed.finish();
    let original = fs::read(&cache).expect("seed bytes");

    let status = ScriptedHttpServer::spawn(503, "{}");
    let cached = fetch_openrouter_models_from("", &network(), &status.url(), Some(&cache))
        .expect("status cache fallback");
    assert_eq!(cached.len(), 1);
    status.finish();

    let malformed = ScriptedHttpServer::spawn(200, r#"{"data":"not-an-array"}"#);
    let error = fetch_openrouter_models_from(
        "secret-key-canary",
        &network(),
        &malformed.url(),
        Some(&cache),
    )
    .expect_err("successful malformed response must not use cache");
    malformed.finish();
    let projection = format!("{error:?} {error}");
    assert!(!projection.contains("secret-key-canary"));
    assert_eq!(fs::read(cache).expect("unchanged cache"), original);
}

/// Ensures a transport failure without an eligible cache remains a redacted
/// typed failure rather than consulting the public OpenRouter endpoint.
#[test]
fn transport_failure_without_cache_is_redacted_and_offline() {
    let server = ScriptedHttpServer::spawn_truncated_success();
    let address = server.address();
    let directory = tempfile::tempdir().expect("cache directory");
    let error = fetch_openrouter_models_from(
        "transport-key-canary",
        &network(),
        &server.url(),
        Some(&directory.path().join("missing.json")),
    )
    .expect_err("transport failure");
    server.finish();
    let projection = format!("{error:?} {error}");
    assert!(!projection.contains("transport-key-canary"));
    assert!(!projection.contains(&address.to_string()));
}

/// Ensures body transport may use valid cache, while invalid configuration,
/// empty/invalid/oversized successes, and malformed cache all fail closed
/// without replacing last-good bytes.
#[test]
fn cache_policy_preserves_last_good_data_across_failure_classes() {
    let directory = tempfile::tempdir().expect("cache directory");
    let cache = directory.path().join("models.json");
    let seed = ScriptedHttpServer::spawn(200, MODELS);
    fetch_openrouter_models_from("", &network(), &seed.url(), Some(&cache)).expect("seed cache");
    seed.finish();
    let original = fs::read(&cache).expect("seed bytes");

    let truncated = ScriptedHttpServer::spawn_truncated_success();
    let cached = fetch_openrouter_models_from("", &network(), &truncated.url(), Some(&cache))
        .expect("body transport cache fallback");
    assert_eq!(cached.len(), 1);
    truncated.finish();

    for body in [
        r#"{"data":[]}"#.to_owned(),
        r#"{"data":[{"id":""}]}"#.to_owned(),
        format!(
            r#"{{"data":[{{"id":"{}"}}]}}"#,
            "x".repeat(MAX_OPENROUTER_MODELS_BODY_BYTES + 1)
        ),
    ] {
        let invalid = ScriptedHttpServer::spawn(200, body);
        fetch_openrouter_models_from("", &network(), &invalid.url(), Some(&cache))
            .expect_err("ineligible successful response");
        invalid.finish();
        assert_eq!(fs::read(&cache).expect("preserved cache"), original);
    }

    let invalid_network = tau_provider::OutboundNetworkPolicy::from_environment(
        path_std_collections::BTreeMap::from([(
            "https_proxy".to_owned(),
            "socks5://unsupported.invalid".to_owned(),
        )]),
        None,
    );
    fetch_openrouter_models_from(
        "",
        &invalid_network,
        "https://unused.invalid/models",
        Some(&cache),
    )
    .expect_err("invalid startup configuration cannot use cache");
    assert_eq!(fs::read(&cache).expect("preserved cache"), original);

    fs::write(&cache, b"{malformed").expect("malformed cache");
    let status = ScriptedHttpServer::spawn(503, "{}");
    fetch_openrouter_models_from("", &network(), &status.url(), Some(&cache))
        .expect_err("malformed cache is ineligible");
    status.finish();
}
