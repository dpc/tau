//! Provider switching is tested at dispatch without live upstream requests.

use std::collections::BTreeMap;

use tau_config::settings::BuiltinComponentIdentity;
use tau_proto::ModelId;

use super::*;

/// Publish synthetic private aliases on the quiet provider, attaching the same
/// configured-component and frozen-profile facts as real supervised startup.
fn install_private_aliases(h: &mut Harness) -> (ModelId, ModelId) {
    let original: ModelId = "test/model".into();
    let connection = h.provider_runtime.model_routes[&original].clone();
    let source: ModelId = "test/gpt-5.6-luna".into();
    let destination: ModelId = "foreign/gpt-5.6-luna".into();
    let mut info = h.provider_runtime.model_info[&original].clone();
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    for model in [&source, &destination] {
        info.id = model.clone();
        h.provider_runtime
            .model_info
            .insert(model.clone(), info.clone());
        h.provider_runtime
            .model_routes
            .insert(model.clone(), connection.clone());
        h.provider_runtime.available_models.push(model.clone());
        h.provider_runtime
            .models_by_extension
            .entry(connection.clone())
            .or_default()
            .push(info.clone());
    }
    let mut config = crate::settings::default_config()
        .extensions
        .into_values()
        .find(|config| config.component == Some(BuiltinComponentIdentity::Provider))
        .expect("built-in provider config");
    let extension = h.extensions.entries.get_mut(&connection).expect("provider");
    config.name = extension.name.to_string();
    extension.supervised_config = Some(config);
    h.config.provider_settings_snapshots.insert(
        extension.name.to_string(),
        BTreeMap::from([
            (
                "test.json".to_owned(),
                br#"{"kind":"chatgpt","credential":{"identity":"account-a","slot":"oauth"}}"#
                    .to_vec(),
            ),
            (
                "foreign.json".to_owned(),
                br#"{"kind":"chatgpt","credential":{"identity":"account-b","slot":"oauth"}}"#
                    .to_vec(),
            ),
        ]),
    );
    (source, destination)
}

/// A name/tag cannot grant private replay authority; only matching published
/// routes in one known component with matching frozen wire settings qualify.
#[test]
fn provider_switch_private_alias_compatibility_requires_frozen_route_authority() {
    for invalid in [
        "none",
        "both-lite",
        "external",
        "other-owner",
        "ambiguous-source",
        "ambiguous-destination",
        "missing-source",
        "removed-source",
        "public-source",
        "public-destination",
        "lite-source",
        "lite-destination",
        "other-model",
    ] {
        let td = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
        let (source, destination) = install_private_aliases(&mut h);
        let connection = h.provider_runtime.model_routes[&destination].clone();
        let extension = h.extensions.entries.get_mut(&connection).expect("provider");
        let settings = h
            .config
            .provider_settings_snapshots
            .get_mut(extension.name.as_str())
            .expect("settings");
        match invalid {
            "external" => {
                extension
                    .supervised_config
                    .as_mut()
                    .expect("config")
                    .component = None
            }
            "other-owner" => {
                h.provider_runtime
                    .model_routes
                    .insert(source.clone(), crate::test_connection_id("other"));
            }
            "ambiguous-source" | "ambiguous-destination" => {
                let model = if invalid == "ambiguous-source" {
                    &source
                } else {
                    &destination
                };
                h.provider_runtime.models_by_extension.insert(
                    crate::test_connection_id("duplicate-publisher"),
                    vec![h.provider_runtime.model_info[model].clone()],
                );
            }
            "missing-source" => {
                settings.remove("test.json");
            }
            "removed-source" => {
                h.provider_runtime.model_routes.remove(&source);
            }
            "public-source" => {
                settings.insert(
                    "test.json".to_owned(),
                    br#"{"kind":"responses","tags":["shell:chatgpt"]}"#.to_vec(),
                );
            }
            "public-destination" => {
                settings.insert(
                    "foreign.json".to_owned(),
                    br#"{"kind":"responses","tags":["shell:chatgpt"]}"#.to_vec(),
                );
            }
            "lite-source" => {
                settings.insert(
                    "test.json".to_owned(),
                    br#"{"kind":"chatgpt","responses_lite_compatibility":true}"#.to_vec(),
                );
            }
            "lite-destination" => {
                settings.insert(
                    "foreign.json".to_owned(),
                    br#"{"kind":"chatgpt","responses_lite_compatibility":true}"#.to_vec(),
                );
            }
            "other-model" => {
                let owner = h
                    .provider_runtime
                    .model_routes
                    .remove(&source)
                    .expect("source");
                h.provider_runtime
                    .model_routes
                    .insert("test/gpt-5.6-sol".into(), owner);
            }
            "none" => {}
            "both-lite" => {
                for name in ["test.json", "foreign.json"] {
                    settings.insert(
                        name.to_owned(),
                        br#"{"kind":"chatgpt","responses_lite_compatibility":true}"#.to_vec(),
                    );
                }
            }
            _ => unreachable!(),
        }
        assert_eq!(
            h.compatible_provider_replay_sources(&destination)
                .contains(&source),
            matches!(invalid, "none" | "both-lite"),
            "{invalid}",
        );
        h.shutdown().expect("shutdown");
    }
}

/// The actual selection/dispatch path accepts account-only continuation without
/// changing opaque bytes; a destination terminal rejection keeps that history
/// available for switching back, while incompatible selection preserves the
/// model.
#[test]
fn provider_switch_private_alias_dispatch_and_rejection_preserve_opaque_history() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let (source, destination) = install_private_aliases(&mut h);
    let cid = ensure_test_user_agent(&mut h);
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .identity
        .model_override = Some(source.clone());
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(&agent_id),
    );
    let prompt = read_nth_prompt_created(&h, 0);
    let raw =
        r#"{ "z":1e+02, "encrypted_content":"synthetic\u0020prefix", "type" : "compaction" }"#;
    let replacement = ContextItem::Compaction(
        tau_proto::OpaqueProviderItem::from_raw_json(raw).expect("opaque replacement"),
    );
    let mut response = provider_text_response(&prompt.agent_prompt_id, prompt.agent_id, "unused");
    response.output_items = vec![replacement.clone()];
    h.handle_provider_response_finished(response)
        .expect("compact");
    let requests = request_count(&h);

    // The published unrelated model is rejected before changing the override.
    let incompatible: ModelId = "foreign/model".into();
    let mut info = h.provider_runtime.model_info[&destination].clone();
    info.id = incompatible.clone();
    h.provider_runtime
        .model_info
        .insert(incompatible.clone(), info);
    h.provider_runtime.model_routes.insert(
        incompatible.clone(),
        h.provider_runtime.model_routes[&destination].clone(),
    );
    h.provider_runtime
        .available_models
        .push(incompatible.clone());
    let clears_before_rejection = h.provider_runtime.cache_refresh_clear_count;
    h.handle_ui_agent_model_select(
        crate::harness::harness_connection_id(),
        tau_proto::UiAgentModelSelect {
            session_id: test_session_id("s1"),
            target_agent_id: Some(agent_id.clone()),
            model: incompatible,
        },
    )
    .expect("incompatible selection handled");
    assert_eq!(
        h.provider_runtime.cache_refresh_clear_count,
        clears_before_rejection
    );
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .model_override,
        Some(source.clone())
    );
    for model in [destination.clone(), source.clone()] {
        let clears_before_selection = h.provider_runtime.cache_refresh_clear_count;
        h.handle_ui_agent_model_select(
            crate::harness::harness_connection_id(),
            tau_proto::UiAgentModelSelect {
                session_id: test_session_id("s1"),
                target_agent_id: Some(agent_id.clone()),
                model: model.clone(),
            },
        )
        .expect("select account");
        assert_eq!(
            h.provider_runtime.cache_refresh_clear_count,
            clears_before_selection + 1
        );
        assert_eq!(
            h.agent_runtime.agent_registry.agents[&cid]
                .identity
                .model_override,
            Some(model.clone())
        );
        let index = request_count(&h);
        h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("continue".to_owned()))
            .expect("dispatch");
        let prompt = read_nth_prompt_created(&h, index);
        assert_eq!(prompt.model, model);
        assert!(prompt.context.flatten().contains(&replacement));
        let mut rejected =
            provider_text_response(&prompt.agent_prompt_id, prompt.agent_id, "unused");
        rejected.output_items.clear();
        rejected.stop_reason = tau_proto::ProviderStopReason::Error;
        h.handle_provider_response_finished(rejected)
            .expect("remote rejection");
        assert_eq!(
            request_count(&h),
            index + 1,
            "no stripping or automatic resend"
        );
    }
    assert_eq!(request_count(&h), requests + 2);
    let tree = h
        .session_runtime
        .agent_store
        .agent(&agent_id)
        .expect("tree");
    assert!(
        crate::prompt::assemble_prompt_context_from(tree, tree.head())
            .context
            .flatten()
            .contains(&replacement)
    );
    h.shutdown().expect("shutdown");
}

/// Selects a synthetic second provider on the existing quiet test route.
fn select_foreign_provider(h: &mut Harness, cid: &AgentId) {
    let source: tau_proto::ModelId = "test/model".into();
    let destination: tau_proto::ModelId = "foreign/model".into();
    let mut info = h.provider_runtime.model_info[&source].clone();
    info.id = destination.clone();
    h.provider_runtime
        .model_info
        .insert(destination.clone(), info);
    h.provider_runtime.model_routes.insert(
        destination.clone(),
        h.provider_runtime.model_routes[&source].clone(),
    );
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(cid)
        .expect("agent")
        .identity
        .model_override = Some(destination);
}

/// Counts actual materializations, not selection acknowledgements.
fn request_count(h: &Harness) -> usize {
    event_log_events(h)
        .iter()
        .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
        .count()
}

/// The dispatch boundary must refuse an incompatible native replacement before
/// materializing or delivering another paid request, leaving the prefix intact.
#[test]
fn provider_switch_dispatch_refuses_opaque_compaction_before_provider_request() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(&agent_id),
    );
    let prompt = read_nth_prompt_created(&h, 0);
    let replacement = ContextItem::Compaction(
        tau_proto::OpaqueProviderItem::from_raw_json(
            r#"{ "type": "compaction", "encrypted_content": "synthetic" }"#,
        )
        .expect("opaque replacement"),
    );
    let mut response = provider_text_response(&prompt.agent_prompt_id, prompt.agent_id, "unused");
    response.output_items = vec![replacement.clone()];
    h.handle_provider_response_finished(response)
        .expect("compact");
    let requests = request_count(&h);
    select_foreign_provider(&mut h, &cid);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("continue elsewhere".to_owned()))
        .expect("activation handled");
    assert_eq!(request_count(&h), requests);
    assert!(event_log_events(&h).iter().any(|event| matches!(
        event, Event::HarnessNotice(notice) if notice.message.contains("Cannot switch provider")
    )));
    let tree = h
        .session_runtime
        .agent_store
        .agent(&agent_id)
        .expect("tree");
    assert!(
        crate::prompt::assemble_prompt_context_from(tree, tree.head())
            .context
            .flatten()
            .contains(&replacement)
    );
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .in_flight_prompt
            .is_none()
    );
    h.shutdown().expect("shutdown");
}

/// Ordinary dispatch must actually send the portable projection and publish a
/// UI-only diagnostic once, rather than merely testing a conversion helper.
#[test]
fn provider_switch_dispatch_warns_once_and_sends_portable_history() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("first".to_owned()))
        .expect("dispatch");
    let first = read_nth_prompt_created(&h, 0);
    let mut response =
        provider_text_response(&first.agent_prompt_id, first.agent_id, "portable answer");
    response.output_items.insert(
        0,
        ContextItem::Reasoning(
            tau_proto::OpaqueProviderItem::from_raw_json(
                r#"{ "type": "reasoning", "encrypted_content": "synthetic" }"#,
            )
            .expect("reasoning"),
        ),
    );
    h.handle_provider_response_finished(response)
        .expect("finish");
    select_foreign_provider(&mut h, &cid);
    for _ in 0..2 {
        let index = request_count(&h);
        h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("continue".to_owned()))
            .expect("dispatch");
        let prompt = read_nth_prompt_created(&h, index);
        assert!(
            !prompt
                .context
                .flatten_iter()
                .any(|item| matches!(item, ContextItem::Reasoning(_)))
        );
        assert!(
            serde_json::to_string(&prompt.context)
                .expect("context")
                .contains("portable answer")
        );
        h.handle_provider_response_finished(provider_text_response(
            &prompt.agent_prompt_id,
            prompt.agent_id,
            "continued",
        ))
        .expect("finish");
    }
    let warnings = event_log_events(&h)
        .into_iter()
        .filter(|event| {
            matches!(
                event, Event::HarnessNotice(notice)
                    if notice.message.contains("continuation is best effort")
                        && notice.purpose == tau_proto::NoticePurpose::Diagnostic
            )
        })
        .count();
    assert_eq!(warnings, 1);
    h.shutdown().expect("shutdown");
}
