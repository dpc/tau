//! Deterministic OpenRouter discovery and cache acceptance.

use std::collections as path_std_collections;

mod scripted_http_server;

use scripted_http_server::ScriptedHttpServer;

use super::super::{CacheUsageCompat, models_for_provider};
use super::*;

/// One valid bounded OpenRouter response used by cache tests.
const MODELS: &str = r#"{"data":[{"id":"vendor/model","name":"Fixture","context_length":1234,"supported_parameters":["reasoning"]}]}"#;

/// Exact OpenRouter parameter memberships must remain independent, including
/// conservative handling of absent, null, and empty metadata.
#[test]
fn discovery_maps_exact_tool_parameter_capabilities() {
    let server = ScriptedHttpServer::spawn(
        200,
        r#"{"data":[
            {"id":"vendor/all","context_length":1,"supported_parameters":["tools","tool_choice","parallel_tool_calls"]},
            {"id":"vendor/auto-only","context_length":2,"supported_parameters":["tools"]},
            {"id":"vendor/controls-without-tools","context_length":3,"supported_parameters":["tool_choice","parallel_tool_calls"]},
            {"id":"vendor/empty","context_length":4,"supported_parameters":[]},
            {"id":"vendor/null","context_length":5,"supported_parameters":null},
            {"id":"vendor/missing","context_length":6},
            {"id":"vendor/near-match","context_length":7,"supported_parameters":["Tools","tools ","tool-choice","parallel_tool_call"]}
        ]}"#,
    );

    let models =
        fetch_openrouter_models_from("", &network(), &server.url(), None).expect("discovery");
    let capabilities = models
        .iter()
        .map(|model| {
            let compat = model.compat.expect("discovered compatibility");
            (
                model.id.as_str(),
                model.supported_tool_types.as_slice(),
                compat.tool_choice,
                compat.parallel_tool_calls,
                model.supports_parallel_tool_calls,
            )
        })
        .collect::<Vec<_>>();

    assert_eq!(
        capabilities,
        vec![
            (
                "vendor/all",
                &[tau_proto::ToolType::Function][..],
                true,
                true,
                true
            ),
            (
                "vendor/auto-only",
                &[tau_proto::ToolType::Function][..],
                false,
                false,
                false
            ),
            ("vendor/controls-without-tools", &[][..], true, false, false),
            ("vendor/empty", &[][..], false, false, false),
            ("vendor/null", &[][..], false, false, false),
            ("vendor/missing", &[][..], false, false, false),
            ("vendor/near-match", &[][..], false, false, false),
        ]
    );
    server.finish();
}

/// Conflicting duplicate discovery rows cannot split publication from backend
/// lookup by assigning different exact capabilities to one model id.
#[test]
fn discovery_rejects_duplicate_model_ids() {
    let server = ScriptedHttpServer::spawn(
        200,
        r#"{"data":[
            {"id":"vendor/duplicate","context_length":1,"supported_parameters":["tools"]},
            {"id":"vendor/duplicate","context_length":1,"supported_parameters":[]}
        ]}"#,
    );

    fetch_openrouter_models_from("", &network(), &server.url(), None)
        .expect_err("duplicate discovery rows must fail closed");

    server.finish();
}

/// Explicit OpenRouter profiles use the same exact Function-only and identity
/// invariants as fresh discovery and cached discovery.
#[test]
fn explicit_profiles_reject_invalid_tool_capabilities_and_duplicates() {
    let model = |id, supported_tool_types, supports_parallel_tool_calls| {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "supported_tool_types": supported_tool_types,
            "supports_parallel_tool_calls": supports_parallel_tool_calls
        }))
        .expect("model")
    };
    let invalid_tools = OpenRouterProfile {
        api_key: String::new(),
        models: vec![model("vendor/custom", serde_json::json!(["custom"]), false)],
    };
    assert!(invalid_tools.validate().is_err());
    let duplicates = OpenRouterProfile {
        api_key: String::new(),
        models: vec![
            model("vendor/duplicate", serde_json::json!(["function"]), true),
            model("vendor/duplicate", serde_json::json!([]), false),
        ],
    };
    assert!(duplicates.validate().is_err());
}

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
    let compat = models[0].compat.as_ref().expect("compat");
    assert!(compat.reasoning_effort.is_some());
    assert!(compat.stream_options);
    assert_eq!(compat.cache_usage, CacheUsageCompat::OpenAi);
    assert!(compat.openai_prompt_cache.is_none());
    assert!(models[0].cache_contract.is_none());
    let request = server.finish();
    assert!(request.starts_with("GET /models HTTP/1.1\r\n"));
    assert!(request.contains("authorization: Bearer openrouter-key-canary\r\n"));
    let cached: CachedOpenRouterModels =
        serde_json::from_reader(fs::File::open(cache).expect("cache file")).expect("cache JSON");
    assert_eq!(cached.version, OPENROUTER_MODELS_CACHE_VERSION);
    assert_eq!(cached.models.len(), 1);
}

/// Ensures discovery keeps known root context lengths while dropping entries
/// with null or absent root lengths, without borrowing provider-specific data.
#[test]
fn discovery_filters_models_without_root_context_length() {
    let server = ScriptedHttpServer::spawn(
        200,
        r#"{"data":[
            {"id":"vendor/valid","context_length":1234},
            {"id":"vendor/null","context_length":null,"top_provider":{"context_length":9999}},
            {"id":"vendor/missing","top_provider":{"context_length":8888}},
            {"id":"vendor/zero","context_length":0}
        ]}"#,
    );

    let models =
        fetch_openrouter_models_from("", &network(), &server.url(), None).expect("discovery");

    assert_eq!(
        models
            .iter()
            .map(|model| (model.id.as_str(), model.context_window))
            .collect::<Vec<_>>(),
        vec![("vendor/valid", 1234), ("vendor/zero", 0)]
    );
    server.finish();
}

/// OpenRouter routes may emit documented cache counters, but provider selection
/// prevents an asserted upstream cache lifecycle from becoming model metadata.
#[test]
fn openrouter_conversion_strips_upstream_cache_contract() {
    let model: ChatCompletionsModel = serde_json::from_value(serde_json::json!({
        "id": "vendor/model",
        "compat": {
            "stream_options": true,
            "parallel_tool_calls": true,
            "openai_prompt_cache": {
                "key": "agent",
                "retention": "in_memory"
            },
            "cache_usage": "deep_seek"
        },
        "cache_contract": {
            "kind": "automatic_prefix",
            "ttl": {"kind": "unknown"},
            "renewal": "unsupported",
            "output_floor": "unknown",
            "quota": {
                "requests": "unknown",
                "read_tokens": "unknown",
                "write_tokens": "unknown",
                "output_tokens": "unknown"
            },
            "privacy": {
                "storage": "unknown",
                "zero_data_retention": "unknown",
                "data_residency": "unknown",
                "manual_deletion": "unavailable"
            }
        }
    }))
    .expect("valid generic contract");
    let provider = OpenRouterProfile {
        api_key: String::new(),
        models: vec![model],
    }
    .to_chat_completions();

    assert!(provider.compat.stream_options);
    assert!(
        provider.extra_body.is_empty(),
        "OpenRouter must not force provider.require_parameters"
    );
    assert_eq!(provider.compat.cache_usage, CacheUsageCompat::OpenAi);
    assert_eq!(provider.models[0].cache_contract, None);
    let compat = provider.models[0]
        .compat
        .expect("configured model compatibility");
    assert!(compat.stream_options);
    assert_eq!(compat.cache_usage, CacheUsageCompat::OpenAi);
    assert!(compat.openai_prompt_cache.is_none());
    assert!(compat.parallel_tool_calls);
    assert!(compat.reasoning_effort.is_none());
    assert_eq!(
        models_for_provider(&tau_proto::ProviderName::new("router"), &provider)[0].cache_policy,
        None
    );
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

/// Pre-capability cache rows omit exact discovery evidence and must never
/// regain legacy Function support during offline fallback.
#[test]
fn unversioned_cache_is_ineligible_for_offline_fallback() {
    let directory = tempfile::tempdir().expect("cache directory");
    let cache = directory.path().join("models.json");
    fs::write(&cache, r#"[{"id":"vendor/legacy","context_window":1234}]"#).expect("legacy cache");
    let status = ScriptedHttpServer::spawn(503, "{}");

    fetch_openrouter_models_from("", &network(), &status.url(), Some(&cache))
        .expect_err("unversioned cache must fail closed");

    status.finish();
}

/// A current cache wrapper does not authorize impossible Chat Completions tool
/// combinations; semantic profile validation still applies during fallback.
#[test]
fn versioned_cache_rejects_incoherent_tool_capabilities() {
    for model in [
        serde_json::json!({
            "id": "vendor/custom",
            "supported_tool_types": ["custom"],
            "supports_parallel_tool_calls": false
        }),
        serde_json::json!({
            "id": "vendor/parallel-without-tools",
            "supported_tool_types": [],
            "supports_parallel_tool_calls": true
        }),
    ] {
        let directory = tempfile::tempdir().expect("cache directory");
        let cache = directory.path().join("models.json");
        fs::write(
            &cache,
            serde_json::to_vec(&serde_json::json!({
                "version": OPENROUTER_MODELS_CACHE_VERSION,
                "models": [model]
            }))
            .expect("cache JSON"),
        )
        .expect("cache");
        let status = ScriptedHttpServer::spawn(503, "{}");

        fetch_openrouter_models_from("", &network(), &status.url(), Some(&cache))
            .expect_err("incoherent cache must fail closed");

        status.finish();
    }
}
