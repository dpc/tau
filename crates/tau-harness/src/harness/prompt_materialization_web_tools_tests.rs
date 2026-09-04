//! Focused logical-web prompt materialization tests.

use super::*;

fn model(hosted: bool) -> tau_proto::ProviderModelInfo {
    tau_proto::ProviderModelInfo {
        id: "chatgpt/test".into(),
        display_name: None,
        tags: Vec::new(),
        supported_tool_types: vec![tau_proto::ToolType::Function],
        hosted_tool_capabilities: hosted
            .then_some(tau_proto::ProviderHostedToolCapability::WebSearch {
                access_modes: vec![
                    tau_proto::ProviderWebSearchAccess::Cached,
                    tau_proto::ProviderWebSearchAccess::Live,
                ],
                supports_allowed_domains: true,
                supports_context_size: true,
            })
            .into_iter()
            .collect(),
        input_modalities: Vec::new(),
        tool_result_modalities: Vec::new(),
        supports_parallel_tool_calls: false,
        default_affinity: 0,
        context_window: tau_proto::TokenCount::new(128_000),
        max_input_tokens: None,
        max_output_tokens: None,
        efforts: vec![tau_proto::Effort::Off],
        verbosities: vec![tau_proto::Verbosity::Medium],
        thinking_summaries: vec![tau_proto::ThinkingSummary::Off],
        supports_compaction: false,
        supports_standalone_compaction: false,
        standalone_compaction_generation_negative: false,
        standalone_compaction_threshold: None,
        standalone_compaction_prefix_budget: None,
        cache_policy: None,
        est_uncached_input_cost_1m_usd: None,
        est_cached_input_cost_1m_usd: None,
        est_cache_write_input_cost_1m_usd: None,
        est_output_cost_1m_usd: None,
        est_cache_storage_cost_1m_token_hour_usd: None,
    }
}

fn spec(name: &str, alias: &str, tags: &[&str]) -> tau_proto::ToolSpec {
    tau_proto::ToolSpec {
        name: tau_proto::ToolName::new(name),
        model_visible_name: Some(tau_proto::ToolName::new(alias)),
        description: None,
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({"type":"object"})),
        format: None,
        tags: tags
            .iter()
            .map(|tag| tau_proto::ToolTag::new(*tag))
            .collect(),
        enabled_by_default: true,
        background_support: None,
        examples: Vec::new(),
    }
}

fn policy(allowed_domains: serde_json::Value, unavailable: &str) -> tau_config::WebToolsPolicy {
    serde_json::from_value(serde_json::json!({
        "allowed_domains": allowed_domains,
        "search": {
            "unavailable": unavailable,
            "candidates": {
                "native": {
                    "enable": true, "priority": 10, "kind": "model_provider",
                    "access": "cached"
                },
                "external": {
                    "enable": true, "priority": 20, "kind": "tool",
                    "tool": "websearch_hybrid_search"
                }
            }
        },
        "fetch": {
            "unavailable": unavailable,
            "candidates": {
                "external": {
                    "enable": true, "priority": 10, "kind": "tool",
                    "tool": "websearch_hybrid_fetch"
                }
            }
        }
    }))
    .expect("valid effective web policy")
}

fn ordinary_specs(with_search_enforcement: bool) -> Vec<tau_proto::ToolSpec> {
    let mut search_tags = vec![tau_proto::WEB_SEARCH_TOOL_TAG];
    if with_search_enforcement {
        search_tags.push(tau_proto::WEB_PROVIDER_FILTER_DOMAIN_ENFORCEMENT_TAG);
    }
    vec![
        spec("websearch_hybrid_search", "web_search", &search_tags),
        spec(
            "websearch_hybrid_fetch",
            "web_fetch",
            &[
                tau_proto::WEB_FETCH_TOOL_TAG,
                tau_proto::WEB_REQUESTED_TARGET_DOMAIN_ENFORCEMENT_TAG,
            ],
        ),
    ]
}

/// A capable exact route wins native search while preserving ordinary fetch;
/// a route without the capability selects both ordinary fallbacks.
#[test]
fn exact_route_native_search_suppresses_only_external_search() {
    let specs = ordinary_specs(false);
    let compiled = compile_web_tools(
        &policy(serde_json::Value::Null, "omit"),
        &model(true),
        &specs,
    )
    .expect("compile web tools");
    assert_eq!(compiled.hosted_tools.len(), 1);
    assert!(
        !compiled
            .retained_tools
            .contains(&tau_proto::ToolName::new("websearch_hybrid_search"))
    );
    assert!(
        compiled
            .retained_tools
            .contains(&tau_proto::ToolName::new("websearch_hybrid_fetch"))
    );

    let external = compile_web_tools(
        &policy(serde_json::Value::Null, "omit"),
        &model(false),
        &specs,
    )
    .expect("compile external fallback");
    assert!(external.hosted_tools.is_empty());
    assert_eq!(external.retained_tools.len(), 2);
}

/// Domain policy requires declared enforcement and freezes the exact hidden
/// policy on an eligible ordinary invocation.
#[test]
fn domain_policy_requires_declared_enforcement_and_freezes_fetch_policy() {
    let domains = serde_json::json!(["example.com"]);
    let omitted = compile_web_tools(
        &policy(domains.clone(), "omit"),
        &model(false),
        &ordinary_specs(false),
    )
    .expect("omit unenforced search");
    assert!(
        !omitted
            .retained_tools
            .contains(&tau_proto::ToolName::new("websearch_hybrid_search"))
    );
    assert_eq!(
        omitted.invocation_policies[&tau_proto::ToolName::new("websearch_hybrid_fetch")]
            .allowed_web_domains,
        Some(vec!["example.com".to_owned()])
    );

    let enforced = compile_web_tools(
        &policy(domains, "error"),
        &model(false),
        &ordinary_specs(true),
    )
    .expect("eligible enforced ordinary tools");
    assert_eq!(enforced.retained_tools.len(), 2);
}

/// An explicit deny-all allowlist obeys `unavailable: error` before provider
/// dispatch even when the exact route supports hosted search.
#[test]
fn empty_domain_allowlist_and_unavailable_error_fail_before_dispatch() {
    let error = compile_web_tools(
        &policy(serde_json::json!([]), "error"),
        &model(true),
        &ordinary_specs(true),
    );
    let Err(error) = error else {
        panic!("deny-all must have no eligible implementation");
    };
    assert!(error.contains("logical web search is unavailable"));
}

fn search_policy(candidates: serde_json::Value) -> tau_config::WebToolsPolicy {
    serde_json::from_value(serde_json::json!({
        "search": {"unavailable":"error", "candidates": candidates},
        "fetch": {"candidates": {"disabled": {"enable": false}}}
    }))
    .expect("valid test policy")
}

/// Equal priorities select by candidate name, while a disabled earlier name
/// cannot shadow the next eligible implementation.
#[test]
fn priority_ties_use_names_and_disabled_candidates_are_skipped() {
    let specs = vec![
        spec(
            "alpha_tool",
            "web_search",
            &[tau_proto::WEB_SEARCH_TOOL_TAG],
        ),
        spec("zeta_tool", "web_search", &[tau_proto::WEB_SEARCH_TOOL_TAG]),
    ];
    let tied = search_policy(serde_json::json!({
        "zeta": {"priority":10, "kind":"tool", "tool":"zeta_tool"},
        "alpha": {"priority":10, "kind":"tool", "tool":"alpha_tool"}
    }));
    let compiled = compile_web_tools(&tied, &model(false), &specs).expect("tie winner");
    assert_eq!(
        compiled.retained_tools,
        HashSet::from([tau_proto::ToolName::new("alpha_tool")])
    );

    let disabled = search_policy(serde_json::json!({
        "alpha": {"enable":false, "priority":1, "kind":"tool", "tool":"alpha_tool"},
        "zeta": {"priority":2, "kind":"tool", "tool":"zeta_tool"}
    }));
    let compiled = compile_web_tools(&disabled, &model(false), &specs).expect("disabled skipped");
    assert_eq!(
        compiled.retained_tools,
        HashSet::from([tau_proto::ToolName::new("zeta_tool")])
    );
}

/// Hosted access/context controls and every ordinary metadata discriminator
/// fail closed, allowing only the next fully eligible candidate.
#[test]
fn unsupported_native_controls_and_wrong_tool_metadata_fall_through() {
    let external = spec("external", "web_search", &[tau_proto::WEB_SEARCH_TOOL_TAG]);
    let live = search_policy(serde_json::json!({
        "native": {"priority":1, "kind":"model_provider", "access":"live"},
        "external": {"priority":2, "kind":"tool", "tool":"external"}
    }));
    let mut cached_only = model(true);
    cached_only.hosted_tool_capabilities =
        vec![tau_proto::ProviderHostedToolCapability::WebSearch {
            access_modes: vec![tau_proto::ProviderWebSearchAccess::Cached],
            supports_allowed_domains: true,
            supports_context_size: false,
        }];
    assert_eq!(
        compile_web_tools(&live, &cached_only, std::slice::from_ref(&external))
            .expect("fallback")
            .retained_tools,
        HashSet::from([tau_proto::ToolName::new("external")])
    );
    let context = search_policy(serde_json::json!({
        "native": {
            "priority":1, "kind":"model_provider", "access":"cached",
            "context_size":"high"
        },
        "external": {"priority":2, "kind":"tool", "tool":"external"}
    }));
    assert_eq!(
        compile_web_tools(&context, &cached_only, std::slice::from_ref(&external))
            .expect("unsupported context fallback")
            .retained_tools,
        HashSet::from([tau_proto::ToolName::new("external")])
    );

    let policy = search_policy(serde_json::json!({
        "wrong_type": {"priority":1, "kind":"tool", "tool":"wrong_type"},
        "wrong_alias": {"priority":2, "kind":"tool", "tool":"wrong_alias"},
        "wrong_operation": {"priority":3, "kind":"tool", "tool":"wrong_operation"},
        "good": {"priority":4, "kind":"tool", "tool":"external"}
    }));
    let mut wrong_type = spec(
        "wrong_type",
        "web_search",
        &[tau_proto::WEB_SEARCH_TOOL_TAG],
    );
    wrong_type.tool_type = tau_proto::ToolType::Custom;
    let wrong_alias = spec("wrong_alias", "other", &[tau_proto::WEB_SEARCH_TOOL_TAG]);
    let wrong_operation = spec(
        "wrong_operation",
        "web_search",
        &[tau_proto::WEB_FETCH_TOOL_TAG],
    );
    let compiled = compile_web_tools(
        &policy,
        &model(false),
        &[wrong_type, wrong_alias, wrong_operation, external],
    )
    .expect("next eligible");
    assert_eq!(
        compiled.retained_tools,
        HashSet::from([tau_proto::ToolName::new("external")])
    );
}

/// Native suppression removes only declared candidates; an unrelated
/// ordinary `web_search` alias still collides, while side-query suppression
/// removes both declared ordinary web implementations.
#[test]
fn unrelated_hosted_alias_collides_and_side_query_suppresses_declared_web() {
    let policy = policy(serde_json::Value::Null, "omit");
    let mut specs = ordinary_specs(false);
    specs.push(spec("unrelated", "web_search", &[]));
    let compiled = compile_web_tools(&policy, &model(true), &specs).expect("native");
    let declared = policy.declared_tool_names().collect::<HashSet<_>>();
    specs.retain(|spec| {
        !declared.contains(&spec.name) || compiled.retained_tools.contains(&spec.name)
    });
    assert!(hosted_web_search_collides(&compiled.hosted_tools, &specs));

    let mut side_specs = ordinary_specs(false);
    side_specs.push(spec("unrelated", "other", &[]));
    suppress_declared_web_candidates(&policy, &mut side_specs);
    assert_eq!(
        side_specs
            .iter()
            .map(|spec| spec.name.as_str())
            .collect::<Vec<_>>(),
        ["unrelated"]
    );
}
