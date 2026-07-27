mod agent_context;
mod custom_event;
mod internal_prompt;
mod metadata_request;
mod notice_request;
mod prompt_fragment;
mod session_discovery;
mod shell_report;
mod start_agent;
mod terminal_output;
mod tool_lifecycle;
mod tool_progress;
mod tool_request;
mod tool_terminal;
mod ui_liveness;

use super::dispatch::{context_overflow_response, provider_text_response};
use super::*;
use crate::harness::interception::AgentPublishCompletion;
use crate::harness::{PendingTool, background_completion_prompt};

/// Construct one forged-provenance report for ordinary extension publication.
fn extension_message_report(message_id: &str) -> Event {
    Event::MessageDeliveredReported(tau_proto::MessageDelivered::new(
        tau_proto::MessagePublisherId::new("forged"),
        tau_proto::MessageAgentTarget::new("invalid target"),
        tau_proto::MessageFactId::new(message_id),
        tau_proto::MessageParty {
            stable_id: "sender-1".to_owned(),
            display_name: None,
            sender_auth: None,
        },
        None,
        "hello",
    ))
}

/// Construct one provider model declaration with an observable context window.
fn provider_models_declaration(model: &str, context_window: u64) -> Event {
    Event::ProviderModelsDeclared(tau_proto::ProviderModelsDeclared {
        models: vec![tau_proto::ProviderModelInfo {
            id: model.into(),
            display_name: None,
            tags: Vec::new(),
            supported_tool_types: Vec::new(),
            input_modalities: Vec::new(),
            tool_result_modalities: Vec::new(),
            supports_parallel_tool_calls: true,
            default_affinity: 0,
            context_window,
            efforts: vec![tau_proto::Effort::Medium],
            verbosities: vec![tau_proto::Verbosity::Medium],
            thinking_summaries: vec![tau_proto::ThinkingSummary::Auto],
            supports_compaction: false,
            supports_standalone_compaction: false,
            standalone_compaction_threshold: None,
            est_uncached_input_cost_1m_usd: Default::default(),
            est_cached_input_cost_1m_usd: Default::default(),
            est_output_cost_1m_usd: Default::default(),
        }],
    })
}

/// Clear the quiet fixture's startup model without adding declaration log
/// noise.
fn clear_interception_fixture_models(h: &mut Harness) {
    let provider_id = h
        .extension_connection_id("provider")
        .expect("quiet provider")
        .to_owned();
    h.set_provider_models(&provider_id, Vec::new());
}

/// Collect committed model declaration/state events with their delivery source.
fn committed_provider_model_events(h: &Harness) -> Vec<(Option<tau_proto::ConnectionId>, Event)> {
    let mut events = Vec::new();
    let mut seq = crate::event_log::EventLogSeq::new(0);
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        let relevant = match &entry.event {
            Event::ProviderModelsDeclared(_) => entry.source.as_deref() == Some("model-provider"),
            Event::ProviderModelsUpdated(update) => update
                .models
                .iter()
                .any(|model| model.id.provider.as_str() == "declared"),
            _ => false,
        };
        if relevant {
            events.push((entry.source, entry.event));
        }
    }
    events
}

/// A model declaration enters exact interception and a drop prevents every
/// downstream model-state effect.
#[test]
fn dropping_provider_model_declaration_prevents_canonical_state() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    clear_interception_fixture_models(&mut h);
    connect_ready_configured_extension(
        &mut h,
        "model-provider",
        "configured-provider",
        tau_proto::ClientKind::Provider,
    );
    connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::PROVIDER_MODELS_DECLARED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");

    h.handle_extension_event_inner_with_persist(
        "model-provider",
        provider_models_declaration("declared/dropped", 1),
        Some(false),
    )
    .expect("declare models");
    assert!(matches!(
        h.pending_intercept.as_ref().map(|pending| &pending.event),
        Some(Event::ProviderModelsDeclared(_))
    ));
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop declaration");

    let model: tau_proto::ModelId = "declared/dropped".into();
    assert!(!h.provider_model_routes.contains_key(&model));
    assert!(committed_provider_model_events(&h).is_empty());
}

/// A model declaration deferred behind another intercepted publication updates
/// process-global provider state across rollover for the same live generation.
#[test]
fn rollover_applies_deferred_provider_models_for_current_generation() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    clear_interception_fixture_models(&mut h);
    connect_ready_configured_extension(
        &mut h,
        "model-provider",
        "configured-provider",
        tau_proto::ClientKind::Provider,
    );
    connect_test_tool(&mut h, "rollover-blocker");
    h.handle_extension_event(
        "rollover-blocker",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register rollover blocker");
    h.publish_event(None, draft_event("block provider models"));
    h.handle_extension_event_inner_with_persist(
        "model-provider",
        provider_models_declaration("declared/rollover", 1234),
        Some(false),
    )
    .expect("defer model declaration");
    let model: tau_proto::ModelId = "declared/rollover".into();
    assert!(!h.provider_model_routes.contains_key(&model));

    h.switch_session("replacement".into(), tau_proto::SessionStartReason::New)
        .expect("switch session");

    assert_eq!(
        h.provider_model_routes
            .get(&model)
            .map(tau_proto::ConnectionId::as_str),
        Some("model-provider")
    );
    assert_eq!(h.provider_model_info[&model].context_window, 1234);
    assert!(matches!(
        committed_provider_model_events(&h).as_slice(),
        [
            (Some(declaration_source), Event::ProviderModelsDeclared(_)),
            (Some(canonical_source), Event::ProviderModelsUpdated(_)),
        ] if declaration_source == "model-provider"
            && canonical_source == HARNESS_CONNECTION_ID
    ));
}

/// Same-name declaration replacement drives canonicalization from the committed
/// payload, preserving declaration-before-state order and immutable sources.
#[test]
fn replaced_provider_model_declaration_drives_canonical_state() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    clear_interception_fixture_models(&mut h);
    connect_ready_configured_extension(
        &mut h,
        "model-provider",
        "configured-provider",
        tau_proto::ClientKind::Provider,
    );
    connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::PROVIDER_MODELS_DECLARED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    h.handle_extension_event_inner_with_persist(
        "model-provider",
        provider_models_declaration("declared/original", 1),
        Some(false),
    )
    .expect("declare models");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(provider_models_declaration(
                "declared/replacement",
                2,
            )))),
        })),
    )
    .expect("replace declaration");

    let events = committed_provider_model_events(&h);
    assert!(matches!(
        events.as_slice(),
        [
            (Some(declaration_source), Event::ProviderModelsDeclared(declaration)),
            (Some(canonical_source), Event::ProviderModelsUpdated(canonical)),
        ] if declaration_source == "model-provider"
            && canonical_source == HARNESS_CONNECTION_ID
            && declaration.models[0].id.to_string() == "declared/replacement"
            && canonical.models[0].id.to_string() == "declared/replacement"
    ));
    let replacement: tau_proto::ModelId = "declared/replacement".into();
    let original: tau_proto::ModelId = "declared/original".into();
    assert_eq!(
        h.provider_model_routes
            .get(&replacement)
            .map(tau_proto::ConnectionId::as_str),
        Some("model-provider")
    );
    assert!(!h.provider_model_routes.contains_key(&original));
}

/// A provider-prefix interceptor sees both the declaration and canonical state;
/// it may mutate the declaration but cannot rewrite or drop canonical state.
#[test]
fn provider_prefix_interception_protects_canonical_model_state() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    clear_interception_fixture_models(&mut h);
    connect_ready_configured_extension(
        &mut h,
        "model-provider",
        "configured-provider",
        tau_proto::ClientKind::Provider,
    );
    connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Prefix("provider".to_owned())],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    h.handle_extension_event_inner_with_persist(
        "model-provider",
        provider_models_declaration("declared/protected", 10),
        Some(false),
    )
    .expect("declare models");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit declaration");
    assert!(matches!(
        h.pending_intercept.as_ref().map(|pending| &pending.event),
        Some(Event::ProviderModelsUpdated(_))
    ));
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(Event::ProviderModelsUpdated(
                tau_proto::ProviderModelsUpdated {
                    publisher_extension_id: tau_proto::ExtensionName::from("configured-provider"),
                    models: Vec::new(),
                },
            )))),
        })),
    )
    .expect("reject canonical rewrite");

    let model: tau_proto::ModelId = "declared/protected".into();
    assert!(h.provider_model_routes.contains_key(&model));
    assert!(matches!(
        committed_provider_model_events(&h).as_slice(),
        [
            (_, Event::ProviderModelsDeclared(_)),
            (_, Event::ProviderModelsUpdated(update)),
        ] if update.models[0].id == model
    ));

    h.handle_extension_event_inner_with_persist(
        "model-provider",
        provider_models_declaration("declared/drop-protected", 20),
        Some(false),
    )
    .expect("declare replacement models");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit replacement declaration");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("canonical drop is overridden");
    let replacement: tau_proto::ModelId = "declared/drop-protected".into();
    assert!(h.provider_model_routes.contains_key(&replacement));
}

/// Disconnecting after a canonical update enters interception rewrites that
/// must-pass snapshot to the provider's empty terminal state.
#[test]
fn provider_disconnect_rewrites_parked_canonical_state_to_empty() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    clear_interception_fixture_models(&mut h);
    connect_ready_configured_extension(
        &mut h,
        "model-provider",
        "configured-provider",
        tau_proto::ClientKind::Provider,
    );
    connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Prefix("provider".to_owned())],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");

    h.handle_extension_event_inner_with_persist(
        "model-provider",
        provider_models_declaration("declared/disconnected", 10),
        Some(false),
    )
    .expect("declare models");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit declaration");
    assert!(matches!(
        h.pending_intercept.as_ref().map(|pending| &pending.event),
        Some(Event::ProviderModelsUpdated(update)) if !update.models.is_empty()
    ));
    let model: tau_proto::ModelId = "declared/disconnected".into();
    assert!(h.provider_model_routes.contains_key(&model));

    h.handle_disconnect("model-provider");
    assert!(matches!(
        h.pending_intercept.as_ref().map(|pending| &pending.event),
        Some(Event::ProviderModelsUpdated(update))
            if update.publisher_extension_id.as_str() == "configured-provider"
                && update.models.is_empty()
    ));
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit corrected canonical state");

    assert!(!h.provider_model_routes.contains_key(&model));
    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::ProviderModelsUpdated(update)
            if update.publisher_extension_id.as_str() == "configured-provider"
                && update.models.is_empty()
    )));
}

/// A parked declaration retains its admitted source envelope, but disconnect
/// cleanup and extension-generation replacement prevent it from recreating a
/// stale provider route.
#[test]
fn parked_provider_declaration_cannot_mutate_replacement_generation() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    clear_interception_fixture_models(&mut h);
    connect_ready_configured_extension(
        &mut h,
        "model-provider",
        "original-provider",
        tau_proto::ClientKind::Provider,
    );
    connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::PROVIDER_MODELS_DECLARED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    h.handle_extension_event_inner_with_persist(
        "model-provider",
        provider_models_declaration("declared/stale", 1),
        Some(false),
    )
    .expect("park declaration");

    h.handle_disconnect("model-provider");
    h.extensions.entries.remove("model-provider");
    connect_ready_configured_extension(
        &mut h,
        "model-provider",
        "replacement-provider",
        tau_proto::ClientKind::Provider,
    );
    h.extensions
        .entries
        .get_mut("model-provider")
        .expect("replacement provider")
        .instance_id = 43.into();
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit old declaration");

    let events = committed_provider_model_events(&h);
    assert!(matches!(
        events.as_slice(),
        [(Some(source), Event::ProviderModelsDeclared(_))]
            if source == "model-provider"
    ));
    let stale: tau_proto::ModelId = "declared/stale".into();
    assert!(!h.provider_model_routes.contains_key(&stale));
}

/// An old generation's dropped declaration cannot release the activation
/// reservation or pending count owned by a handshaking replacement that
/// reuses the same synthetic connection id.
#[test]
fn parked_old_generation_drop_cannot_activate_same_id_replacement() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    clear_interception_fixture_models(&mut h);
    connect_ready_configured_extension(
        &mut h,
        "model-provider",
        "original-provider",
        tau_proto::ClientKind::Provider,
    );
    connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::PROVIDER_MODELS_DECLARED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    h.handle_extension_event_inner_with_persist(
        "model-provider",
        provider_models_declaration("declared/old-generation", 1),
        Some(false),
    )
    .expect("park old declaration");

    h.handle_disconnect("model-provider");
    h.extensions.entries.remove("model-provider");
    connect_ready_configured_extension(
        &mut h,
        "model-provider",
        "replacement-provider",
        tau_proto::ClientKind::Provider,
    );
    let replacement = h
        .extensions
        .entries
        .get_mut("model-provider")
        .expect("replacement provider");
    replacement.instance_id = 43.into();
    replacement.state = crate::extension::ExtensionState::Handshaking;
    h.extensions.activation_staging.insert(
        "model-provider".into(),
        crate::harness::extensions::ExtensionActivationStage::default(),
    );
    h.handle_extension_event_inner_with_persist(
        "model-provider",
        provider_models_declaration("declared/new-generation", 2),
        Some(false),
    )
    .expect("queue replacement declaration");
    h.handle_extension_message(
        "model-provider",
        TestMessage::Ready(tau_proto::Ready { message: None }),
    )
    .expect("replacement ready waits");

    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop old declaration");
    assert_eq!(
        h.extensions.entries["model-provider"].state,
        crate::extension::ExtensionState::Handshaking
    );
    assert_eq!(
        h.extensions
            .pending_provider_model_declarations
            .get("model-provider"),
        Some(&1)
    );
    assert!(matches!(
        h.pending_intercept.as_ref().map(|pending| &pending.event),
        Some(Event::ProviderModelsDeclared(update))
            if update.models[0].id.to_string() == "declared/new-generation"
    ));

    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit replacement declaration");
    let model: tau_proto::ModelId = "declared/new-generation".into();
    assert_eq!(
        h.extensions.entries["model-provider"].state,
        crate::extension::ExtensionState::Ready
    );
    assert_eq!(
        h.provider_model_routes
            .get(&model)
            .map(tau_proto::ConnectionId::as_str),
        Some("model-provider")
    );
}

/// Assert one report selector sees the report before downstream
/// canonicalization.
fn assert_message_report_is_intercepted(selector: EventSelector, intercepts_canonical: bool) {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let interceptor_sink = connect_test_tool(&mut h, "message-interceptor");
    h.handle_extension_event(
        "message-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![selector],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    connect_ready_message_publisher(&mut h, "bridge-connection", "configured-bridge");

    h.handle_extension_event_inner_with_persist(
        "bridge-connection",
        extension_message_report("m1"),
        Some(false),
    )
    .expect("message report intake");

    assert!(h.pending_intercept.is_some());
    assert!(
        interceptor_sink
            .lock()
            .expect("interceptor sink")
            .iter()
            .any(|frame| matches!(
                &frame.frame,
                HarnessOutputMessage::InterceptRequest(request)
                    if matches!(request.event.as_ref(), Event::MessageDeliveredReported(_))
            )),
        "message report must enter ordinary interception"
    );
    assert!(
        h.store
            .session_events("s1")
            .expect("fallback records")
            .is_empty(),
        "canonical fact must wait for report commit"
    );
    h.handle_extension_event(
        "message-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit report");
    if intercepts_canonical {
        assert!(h.pending_intercept.is_some());
        h.handle_extension_event(
            "message-interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Drop,
            })),
        )
        .expect("protected canonical fact survives drop");
    }
    assert!(h.pending_intercept.is_none());
    let records = h.store.session_events("s1").expect("fallback records");
    assert_eq!(records.len(), 1);
    assert!(matches!(
        &records[0].event,
        Event::MessageDelivered(fact)
            if fact.publisher_extension_id.as_str() == "configured-bridge"
                && fact.message_id.as_str() == "m1"
    ));
}

/// An exact report selector runs before downstream canonicalization.
#[test]
fn message_report_enters_exact_interception() {
    assert_message_report_is_intercepted(
        EventSelector::Exact(tau_proto::EventName::MESSAGE_DELIVERED_REPORTED),
        false,
    );
}

/// A message-prefix interceptor sees both the report and protected canonical
/// fact.
#[test]
fn message_report_and_canonical_fact_enter_prefix_interception() {
    assert_message_report_is_intercepted(EventSelector::Prefix("message".to_owned()), true);
}

/// Dropping a mutable bridge report prevents downstream canonical publication.
#[test]
fn dropping_message_report_produces_no_canonical_fact() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_message_publisher(&mut h, "bridge", "configured-bridge");
    connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::MESSAGE_DELIVERED_REPORTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    h.handle_extension_event_inner_with_persist(
        "bridge",
        extension_message_report("original"),
        Some(false),
    )
    .expect("report");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop report");
    assert!(h.store.session_events("s1").expect("journal").is_empty());
    assert!(event_log_events(&h).iter().all(|event| {
        !matches!(
            event,
            Event::MessageDelivered(_) | Event::MessageDeliveredReported(_)
        )
    }));
}

/// A message report active in interception at rollover keeps its raw
/// observation but cannot canonicalize, wake a branch, or create an activation
/// from the stale session admission.
#[test]
fn rollover_message_report_is_observation_only() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    connect_ready_message_publisher(&mut h, "bridge", "configured-bridge");
    connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::MESSAGE_DELIVERED_REPORTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    let report = Event::MessageDeliveredReported(tau_proto::MessageDelivered::new(
        tau_proto::MessagePublisherId::new("forged"),
        tau_proto::MessageAgentTarget::new(agent_id.as_str()),
        tau_proto::MessageFactId::new("rollover-message"),
        tau_proto::MessageParty {
            stable_id: "sender-1".to_owned(),
            display_name: None,
            sender_auth: None,
        },
        None,
        "wake replacement",
    ));
    h.handle_extension_event_inner_with_persist("bridge", report, Some(false))
        .expect("park report");

    h.switch_session("replacement".into(), tau_proto::SessionStartReason::New)
        .expect("switch session");

    assert!(event_log_events(&h).iter().any(|event| {
        matches!(
            event,
            Event::MessageDeliveredReported(report)
                if report.message_id.as_str() == "rollover-message"
        )
    }));
    assert!(!event_log_events(&h).iter().any(|event| {
        matches!(
            event,
            Event::MessageDelivered(fact)
                if fact.message_id.as_str() == "rollover-message"
        )
    }));
    assert!(h.pending_publish_idle_dispatches.is_empty());
}

/// Downstream canonicalization consumes the committed same-name replacement,
/// not the pre-interception report.
#[test]
fn replacing_message_report_canonicalizes_replacement() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_message_publisher(&mut h, "bridge", "configured-bridge");
    connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::MESSAGE_DELIVERED_REPORTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    h.handle_extension_event_inner_with_persist(
        "bridge",
        extension_message_report("original"),
        Some(false),
    )
    .expect("report");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(extension_message_report("replacement")))),
        })),
    )
    .expect("replace report");
    assert!(matches!(
        h.store.session_events("s1").expect("journal").as_slice(),
        [tau_core::PersistedSessionEvent {
            event: Event::MessageDelivered(fact),
            ..
        }] if fact.message_id.as_str() == "replacement"
            && fact.publisher_extension_id.as_str() == "configured-bridge"
    ));
}

/// Authenticated publisher identity survives bridge disconnect while the report
/// is parked in interception.
#[test]
fn parked_message_report_survives_bridge_disconnect() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_message_publisher(&mut h, "bridge", "configured-bridge");
    connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::MESSAGE_DELIVERED_REPORTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    h.handle_extension_event_inner_with_persist(
        "bridge",
        extension_message_report("m1"),
        Some(false),
    )
    .expect("report");
    h.handle_disconnect("bridge");
    // Supervised replacement removes the disconnected entry before installing
    // its successor connection.
    h.extensions.entries.remove("bridge");
    assert!(!h.extensions.entries.contains_key("bridge"));
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("pass report");
    assert!(matches!(
        h.store.session_events("s1").expect("journal").as_slice(),
        [tau_core::PersistedSessionEvent {
            source: Some(source),
            event: Event::MessageDelivered(fact),
            ..
        }] if source.as_str() == HARNESS_CONNECTION_ID
            && fact.publisher_extension_id.as_str() == "configured-bridge"
    ));
}

/// A report arriving behind an unrelated intercepted publish retains FIFO order
/// before downstream canonical publication.
#[test]
fn message_report_preserves_deferred_publish_fifo() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let interceptor = connect_test_tool(&mut h, "draft-interceptor");
    h.handle_extension_event(
        "draft-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    connect_ready_message_publisher(&mut h, "bridge-connection", "configured-bridge");
    let observer = connect_test_client(&mut h, "fifo-ui", tau_proto::ClientKind::Ui);
    h.handle_client_event(
        "fifo-ui",
        TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
            historical_selectors: Vec::new(),
            live_selectors: vec![
                EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT),
                EventSelector::Exact(tau_proto::EventName::MESSAGE_DELIVERED),
            ],
        })),
    )
    .expect("subscribe observer");
    h.publish_event(None, draft_event("held"));
    assert!(h.pending_intercept.is_some());

    h.handle_extension_event_inner_with_persist(
        "bridge-connection",
        extension_message_report("m1"),
        Some(false),
    )
    .expect("queue message report");
    assert!(
        h.store
            .session_events("s1")
            .expect("fallback records")
            .is_empty()
    );

    h.handle_extension_event(
        "draft-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release intercepted publish");

    assert_eq!(
        h.store
            .session_events("s1")
            .expect("fallback records")
            .len(),
        1
    );
    assert!(
        interceptor
            .lock()
            .expect("interceptor sink")
            .iter()
            .filter(|frame| matches!(frame.frame, HarnessOutputMessage::InterceptRequest(_)))
            .count()
            == 1,
        "only the earlier draft matches this interceptor"
    );
    let delivered_names = observer
        .lock()
        .expect("observer sink")
        .iter()
        .filter_map(|frame| peel_inner_event(&frame.frame).map(Event::name))
        .filter(|name| {
            *name == tau_proto::EventName::UI_PROMPT_DRAFT
                || *name == tau_proto::EventName::MESSAGE_DELIVERED
        })
        .collect::<Vec<_>>();
    assert_eq!(
        delivered_names,
        vec![
            tau_proto::EventName::UI_PROMPT_DRAFT,
            tau_proto::EventName::MESSAGE_DELIVERED
        ]
    );
}

fn prompt_created_count(h: &Harness) -> u64 {
    let mut cursor = crate::event_log::EventLogSeq::new(0);
    let mut count = 0;
    while let Some(entry) = h.event_log.get_next_from(cursor) {
        cursor = entry.seq.next();
        if matches!(entry.event, Event::AgentPromptCreated(_)) {
            count += 1;
        }
    }
    count
}

fn add_second_test_model(h: &mut Harness) {
    let first: tau_proto::ModelId = "echo/model".into();
    let second: tau_proto::ModelId = "other/model".into();
    let mut info = h.provider_model_info[&first].clone();
    info.id = second.clone();
    let route = h.provider_model_routes[&first].clone();
    h.provider_model_info.insert(second.clone(), info);
    h.provider_model_routes.insert(second, route);
}

fn queue_intercepted_peer_receive(
    h: &mut Harness,
    connection_id: &tau_proto::ConnectionId,
    recipient_id: tau_proto::AgentId,
    suffix: &str,
) {
    h.external_message_peers.insert(connection_id.clone());
    let result = h.complete_external_agent_message_auth(
        connection_id.clone(),
        h.current_session_generation,
        tau_proto::ExternalAgentMessageRequest {
            request_id: format!("peer-request-{suffix}"),
            message_id: format!("peer-message-{suffix}").into(),
            capability: "test-capability".to_owned(),
            sender_session_id: "sender-session".into(),
            sender_id: crate::parse_agent_id("sender_agent"),
            recipient_session_id: h.current_session_id.clone(),
            recipient: tau_proto::ExternalAgentMessageRecipient::Exact(recipient_id),
            kind: tau_proto::AgentMessageKind::Message,
            message: "peer body".to_owned(),
        },
        Ok(()),
    );
    assert!(result.is_none(), "success must wait for receive commit");
}

fn committed_peer_receives(h: &Harness) -> Vec<tau_proto::AgentMessageReceived> {
    event_log_events(h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentMessageReceived(received)
                if received.sender_session_id.as_deref() == Some("sender-session") =>
            {
                Some(received)
            }
            _ => None,
        })
        .collect()
}

/// Remote success remains pending while interception parks the exact receive
/// projection and is released only by the post-persistence commit reaction.
#[test]
fn peer_receive_ack_waits_for_intercepted_projection_commit() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let recipient_id = durable_agent_id_for_conversation(&h, &cid).clone();
    let _interceptor = connect_test_tool(&mut h, "peer-receive-interceptor");
    h.handle_extension_event(
        "peer-receive-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MESSAGE_RECEIVED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    let connection_id = tau_proto::ConnectionId::from("peer-client");

    queue_intercepted_peer_receive(&mut h, &connection_id, recipient_id, "commit");

    assert_eq!(h.pending_external_receive_acks.len(), 1);
    assert!(committed_peer_receives(&h).is_empty());
    h.handle_extension_event(
        "peer-receive-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("pass receive");
    assert!(h.pending_external_receive_acks.is_empty());
    assert_eq!(committed_peer_receives(&h).len(), 1);
}

/// An interceptor rejection fails and removes the live continuation rather than
/// acknowledging or committing a receive projection.
#[test]
fn peer_receive_interception_drop_never_acknowledges_or_commits() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let recipient_id = durable_agent_id_for_conversation(&h, &cid).clone();
    let _interceptor = connect_test_tool(&mut h, "peer-drop-interceptor");
    h.handle_extension_event(
        "peer-drop-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MESSAGE_RECEIVED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    let connection_id = tau_proto::ConnectionId::from("peer-client");

    queue_intercepted_peer_receive(&mut h, &connection_id, recipient_id, "drop");
    h.handle_extension_event(
        "peer-drop-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop receive");

    assert!(h.pending_external_receive_acks.is_empty());
    assert!(committed_peer_receives(&h).is_empty());
}

/// Recipient disappearance while a receive is parked invalidates the
/// continuation at commit-time and cannot produce a durable receive.
#[test]
fn peer_receive_target_disappearance_before_commit_fails() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let recipient_id = durable_agent_id_for_conversation(&h, &cid).clone();
    let _interceptor = connect_test_tool(&mut h, "peer-target-interceptor");
    h.handle_extension_event(
        "peer-target-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MESSAGE_RECEIVED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    let connection_id = tau_proto::ConnectionId::from("peer-client");
    queue_intercepted_peer_receive(&mut h, &connection_id, recipient_id, "target-gone");

    h.remove_agent(&cid);
    h.handle_extension_event(
        "peer-target-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("pass receive");

    assert!(h.pending_external_receive_acks.is_empty());
    assert!(committed_peer_receives(&h).is_empty());
}

/// Current-session bare routing delays its sent projection until the exact
/// receive projection passes interception and commits.
#[test]
fn local_peer_sent_projection_waits_for_receive_commit() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    configure_inter_session_receivers(&mut h, &[("engineer", false)]);
    let cid = ensure_test_user_agent(&mut h);
    let _interceptor = connect_test_tool(&mut h, "local-peer-interceptor");
    h.handle_extension_event(
        "local-peer-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MESSAGE_RECEIVED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");

    h.publish_peer_entrypoint_message_from_agent(
        &cid,
        "local peer body".to_owned(),
        "local-peer-call".into(),
        ToolName::new("message"),
        tau_proto::ToolType::Function,
    )
    .expect("queue local peer");

    assert!(h.pending_external_receive_acks.len() == 1);
    assert!(
        !event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::AgentMessageSent(_)))
    );
    h.handle_extension_event(
        "local-peer-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("pass receive");
    assert!(h.pending_external_receive_acks.is_empty());
    assert!(
        event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::AgentMessageSent(_)))
    );
}

/// Current-session bare routing enforces the same 64 KiB body limit as socket
/// routing before admission or auto-start creation.
#[test]
fn local_peer_oversized_message_rejects_before_auto_start() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let sender = ensure_test_user_agent(&mut h);
    let peer_role = h.available_roles["engineer"].clone();
    h.available_roles.insert("peer".to_owned(), peer_role);
    configure_inter_session_receivers(&mut h, &[("peer", true)]);
    let agents_before = h.agents.len();

    let error = h
        .publish_peer_entrypoint_message_from_agent(
            &sender,
            "x".repeat(64 * 1024 + 1),
            "oversized-local-peer".into(),
            ToolName::new("message"),
            tau_proto::ToolType::Function,
        )
        .expect_err("oversized local peer message rejected");

    assert_eq!(error, "peer message exceeds the 64 KiB limit");
    assert_eq!(h.agents.len(), agents_before);
    assert!(h.pending_external_receive_acks.is_empty());
}

/// Current-session routing uses the same admission, auto-start, and post-commit
/// completion path as remote routing, while preserving local sender provenance.
#[test]
fn local_peer_auto_start_reports_started_only_after_receive_commit() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let sender = ensure_test_user_agent(&mut h);
    let peer_role = h.available_roles["engineer"].clone();
    h.available_roles.insert("peer".to_owned(), peer_role);
    configure_inter_session_receivers(&mut h, &[("peer", true)]);
    let _interceptor = connect_test_tool(&mut h, "local-auto-start-interceptor");
    h.handle_extension_event(
        "local-auto-start-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MESSAGE_RECEIVED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");

    h.publish_peer_entrypoint_message_from_agent(
        &sender,
        "auto-start body".to_owned(),
        "local-auto-start-call".into(),
        ToolName::new("message"),
        tau_proto::ToolType::Function,
    )
    .expect("queue local auto-start");

    let pending = h
        .pending_external_receive_acks
        .values()
        .next()
        .expect("pending receive");
    assert!(pending.started);
    let recipient_id = pending.recipient_id.clone();
    let recipient_cid = h
        .agent_routes
        .get(recipient_id.as_str())
        .expect("auto-started route");
    let recipient = &h.agents[recipient_cid];
    assert_eq!(recipient.role.as_deref(), Some("peer"));
    assert_eq!(recipient.parent_agent_id, None);
    assert!(
        !event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::AgentMessageSent(_)))
    );

    h.handle_extension_event(
        "local-auto-start-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("pass receive");

    assert!(h.pending_external_receive_acks.is_empty());
    assert!(
        event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::AgentMessageSent(_)))
    );
}

/// A parked local auto-start is visible to a remote send immediately, so both
/// precommit deliveries coalesce on one endpoint rather than creating fan-out.
#[test]
fn parked_local_and_remote_peer_sends_coalesce_on_one_auto_start() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let sender = ensure_test_user_agent(&mut h);
    let peer_role = h.available_roles["engineer"].clone();
    h.available_roles.insert("peer".to_owned(), peer_role);
    configure_inter_session_receivers(&mut h, &[("peer", true)]);
    let _interceptor = connect_test_tool(&mut h, "coalesce-interceptor");
    h.handle_extension_event(
        "coalesce-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MESSAGE_RECEIVED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    h.publish_peer_entrypoint_message_from_agent(
        &sender,
        "local first".to_owned(),
        "coalesce-local-call".into(),
        ToolName::new("message"),
        tau_proto::ToolType::Function,
    )
    .expect("queue local auto-start");
    let recipient = h
        .pending_external_receive_acks
        .values()
        .next()
        .expect("local pending")
        .recipient_id
        .clone();
    let connection_id = tau_proto::ConnectionId::from("coalesce-peer-client");
    h.external_message_peers.insert(connection_id.clone());
    let remote = h.complete_external_agent_message_auth(
        connection_id,
        h.current_session_generation,
        tau_proto::ExternalAgentMessageRequest {
            request_id: "coalesce-remote".to_owned(),
            message_id: "coalesce-remote-message".into(),
            capability: "capability".to_owned(),
            sender_session_id: "sender-session".into(),
            sender_id: crate::parse_agent_id("sender_agent"),
            recipient_session_id: h.current_session_id.clone(),
            recipient: tau_proto::ExternalAgentMessageRecipient::BareEntrypoint,
            kind: tau_proto::AgentMessageKind::Message,
            message: "remote second".to_owned(),
        },
        Ok(()),
    );

    assert!(remote.is_none());
    assert_eq!(h.agents.len(), 2, "sender plus exactly one peer endpoint");
    assert_eq!(h.pending_external_receive_acks.len(), 2);
    assert!(
        h.pending_external_receive_acks
            .values()
            .all(|pending| pending.recipient_id == recipient)
    );
    assert_eq!(
        h.pending_external_receive_acks
            .values()
            .filter(|pending| pending.started)
            .count(),
        1
    );
}

/// Failed callback correlation cannot consume auto-start authority or create an
/// endpoint, even when the target explicitly configured an auto-start role.
#[test]
fn peer_auto_start_authentication_failure_precedes_spend() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    configure_inter_session_receivers(&mut h, &[("engineer", true)]);
    let request = tau_proto::ExternalAgentMessageRequest {
        request_id: "auth-before-spend".to_owned(),
        message_id: "auth-before-spend-message".into(),
        capability: "invalid".to_owned(),
        sender_session_id: "sender-session".into(),
        sender_id: crate::parse_agent_id("sender_agent"),
        recipient_session_id: h.current_session_id.clone(),
        recipient: tau_proto::ExternalAgentMessageRecipient::BareEntrypoint,
        kind: tau_proto::AgentMessageKind::Message,
        message: "must not create".to_owned(),
    };

    let result = h
        .complete_external_agent_message_auth(
            "peer-client".into(),
            h.current_session_generation,
            request,
            Err("external message authentication failed".to_owned()),
        )
        .expect("terminal authentication result");

    assert_eq!(
        result.error.as_deref(),
        Some("external message authentication failed")
    );
    assert!(h.agents.is_empty());
    assert!(h.pending_external_receive_acks.is_empty());
}

/// A callback completion that outlives its session generation or peer socket is
/// rejected before selection, admission, or auto-start creation.
#[test]
fn stale_or_disconnected_auth_completion_cannot_auto_start() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    configure_inter_session_receivers(&mut h, &[("engineer", true)]);
    let target_session = h.current_session_id.clone();
    let request = |suffix: &str| tau_proto::ExternalAgentMessageRequest {
        request_id: format!("stale-auth-{suffix}"),
        message_id: format!("stale-auth-message-{suffix}").into(),
        capability: "valid".to_owned(),
        sender_session_id: "sender-session".into(),
        sender_id: crate::parse_agent_id("sender_agent"),
        recipient_session_id: target_session.clone(),
        recipient: tau_proto::ExternalAgentMessageRecipient::BareEntrypoint,
        kind: tau_proto::AgentMessageKind::Message,
        message: "must not create".to_owned(),
    };
    let peer: tau_proto::ConnectionId = "peer-client".into();
    h.external_message_peers.insert(peer.clone());
    let stale = h
        .complete_external_agent_message_auth(
            peer.clone(),
            h.current_session_generation.saturating_add(1),
            request("generation"),
            Ok(()),
        )
        .expect("stale generation result");
    h.external_message_peers.remove(&peer);
    let disconnected = h
        .complete_external_agent_message_auth(
            peer,
            h.current_session_generation,
            request("disconnect"),
            Ok(()),
        )
        .expect("disconnected result");

    assert!(stale.error.is_some());
    assert!(disconnected.error.is_some());
    assert!(h.agents.is_empty());
    assert!(h.pending_external_receive_acks.is_empty());
}

/// Rollover cancels a local continuation and suspends its responder until the
/// stale reply is consumed; no old receive/sent/tool terminal fact reaches the
/// replacement session.
#[test]
fn local_peer_parked_across_rollover_has_no_stale_terminal() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    configure_inter_session_receivers(&mut h, &[("engineer", false)]);
    let cid = ensure_test_user_agent(&mut h);
    let call_id: ToolCallId = "local-rollover-call".into();
    h.tool_agents.insert(call_id.clone(), cid.clone());
    let _interceptor = connect_test_tool(&mut h, "local-rollover-interceptor");
    h.handle_extension_event(
        "local-rollover-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MESSAGE_RECEIVED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    h.publish_peer_entrypoint_message_from_agent(
        &cid,
        "old-session peer body".to_owned(),
        call_id.clone(),
        ToolName::new("message"),
        tau_proto::ToolType::Function,
    )
    .expect("queue local peer");

    h.switch_session("replacement".into(), tau_proto::SessionStartReason::New)
        .expect("switch session");
    h.handle_extension_event(
        "local-rollover-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("consume stale old-receive reply");

    assert!(h.pending_external_receive_acks.is_empty());
    assert!(!event_log_events(&h).iter().any(|event| {
        matches!(event, Event::AgentMessageSent(message) if message.message == "old-session peer body")
            || matches!(event, Event::AgentMessageReceived(message) if message.message == "old-session peer body")
            || matches!(event, Event::ToolResult(result) if result.call_id == call_id)
            || matches!(event, Event::ToolError(error) if error.call_id == call_id)
    }));
}

/// Bare entrypoint authority is revalidated at the persistence boundary, so a
/// parked receive cannot commit after the target policy is revoked.
#[test]
fn peer_receive_bare_authority_revocation_before_commit_fails() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    configure_inter_session_receivers(&mut h, &[("engineer", false)]);
    ensure_test_user_agent(&mut h);
    let _interceptor = connect_test_tool(&mut h, "bare-revoke-interceptor");
    h.handle_extension_event(
        "bare-revoke-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MESSAGE_RECEIVED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    let connection_id = tau_proto::ConnectionId::from("peer-client");
    h.external_message_peers.insert(connection_id.clone());
    let result = h.complete_external_agent_message_auth(
        connection_id,
        h.current_session_generation,
        tau_proto::ExternalAgentMessageRequest {
            request_id: "bare-revoke".to_owned(),
            message_id: "bare-revoke-message".into(),
            capability: "capability".to_owned(),
            sender_session_id: "sender-session".into(),
            sender_id: crate::parse_agent_id("sender_agent"),
            recipient_session_id: h.current_session_id.clone(),
            recipient: tau_proto::ExternalAgentMessageRecipient::BareEntrypoint,
            kind: tau_proto::AgentMessageKind::Message,
            message: "peer body".to_owned(),
        },
        Ok(()),
    );
    assert!(result.is_none());

    h.inter_session_receivers.clear();
    h.handle_extension_event(
        "bare-revoke-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("pass receive");

    assert!(h.pending_external_receive_acks.is_empty());
    assert!(committed_peer_receives(&h).is_empty());
}

/// Bare routing gets only one deterministic re-selection: invalidating the
/// replacement fails terminally without a third selection or committed receive.
#[test]
fn peer_receive_bare_target_loss_reselects_once_before_commit() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    configure_inter_session_receivers(&mut h, &[("engineer", false)]);
    ensure_test_user_agent(&mut h);
    h.create_durable_user_agent("s1".into(), "engineer");
    let _interceptor = connect_test_tool(&mut h, "bare-reselect-interceptor");
    h.handle_extension_event(
        "bare-reselect-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MESSAGE_RECEIVED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    let connection_id = tau_proto::ConnectionId::from("peer-client");
    h.external_message_peers.insert(connection_id.clone());
    let result = h.complete_external_agent_message_auth(
        connection_id,
        h.current_session_generation,
        tau_proto::ExternalAgentMessageRequest {
            request_id: "bare-reselect".to_owned(),
            message_id: "bare-reselect-message".into(),
            capability: "capability".to_owned(),
            sender_session_id: "sender-session".into(),
            sender_id: crate::parse_agent_id("sender_agent"),
            recipient_session_id: h.current_session_id.clone(),
            recipient: tau_proto::ExternalAgentMessageRecipient::BareEntrypoint,
            kind: tau_proto::AgentMessageKind::Message,
            message: "peer body".to_owned(),
        },
        Ok(()),
    );
    assert!(result.is_none());
    let original = h
        .pending_external_receive_acks
        .values()
        .next()
        .expect("pending receive")
        .recipient_id
        .clone();
    let original_cid = h
        .agent_routes
        .get(original.as_str())
        .cloned()
        .expect("original route");
    h.remove_agent(&original_cid);

    h.handle_extension_event(
        "bare-reselect-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release stale receive");
    assert_eq!(h.pending_external_receive_acks.len(), 1);
    assert!(committed_peer_receives(&h).is_empty());
    let replacement = h
        .pending_external_receive_acks
        .values()
        .next()
        .expect("replacement receive")
        .recipient_id
        .clone();
    assert_ne!(replacement, original);
    let replacement_cid = h
        .agent_routes
        .get(replacement.as_str())
        .cloned()
        .expect("replacement route");
    h.remove_agent(&replacement_cid);

    h.handle_extension_event(
        "bare-reselect-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release invalid replacement receive");

    assert!(h.pending_external_receive_acks.is_empty());
    assert!(committed_peer_receives(&h).is_empty());
    assert!(
        h.agent_routes.is_empty(),
        "second invalidation must not reselect"
    );
}

/// A parked old-generation receive retains a canceled tombstone across
/// rollover; its responder is skipped for replacement work and its later stale
/// reply is consumed without applying it.
#[test]
fn peer_receive_parked_across_rollover_cannot_commit() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let recipient_id = durable_agent_id_for_conversation(&h, &cid).clone();
    let _interceptor = connect_test_tool(&mut h, "peer-rollover-interceptor");
    h.handle_extension_event(
        "peer-rollover-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MESSAGE_RECEIVED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    let connection_id = tau_proto::ConnectionId::from("peer-client");
    let peer_results = connect_test_client(&mut h, "peer-client", tau_proto::ClientKind::External);
    queue_intercepted_peer_receive(&mut h, &connection_id, recipient_id.clone(), "rollover");
    assert_eq!(h.pending_external_receive_acks.len(), 1);
    assert_eq!(h.peer_input_rate[&recipient_id].len(), 1);

    h.switch_session("replacement".into(), tau_proto::SessionStartReason::New)
        .expect("switch session");
    assert!(h.pending_external_receive_acks.is_empty());
    assert!(h.peer_input_rate.is_empty());
    let peer_results_after_rollover = peer_results.lock().expect("peer results");
    assert!(
        peer_results_after_rollover.iter().any(|frame| {
            matches!(
                &frame.frame,
                HarnessOutputMessage::ExternalAgentMessageResult(result)
                    if result.request_id == "peer-request-rollover"
                        && result.error.as_deref()
                            == Some("target session changed before receive commit")
                        && result.recipient_id.is_none()
                        && !result.started
            )
        }),
        "peer results: {peer_results_after_rollover:?}"
    );
    drop(peer_results_after_rollover);
    let replacement_cid = ensure_test_user_agent(&mut h);
    let replacement_id = durable_agent_id_for_conversation(&h, &replacement_cid).clone();
    queue_intercepted_peer_receive(
        &mut h,
        &connection_id,
        replacement_id.clone(),
        "replacement-generation",
    );
    let committed_before_stale_reply = committed_peer_receives(&h);
    assert_eq!(committed_before_stale_reply.len(), 1);
    assert_eq!(
        committed_before_stale_reply[0].message_id.as_str(),
        "peer-message-replacement-generation"
    );
    h.handle_extension_event(
        "peer-rollover-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("consume stale old-receive reply");

    assert!(h.pending_external_receive_acks.is_empty());
    assert_eq!(committed_peer_receives(&h), committed_before_stale_reply);
    queue_intercepted_peer_receive(
        &mut h,
        &connection_id,
        replacement_id,
        "post-stale-resumption",
    );
    assert!(
        h.pending_intercept.is_some(),
        "interception must resume after consuming exactly one stale reply"
    );
    assert_eq!(h.pending_external_receive_acks.len(), 1);
    h.handle_extension_event(
        "peer-rollover-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit post-stale receive");
    assert!(h.pending_external_receive_acks.is_empty());
    assert!(
        committed_peer_receives(&h).iter().any(|received| {
            received.message_id.as_str() == "peer-message-post-stale-resumption"
        })
    );
}

/// A final response parked before commit must not fan out watch content after
/// removing the watched sender has made that endpoint non-live.
#[test]
fn intercepted_final_response_cannot_fan_out_after_watched_agent_unload() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let watched_cid = ensure_test_user_agent(&mut h);
    let watcher_cid =
        h.create_durable_user_agent(h.current_session_id.clone(), &h.selected_role.clone());
    let watched_id = durable_agent_id_for_conversation(&h, &watched_cid).to_string();
    let watcher_id = durable_agent_id_for_conversation(&h, &watcher_cid).to_string();
    h.set_agent_watch(
        &watcher_id,
        &watched_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    let interceptor = connect_test_tool(&mut h, "watch-final-interceptor");
    h.handle_extension_event(
        "watch-final-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::PROVIDER_RESPONSE_FINISHED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    h.publish_for_agent(
        &watched_cid,
        Event::ProviderResponseFinished(provider_text_response(
            &"sp-parked-watch-final".into(),
            crate::parse_agent_id(&watched_id),
            "must not cross unload",
        )),
    );
    let (parked, _) = intercepted_payload(&interceptor);
    assert!(matches!(parked, Event::ProviderResponseFinished(_)));

    h.remove_agent(&watched_cid);
    h.handle_extension_event(
        "watch-final-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release final response");

    assert!(
        event_log_events(&h).iter().all(|event| !matches!(
            event,
            Event::AgentMessageReceived(message)
                if message.kind == tau_proto::AgentMessageKind::WatchResponse
                    && message.recipient_id.as_str() == watcher_id
        )),
        "parked final response must not append watch content after unload"
    );
    assert!(h.watchers_for_agent(&watched_id).is_empty());
    h.shutdown().expect("shutdown");
}

/// A checkpoint parked before commit owns its provider-qualified model even if
/// `:model` timing changes the loaded agent before prompt materialization.
#[test]
fn intercepted_inference_checkpoint_pins_materialized_model() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    add_second_test_model(&mut h);
    let cid = ensure_test_user_agent(&mut h);
    let interceptor = connect_test_tool(&mut h, "checkpoint-model-owner");
    h.handle_extension_event(
        "checkpoint-model-owner",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_INFERENCE_DISPATCH_STARTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");

    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("owned by A".to_owned()))
        .expect("dispatch inference");
    let (parked, _) = intercepted_payload(&interceptor);
    let Event::AgentInferenceDispatchStarted(checkpoint) = parked else {
        panic!("checkpoint intercepted");
    };
    assert_eq!(checkpoint.model, Some("echo/model".into()));
    h.agents.get_mut(&cid).expect("agent").model_override = Some("other/model".into());
    h.handle_extension_event(
        "checkpoint-model-owner",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release checkpoint");

    let prompt = read_nth_prompt_created(&h, 0);
    assert_eq!(prompt.agent_prompt_id, checkpoint.agent_prompt_id);
    assert_eq!(prompt.model, checkpoint.model.expect("qualified model"));
    h.shutdown().expect("shutdown");
}

/// If a checkpoint's captured route disappears while interception parks the
/// materialized prompt, commit excludes providers and durably terminalizes it.
#[test]
fn intercepted_inference_checkpoint_fails_before_unroutable_send() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let provider_observer =
        connect_test_client(&mut h, "unowned-provider", tau_proto::ClientKind::Provider);
    h.bus
        .set_subscriptions(
            "unowned-provider",
            Vec::new(),
            vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_CREATED,
            )],
        )
        .expect("subscribe provider observer");
    let interceptor = connect_test_tool(&mut h, "checkpoint-route-owner");
    h.handle_extension_event(
        "checkpoint-route-owner",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_CREATED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("route vanishes".to_owned()))
        .expect("dispatch inference");
    let (parked, _) = intercepted_payload(&interceptor);
    let Event::AgentPromptCreated(prompt) = parked else {
        panic!("materialized prompt intercepted");
    };
    h.provider_model_routes.remove(&prompt.model);
    h.handle_extension_event(
        "checkpoint-route-owner",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release checkpoint");

    assert!(
        !provider_observer
            .lock()
            .expect("provider frames")
            .iter()
            .any(|routed| matches!(
                peel_inner_event(&routed.frame),
                Some(Event::AgentPromptCreated(created))
                    if created.agent_prompt_id == prompt.agent_prompt_id
            )),
        "an unroutable owned prompt must not be broadcast to providers"
    );
    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::ProviderResponseFinished(response)
            if response.agent_prompt_id == prompt.agent_prompt_id
                && response.stop_reason == tau_proto::ProviderStopReason::Error
    )));
    h.shutdown().expect("shutdown");
}

/// If compact-fact storage fails, the full request continuation is destroyed
/// before any provider can observe it.
#[test]
fn intercepted_prompt_start_append_failure_prevents_provider_delivery() {
    let tmp = TempDir::new().expect("tempdir");
    let state_dir = tmp.path().join("state");
    let mut h = echo_harness(&state_dir).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    let provider_observer = connect_test_client(
        &mut h,
        "append-fault-observer",
        tau_proto::ClientKind::Provider,
    );
    h.bus
        .set_subscriptions(
            "append-fault-observer",
            Vec::new(),
            vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_CREATED,
            )],
        )
        .expect("subscribe provider observer");
    let interceptor = connect_test_tool(&mut h, "prompt-start-append-owner");
    h.handle_extension_event(
        "prompt-start-append-owner",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_STARTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");

    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("append must fail".to_owned()))
        .expect("dispatch inference");
    let (parked, _) = intercepted_payload(&interceptor);
    let Event::AgentPromptStarted(started) = parked else {
        panic!("compact prompt fact intercepted");
    };
    let journal_path = state_dir
        .join("agents")
        .join(agent_id.as_str())
        .join("events.cbor");
    let backup_path = journal_path.with_extension("cbor.prompt-start-backup");
    std::fs::rename(&journal_path, &backup_path).expect("park agent journal");
    std::fs::create_dir(&journal_path).expect("reject compact-fact append");
    h.handle_extension_event(
        "prompt-start-append-owner",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release compact fact");
    std::fs::remove_dir(&journal_path).expect("remove append blocker");
    std::fs::rename(&backup_path, &journal_path).expect("restore agent journal");

    assert!(
        !h.pending_prompt_dispatches
            .contains(&started.agent_prompt_id)
    );
    assert!(
        !h.agent_store
            .agent_events(agent_id.as_str())
            .expect("agent events")
            .iter()
            .any(|record| matches!(
                &record.event,
                Event::AgentPromptStarted(value)
                    if value.agent_prompt_id == started.agent_prompt_id
            ))
    );
    assert!(
        !provider_observer
            .lock()
            .expect("provider frames")
            .iter()
            .any(|routed| matches!(
                peel_inner_event(&routed.frame),
                Some(Event::AgentPromptCreated(created))
                    if created.agent_prompt_id == started.agent_prompt_id
            )),
        "a full request must not escape before its compact fact commits"
    );
    h.shutdown().expect("shutdown");
}

/// A parked full request belongs to the exact loaded runtime that materialized
/// it; replacing that runtime invalidates delivery despite stable semantic ids.
#[test]
fn intercepted_prompt_rejects_changed_runtime_incarnation() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let provider_observer =
        connect_test_client(&mut h, "runtime-observer", tau_proto::ClientKind::Provider);
    h.bus
        .set_subscriptions(
            "runtime-observer",
            Vec::new(),
            vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_CREATED,
            )],
        )
        .expect("subscribe provider observer");
    let interceptor = connect_test_tool(&mut h, "runtime-prompt-owner");
    h.handle_extension_event(
        "runtime-prompt-owner",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_CREATED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");

    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("runtime changes".to_owned()))
        .expect("dispatch inference");
    let (parked, _) = intercepted_payload(&interceptor);
    let Event::AgentPromptCreated(prompt) = parked else {
        panic!("full prompt intercepted");
    };
    h.agents.get_mut(&cid).expect("agent").runtime_incarnation += 1;
    h.handle_extension_event(
        "runtime-prompt-owner",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release full prompt");

    assert!(
        !h.pending_prompt_dispatches
            .contains(&prompt.agent_prompt_id)
    );
    assert!(
        !provider_observer
            .lock()
            .expect("provider frames")
            .iter()
            .any(|routed| matches!(
                peel_inner_event(&routed.frame),
                Some(Event::AgentPromptCreated(created))
                    if created.agent_prompt_id == prompt.agent_prompt_id
            )),
        "a stale runtime continuation must not reach any provider"
    );
    h.shutdown().expect("shutdown");
}

fn assert_unload_disposes_parked_prompt(selector: tau_proto::EventName, owner: &str) {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let interceptor = connect_test_tool(&mut h, owner);
    h.handle_extension_event(
        owner,
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(selector)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("park before unload".to_owned()))
        .expect("dispatch inference");
    let (parked, _) = intercepted_payload(&interceptor);
    let prompt_id = match parked {
        Event::AgentPromptStarted(value) => value.agent_prompt_id,
        Event::AgentPromptCreated(value) => value.agent_prompt_id,
        other => panic!("unexpected parked event {}", other.name()),
    };
    assert_eq!(h.current_session_state.token_usage.total.requests, 1);

    h.remove_agent(&cid);

    assert!(!h.pending_prompt_dispatches.contains(&prompt_id));
    assert!(!h.prompt_agents.contains_key(prompt_id.as_str()));
    assert!(!h.prompt_models.contains_key(&prompt_id));
    assert!(!h.prompt_operations.contains_key(&prompt_id));
    assert!(!h.pending_provider_prompts.contains_key(&prompt_id));
    assert_eq!(h.current_session_state.token_usage.total.requests, 0);
    h.shutdown().expect("shutdown");
}

/// Unload disposes bookkeeping while the compact materialization fact is
/// parked.
#[test]
fn unload_disposes_parked_prompt_materialization() {
    assert_unload_disposes_parked_prompt(
        tau_proto::EventName::AGENT_PROMPT_STARTED,
        "materialization-unload-owner",
    );
}

/// Unload also disposes bookkeeping after the compact fact committed but before
/// its transient full request left interception.
#[test]
fn unload_disposes_parked_prompt_delivery() {
    assert_unload_disposes_parked_prompt(
        tau_proto::EventName::AGENT_PROMPT_CREATED,
        "delivery-unload-owner",
    );
}

/// A standalone start parked before commit owns the compact request model even
/// if model selection changes before its post-commit provider dispatch.
#[test]
fn intercepted_compaction_start_pins_materialized_model() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    add_second_test_model(&mut h);
    let cid = ensure_test_user_agent(&mut h);
    {
        let info = h
            .provider_model_info
            .get_mut(&tau_proto::ModelId::from("echo/model"))
            .expect("model");
        info.supports_standalone_compaction = true;
        info.standalone_compaction_threshold = Some(1);
        let agent = h.agents.get_mut(&cid).expect("agent");
        agent.context_input_tokens = Some(1);
        agent.context_usage_head = agent.head;
        agent.context_usage_model = Some("echo/model".into());
    }
    let interceptor = connect_test_tool(&mut h, "compact-model-owner");
    h.handle_extension_event(
        "compact-model-owner",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_STANDALONE_COMPACTION_STARTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");

    assert!(h.schedule_standalone_auto_compaction(&cid));
    let (parked, _) = intercepted_payload(&interceptor);
    let Event::AgentStandaloneCompactionStarted(started) = parked else {
        panic!("compaction start intercepted");
    };
    assert_eq!(started.model, tau_proto::ModelId::from("echo/model"));
    h.agents.get_mut(&cid).expect("agent").model_override = Some("other/model".into());
    h.handle_extension_event(
        "compact-model-owner",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release start");

    let prompt = read_nth_prompt_created(&h, 0);
    assert_eq!(prompt.agent_prompt_id, started.compact_prompt_id);
    assert_eq!(prompt.model, started.model);
    assert_eq!(
        prompt.operation,
        tau_proto::PromptOperation::StandaloneCompaction
    );
    h.shutdown().expect("shutdown");
}

/// A successful standalone compaction binds its continuation to the final
/// steer publication, so interception cannot let the checkpoint overtake and
/// omit that activating input.
#[test]
fn intercepted_compaction_completion_steer_precedes_continuation_checkpoint() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    {
        let info = h
            .provider_model_info
            .get_mut(&tau_proto::ModelId::from("test/model"))
            .expect("model");
        info.supports_standalone_compaction = true;
        info.standalone_compaction_threshold = Some(1);
        let agent = h.agents.get_mut(&cid).expect("agent");
        agent.context_input_tokens = Some(1);
        agent.context_usage_head = agent.head;
        agent.context_usage_model = Some("test/model".into());
    }
    assert!(h.schedule_standalone_auto_compaction_for_activation(
        &cid,
        true,
        Some(tau_proto::AgentHead::Root),
    ));
    let compact_prompt = read_nth_prompt_created(&h, 0);
    let transaction_id = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentStandaloneCompactionStarted(started) => Some(started.transaction_id),
            _ => None,
        })
        .expect("standalone transaction");

    h.agents
        .get_mut(&cid)
        .expect("agent")
        .pending_prompts
        .extend([
            PendingPrompt::user("first steer after compaction".to_owned()),
            PendingPrompt::user("final steer after compaction".to_owned()),
        ]);
    let interceptor = connect_test_tool(&mut h, "completion-steer-owner");
    h.handle_extension_event(
        "completion-steer-owner",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_STEERED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register steer interceptor");

    h.handle_provider_response_finished(provider_text_response(
        &compact_prompt.agent_prompt_id,
        compact_prompt.agent_id.clone(),
        "summary",
    ))
    .expect("accept compact response");
    let (parked, _) = intercepted_payload(&interceptor);
    assert!(matches!(
        parked,
        Event::AgentPromptSteered(tau_proto::AgentPromptSteered { ref text, .. })
            if text == "first steer after compaction"
    ));
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(
                event,
                Event::AgentInferenceDispatchStarted(started)
                    if started.transaction_id.as_ref() == Some(&transaction_id)
            ))
            .count(),
        0,
        "the continuation checkpoint must wait for the exact steer commit"
    );
    assert_eq!(prompt_created_count(&h), 1);
    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: compact_prompt.agent_id.clone(),
            head: tau_proto::AgentHead::Root,
        }),
    );

    h.handle_extension_event(
        "completion-steer-owner",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release first steer");
    assert!(matches!(
        h.pending_intercept.as_ref().map(|pending| &pending.event),
        Some(Event::AgentPromptSteered(tau_proto::AgentPromptSteered { text, .. }))
            if text == "final steer after compaction"
    ));
    assert_eq!(prompt_created_count(&h), 1);
    assert!(!event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::AgentInferenceDispatchStarted(started)
            if started.transaction_id.as_ref() == Some(&transaction_id)
    )));
    h.handle_extension_event(
        "completion-steer-owner",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release final steer");
    let checkpoint = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentInferenceDispatchStarted(started)
                if started.transaction_id.as_ref() == Some(&transaction_id) =>
            {
                Some(started)
            }
            _ => None,
        })
        .expect("continuation checkpoint");
    let tau_proto::AgentHead::Node(final_steer_node) = checkpoint.through else {
        panic!("continuation must end at the final steer node");
    };
    assert!(matches!(
        &default_agent_node(&h, final_steer_node).entry,
        AgentEntry::UserInput { items, .. }
            if items.iter().any(|item| matches!(
                item,
                ContextItem::Message(MessageItem { content, .. })
                    if content.iter().any(|part| matches!(
                        part,
                        ContentPart::Text { text } if text == "final steer after compaction"
                    ))
            ))
    ));
    let continuation = read_nth_prompt_created(&h, 1);
    for expected in [
        "first steer after compaction",
        "final steer after compaction",
    ] {
        assert_eq!(
            continuation
                .context
                .flatten_iter()
                .filter(|item| matches!(
                    item,
                    ContextItem::Message(MessageItem {
                        role: ContextRole::User,
                        content,
                        ..
                    }) if content.iter().any(|part| matches!(
                        part,
                        ContentPart::Text { text } if text == expected
                    ))
                ))
                .count(),
            1
        );
    }
    let events = event_log_events(&h);
    let checkpoint_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::AgentInferenceDispatchStarted(started)
                    if started.transaction_id.as_ref() == Some(&transaction_id)
            )
        })
        .expect("checkpoint index");
    let navigation_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::AgentHeadMoved(moved) if moved.head == tau_proto::AgentHead::Root
            )
        })
        .expect("navigation index");
    assert!(checkpoint_index < navigation_index);
    h.shutdown().expect("shutdown");
}

/// A first-suffix persistence failure drops the interceptor-approved
/// continuation and retains exact retry state for later semantic work.
#[test]
fn rejected_compaction_completion_steer_retries_after_recovery() {
    let tmp = TempDir::new().expect("tempdir");
    let state_dir = tmp.path().join("state");
    let mut h = quiet_provider_harness(&state_dir).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let (agent_id, transaction_id, _, batch_parent) =
        super::lifecycle::seed_restored_compaction_checkpoint(
            &mut h,
            &cid,
            &"test/model".into(),
            "ct-steer-retry",
        );
    h.enqueued_standalone_inference_checkpoints.clear();
    let watcher_cid =
        h.create_durable_user_agent(h.current_session_id.clone(), &h.selected_role.clone());
    let watcher_id = durable_agent_id_for_conversation(&h, &watcher_cid);
    h.set_agent_watch(
        watcher_id.as_str(),
        agent_id.as_str(),
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    let watcher_messages_before = session_agent_message_received_events(&h)
        .into_iter()
        .filter(|message| {
            message.recipient_id == watcher_id
                && message.kind == tau_proto::AgentMessageKind::WatchPrompt
        })
        .count();
    h.agents.get_mut(&cid).expect("agent").activation_dispatch =
        crate::agent::ActivationDispatchState::Running {
            id: transaction_id.clone(),
            cut: tau_proto::AgentHead::Root,
            resume_through: Some(batch_parent),
            model: "test/model".into(),
            branch_generation: 0,
            compact_prompt_id: "ap-compact-steer-retry".into(),
        };
    let retry_prompt =
        PendingPrompt::human_ui_watch_notified("retry exact completion steer".to_owned());
    let final_retry_prompt = PendingPrompt::user("retry final completion steer".to_owned());
    let completion = AgentPublishCompletion::StandaloneContinuation {
        transaction_id: transaction_id.clone(),
        model: "test/model".into(),
        activation_cut: tau_proto::AgentHead::Root,
        batch_parent,
        source: None,
        retry_prompts: vec![retry_prompt.clone(), final_retry_prompt],
        complete_on_commit: false,
        approved_retry_event: None,
    };
    let interceptor = connect_test_tool(&mut h, "retry-replacement-owner");
    h.handle_extension_event(
        "retry-replacement-owner",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_STEERED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register replacement interceptor");
    h.publish_event_for_agent_with_completion(
        &cid,
        None,
        Event::AgentPromptSteered(tau_proto::AgentPromptSteered {
            inference_activation: true,
            submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
            agent_id: agent_id.clone(),
            text: retry_prompt.text.clone(),
            message_class: retry_prompt.message_class,
            internal_kind: retry_prompt.internal_kind(),
            ctx_id: retry_prompt.ctx_id.clone(),
        }),
        Some(completion),
        false,
    );
    let (mut replacement, _) = intercepted_payload(&interceptor);
    let Event::AgentPromptSteered(replaced) = &mut replacement else {
        panic!("steer intercepted");
    };
    replaced.text = "approved replacement steer".to_owned();
    replaced.ctx_id = Some("approved-ctx".to_owned());
    let event_path = state_dir
        .join("agents")
        .join(agent_id.as_str())
        .join("events.cbor");
    let backup_path = event_path.with_extension("cbor.steer-retry-backup");
    std::fs::rename(&event_path, &backup_path).expect("park agent journal");
    std::fs::create_dir(&event_path).expect("reject final steer append");
    h.handle_extension_event(
        "retry-replacement-owner",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(replacement))),
        })),
    )
    .expect("release approved replacement into append failure");
    assert!(
        h.pending_agent_publish_completions.contains_key(&cid),
        "clean storage failure retains the exact retry envelope"
    );
    assert!(matches!(
        h.agents[&cid].activation_dispatch,
        crate::agent::ActivationDispatchState::Running { .. }
    ));
    assert_eq!(prompt_created_count(&h), 0);
    assert_eq!(
        session_agent_message_received_events(&h)
            .into_iter()
            .filter(|message| {
                message.recipient_id == watcher_id
                    && message.kind == tau_proto::AgentMessageKind::WatchPrompt
            })
            .count(),
        watcher_messages_before,
        "rejected steer must not fan out a phantom watcher input"
    );
    std::fs::remove_dir(&event_path).expect("remove append blocker");
    std::fs::rename(&backup_path, &event_path).expect("restore agent journal");

    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id,
            head: batch_parent,
        }),
    );
    assert!(h.pending_intercept.is_some());
    h.handle_extension_event(
        "retry-replacement-owner",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release retained retry");
    assert!(h.pending_intercept.is_none());
    assert!(!h.pending_agent_publish_completions.contains_key(&cid));
    assert_eq!(prompt_created_count(&h), 2);
}

fn session_agent_message_received_events(h: &Harness) -> Vec<tau_proto::AgentMessageReceived> {
    event_log_events(h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentMessageReceived(message) => Some(message),
            _ => None,
        })
        .collect()
}

/// A completion-owned intercepted steer and an independent activation queued
/// behind it retain disjoint envelope ownership; the steer cannot consume the
/// later activation's watermark.
#[test]
fn completion_steer_cannot_steal_queued_activation_ownership() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let (agent_id, transaction_id, _, batch_parent) =
        super::lifecycle::seed_restored_compaction_checkpoint(
            &mut h,
            &cid,
            &"test/model".into(),
            "ct-envelope-ownership",
        );
    h.enqueued_standalone_inference_checkpoints.clear();
    h.agents.get_mut(&cid).expect("agent").activation_dispatch =
        crate::agent::ActivationDispatchState::Running {
            id: transaction_id.clone(),
            cut: tau_proto::AgentHead::Root,
            resume_through: Some(batch_parent),
            model: "test/model".into(),
            branch_generation: 0,
            compact_prompt_id: "ap-envelope-compact".into(),
        };
    let _interceptor = connect_test_tool(&mut h, "completion-envelope-owner");
    h.handle_extension_event(
        "completion-envelope-owner",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_STEERED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    let completion_prompt = PendingPrompt::user("completion-owned steer".to_owned());
    h.publish_event_for_agent_with_completion(
        &cid,
        None,
        Event::AgentPromptSteered(tau_proto::AgentPromptSteered {
            inference_activation: true,
            submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
            agent_id: agent_id.clone(),
            text: completion_prompt.text.clone(),
            message_class: completion_prompt.message_class,
            internal_kind: None,
            ctx_id: None,
        }),
        Some(AgentPublishCompletion::StandaloneContinuation {
            transaction_id,
            model: "test/model".into(),
            activation_cut: tau_proto::AgentHead::Root,
            batch_parent,
            source: None,
            retry_prompts: vec![completion_prompt],
            complete_on_commit: true,
            approved_retry_event: None,
        }),
        false,
    );
    h.publish_for_agent(
        &cid,
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            inference_activation: true,
            agent_id,
            text: "independent queued activation".to_owned(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: tau_proto::PromptSubmissionSource::HumanUi,
            display_name: None,
            ctx_id: None,
        }),
    );
    assert!(h.pending_intercept.as_ref().is_some_and(|pending| {
        pending
            .sync_head_for
            .as_ref()
            .is_some_and(|sync| sync.suppress_activation_dispatch && sync.completion().is_some())
    }));

    h.handle_extension_event(
        "completion-envelope-owner",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release completion steer");
    let independent_node = default_agent_tree(&h)
        .nodes()
        .iter()
        .find_map(|node| {
            matches!(
                &node.entry,
                AgentEntry::UserInput { items, .. }
                    if items.iter().any(|item| matches!(
                        item,
                        ContextItem::Message(MessageItem { content, .. })
                            if content.iter().any(|part| matches!(
                                part,
                                ContentPart::Text { text }
                                    if text == "independent queued activation"
                            ))
                    ))
            )
            .then_some(node.id)
        })
        .expect("independent activation node");
    assert_eq!(h.pending_publish_idle_dispatches.len(), 1);
    assert_eq!(
        h.pending_publish_idle_dispatches[0].activation_through,
        Some(tau_proto::AgentHead::Node(independent_node))
    );
}

/// Agent-local transaction-id collisions cannot purge another agent's deferred
/// completion batch.
#[test]
fn completion_batch_purge_is_scoped_by_agent_and_transaction() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let cid_a = ensure_test_user_agent(&mut h);
    let cid_b = h.create_durable_user_agent(h.current_session_id.clone(), &h.selected_role.clone());
    let agent_a = durable_agent_id_for_conversation(&h, &cid_a);
    let agent_b = durable_agent_id_for_conversation(&h, &cid_b);
    let _interceptor = connect_test_tool(&mut h, "batch-collision-owner");
    h.handle_extension_event(
        "batch-collision-owner",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register queue blocker");
    h.publish_event(None, draft_event("hold completion batches"));
    let transaction_id = tau_proto::CompactionTransactionId::parse("ct-0").expect("transaction");
    let completion = |text: &str| AgentPublishCompletion::StandaloneContinuation {
        transaction_id: transaction_id.clone(),
        model: "test/model".into(),
        activation_cut: tau_proto::AgentHead::Root,
        batch_parent: tau_proto::AgentHead::Root,
        source: None,
        retry_prompts: vec![PendingPrompt::user(text.to_owned())],
        complete_on_commit: true,
        approved_retry_event: None,
    };
    let completion_a = completion("agent A completion");
    for (cid, agent_id, text, owned_completion) in [
        (
            &cid_a,
            agent_a.clone(),
            "agent A completion",
            completion_a.clone(),
        ),
        (
            &cid_b,
            agent_b.clone(),
            "agent B completion",
            completion("agent B completion"),
        ),
    ] {
        h.publish_event_for_agent_with_completion(
            cid,
            None,
            Event::AgentPromptSteered(tau_proto::AgentPromptSteered {
                inference_activation: true,
                submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
                agent_id,
                text: text.to_owned(),
                message_class: tau_proto::PromptMessageClass::User,
                internal_kind: None,
                ctx_id: None,
            }),
            Some(owned_completion),
            false,
        );
    }
    h.discard_deferred_agent_publish_batch(&cid_a, &completion_a);
    assert!(h.deferred_publishes.iter().all(|publish| {
        !matches!(
            publish.event(),
            Event::AgentPromptSteered(steered) if steered.agent_id == agent_a
        )
    }));
    assert!(h.deferred_publishes.iter().any(|publish| {
        matches!(
            publish.event(),
            Event::AgentPromptSteered(steered) if steered.agent_id == agent_b
        )
    }));
}

/// Canceling an unloading agent's intercepted ordinary inference checkpoint
/// preserves and resumes another agent's durable publication from the global
/// deferred FIFO.
#[test]
fn unloading_intercepted_checkpoint_preserves_other_agent_deferred_publish() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let cid_a = ensure_test_user_agent(&mut h);
    let cid_b = h.create_durable_user_agent(h.current_session_id.clone(), &h.selected_role.clone());
    let agent_a = durable_agent_id_for_conversation(&h, &cid_a);
    let agent_b = durable_agent_id_for_conversation(&h, &cid_b);
    let _interceptor = connect_test_tool(&mut h, "unload-completion-owner");
    h.handle_extension_event(
        "unload-completion-owner",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_INFERENCE_DISPATCH_STARTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register completion interceptor");
    let agent_prompt_id = tau_proto::AgentPromptId::from("ap-unload-a");
    h.agents
        .get_mut(&cid_a)
        .expect("agent A")
        .activation_dispatch = crate::agent::ActivationDispatchState::AwaitingCheckpoint {
        owner: crate::agent::InferenceCheckpointOwner::Inference,
        agent_prompt_id: agent_prompt_id.clone(),
        through: tau_proto::AgentHead::Root,
        dispatch: crate::agent::InferenceDispatchOwnership {
            model: "test/model".into(),
            operation: tau_proto::PromptOperation::Inference,
            activation_cut: tau_proto::AgentHead::Root,
        },
    };
    let old_checkpoint =
        Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
            agent_id: agent_a,
            transaction_id: None,
            agent_prompt_id,
            through: tau_proto::AgentHead::Root,
            model: Some("test/model".into()),
            operation: Some(tau_proto::PromptOperation::Inference),
            activation_cut: Some(tau_proto::AgentHead::Root),
        });
    h.publish_for_agent(&cid_a, old_checkpoint.clone());
    assert!(h.pending_intercept.is_some());
    h.publish_for_agent(
        &cid_b,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: agent_b.clone(),
            head: tau_proto::AgentHead::Root,
        }),
    );
    assert_eq!(h.deferred_publishes.len(), 1);

    h.remove_agent(&cid_a);

    assert!(h.pending_intercept.is_none());
    assert!(h.deferred_publishes.is_empty());
    assert!(h.agents.contains_key(&cid_b));
    assert!(!event_log_events(&h).contains(&old_checkpoint));
    assert_eq!(
        h.agent_store
            .agent(agent_b.as_str())
            .expect("other agent tree")
            .head(),
        None,
        "other agent's deferred head move must commit instead of being dropped"
    );
    assert!(event_log_events(&h).into_iter().any(|event| {
        matches!(
            event,
            Event::AgentHeadMoved(moved)
                if moved.agent_id == agent_b && moved.head == tau_proto::AgentHead::Root
        )
    }));
    h.handle_extension_event(
        "unload-completion-owner",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("replace suspended interceptor registration");
    let surviving_event = draft_event("surviving post-unload event");
    h.publish_event(None, surviving_event.clone());
    assert!(h.pending_intercept.is_none());
    h.handle_extension_event(
        "unload-completion-owner",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("stale pre-unload reply is ignored");
    assert!(event_log_events(&h).contains(&surviving_event));
    assert!(!event_log_events(&h).contains(&old_checkpoint));
}

/// Disconnect clears destructive-cancellation suspension with the old
/// registration, so a reconnected interceptor can register and immediately
/// intercept without first supplying the old connection's stale reply.
#[test]
fn suspended_interceptor_disconnect_reconnects_unsuspended() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    let connection_id = "suspended-reconnect";
    let _interceptor = connect_test_tool(&mut h, connection_id);
    h.handle_extension_event(
        connection_id,
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_INFERENCE_DISPATCH_STARTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register checkpoint interceptor");
    let agent_prompt_id = tau_proto::AgentPromptId::from("ap-suspended-reconnect");
    h.agents.get_mut(&cid).expect("agent").activation_dispatch =
        crate::agent::ActivationDispatchState::AwaitingCheckpoint {
            owner: crate::agent::InferenceCheckpointOwner::Inference,
            agent_prompt_id: agent_prompt_id.clone(),
            through: tau_proto::AgentHead::Root,
            dispatch: crate::agent::InferenceDispatchOwnership {
                model: "test/model".into(),
                operation: tau_proto::PromptOperation::Inference,
                activation_cut: tau_proto::AgentHead::Root,
            },
        };
    h.publish_for_agent(
        &cid,
        Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
            agent_id,
            transaction_id: None,
            agent_prompt_id,
            through: tau_proto::AgentHead::Root,
            model: Some("test/model".into()),
            operation: Some(tau_proto::PromptOperation::Inference),
            activation_cut: Some(tau_proto::AgentHead::Root),
        }),
    );
    assert!(h.pending_intercept.is_some());

    h.remove_agent(&cid);
    let typed_connection_id = tau_proto::ConnectionId::from(connection_id);
    assert!(
        h.suspended_interceptor_connections
            .contains(&typed_connection_id)
    );
    h.handle_disconnect(connection_id);
    assert!(
        !h.suspended_interceptor_connections
            .contains(&typed_connection_id)
    );

    let reconnected = connect_test_tool(&mut h, connection_id);
    h.handle_extension_event(
        connection_id,
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register reconnected interceptor");
    let after_reconnect = draft_event("intercept after reconnect");
    h.publish_event(None, after_reconnect.clone());

    let (intercepted, _) = intercepted_payload(&reconnected);
    assert_eq!(intercepted, after_reconnect);
    assert_eq!(
        h.pending_intercept
            .as_ref()
            .map(|pending| pending.conn_id.as_str()),
        Some(connection_id)
    );
    h.handle_extension_event(
        connection_id,
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release reconnected interception");
    assert!(event_log_events(&h).contains(&after_reconnect));
}

/// Rollover retains a deferred default-mandatory standalone cancellation even
/// when its caller did not set the explicit `must_pass` envelope bit.
#[test]
fn rollover_commits_deferred_default_mandatory_compaction_terminal() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    let transaction_id =
        tau_proto::CompactionTransactionId::parse("ct-rollover-terminal").expect("transaction");
    h.publish_for_agent(
        &cid,
        Event::AgentStandaloneCompactionStarted(tau_proto::AgentStandaloneCompactionStarted {
            compact_prompt_id: "ap-rollover-terminal".into(),
            operation: tau_proto::PromptOperation::StandaloneCompaction,
            agent_id: agent_id.clone(),
            transaction_id: transaction_id.clone(),
            cut: tau_proto::AgentHead::Root,
            resume_through: None,
            model: "test/model".into(),
            originator: tau_proto::PromptOriginator::User,
            supersedes: None,
            trigger: tau_proto::StandaloneCompactionTrigger::Manual,
        }),
    );
    let _interceptor = connect_test_tool(&mut h, "rollover-terminal-blocker");
    h.handle_extension_event(
        "rollover-terminal-blocker",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register rollover blocker");
    h.publish_event(None, draft_event("block rollover terminal"));
    h.publish_for_agent(
        &cid,
        Event::AgentStandaloneCompactionFailed(tau_proto::AgentStandaloneCompactionFailed {
            agent_id,
            transaction_id: transaction_id.clone(),
            cut: tau_proto::AgentHead::Root,
            reason: tau_proto::StandaloneCompactionFailureReason::Cancelled,
            resume_through: None,
        }),
    );
    assert!(h.pending_intercept.is_some());
    assert!(!event_log_events(&h).into_iter().any(|event| {
        matches!(
            event,
            Event::AgentStandaloneCompactionFailed(failed)
                if failed.transaction_id == transaction_id
        )
    }));

    h.switch_session("replacement".into(), tau_proto::SessionStartReason::New)
        .expect("switch session");

    assert!(event_log_events(&h).into_iter().any(|event| {
        matches!(
            event,
            Event::AgentStandaloneCompactionFailed(failed)
                if failed.transaction_id == transaction_id
                    && failed.reason
                        == tau_proto::StandaloneCompactionFailureReason::Cancelled
        )
    }));
}

/// A replay-drift claim parked by interception must remain uniquely pending;
/// after commit its suppression is consumed and the correlated failure blocks
/// recovery without ever creating a compact provider prompt.
#[test]
fn intercepted_reactive_drift_terminalization_never_dispatches() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("overflow".to_owned()))
        .expect("dispatch inference");
    let inference = read_nth_prompt_created(&h, 0);
    let checkpoint = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentInferenceDispatchStarted(checkpoint)
                if checkpoint.agent_prompt_id == inference.agent_prompt_id =>
            {
                Some(checkpoint)
            }
            _ => None,
        })
        .expect("checkpoint");
    let mut planned = context_overflow_response(&inference);
    planned.recovery_disposition = tau_proto::ContextRecoveryDisposition::ReactiveCompactionPlanned;
    h.publish_for_agent(&cid, Event::ProviderResponseFinished(planned));
    h.agents.get_mut(&cid).expect("agent").activation_dispatch =
        crate::agent::ActivationDispatchState::ContextRecoveryPending {
            checkpoint: checkpoint.clone(),
        };

    let interceptor = connect_test_tool(&mut h, "reactive-start-interceptor");
    h.handle_extension_event(
        "reactive-start-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_STANDALONE_COMPACTION_STARTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    h.reconcile_pending_context_recoveries(false);
    assert!(matches!(
        h.agents[&cid].activation_dispatch,
        crate::agent::ActivationDispatchState::ContextRecoveryClaimPending { .. }
    ));
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(
                event,
                Event::AgentPromptCreated(prompt)
                    if prompt.operation == tau_proto::PromptOperation::StandaloneCompaction
            ))
            .count(),
        0
    );
    let _ = intercepted_payload(&interceptor);
    h.handle_extension_event(
        "reactive-start-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release start");
    assert!(h.suppressed_compaction_dispatches.is_empty());
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(
                event,
                Event::AgentPromptCreated(prompt)
                    if prompt.operation == tau_proto::PromptOperation::StandaloneCompaction
            ))
            .count(),
        0
    );
    assert!(matches!(
        h.agents[&cid].activation_dispatch,
        crate::agent::ActivationDispatchState::Blocked { .. }
    ));
    h.shutdown().expect("shutdown");
}

/// Real interception replies cannot flip the harness-owned activation bit in
/// either direction on any canonical transcript fact.
#[test]
fn interception_rejects_activation_bit_forgery_for_all_canonical_facts() {
    for inference_activation in [false, true] {
        let agent_id = tau_proto::AgentId::parse("main").expect("agent id");
        let cases = [
            (
                tau_proto::EventName::AGENT_PROMPT_SUBMITTED,
                Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                    inference_activation,
                    agent_id: agent_id.clone(),
                    text: "submitted".to_owned(),
                    message_class: tau_proto::PromptMessageClass::User,
                    internal_kind: None,
                    originator: tau_proto::PromptOriginator::User,
                    submission_source: tau_proto::PromptSubmissionSource::HumanUi,
                    display_name: None,
                    ctx_id: None,
                }),
            ),
            (
                tau_proto::EventName::AGENT_USER_MESSAGE_INJECTED,
                Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
                    inference_activation,
                    agent_id: agent_id.clone(),
                    text: "injected".to_owned(),
                    message_class: tau_proto::PromptMessageClass::Internal,
                }),
            ),
            (
                tau_proto::EventName::AGENT_PROMPT_STEERED,
                Event::AgentPromptSteered(tau_proto::AgentPromptSteered {
                    inference_activation,
                    submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
                    agent_id,
                    text: "steered".to_owned(),
                    message_class: tau_proto::PromptMessageClass::User,
                    internal_kind: None,
                    ctx_id: None,
                }),
            ),
        ];

        for (event_name, original) in cases {
            let tmp = TempDir::new().expect("tempdir");
            let mut h = echo_harness(tmp.path()).expect("harness");
            let cid = ensure_test_user_agent(&mut h);
            let _interceptor = connect_test_tool(&mut h, "activation-rewriter");
            h.handle_extension_event(
                "activation-rewriter",
                TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                    selectors: vec![EventSelector::Exact(event_name)],
                    priority: InterceptionPriority::new(0),
                })),
            )
            .expect("intercept registration");

            h.publish_for_agent(&cid, original.clone());
            let mut replacement = original.clone();
            match &mut replacement {
                Event::AgentPromptSubmitted(prompt) => {
                    prompt.inference_activation = !inference_activation;
                }
                Event::AgentUserMessageInjected(prompt) => {
                    prompt.inference_activation = !inference_activation;
                }
                Event::AgentPromptSteered(prompt) => {
                    prompt.inference_activation = !inference_activation;
                }
                _ => unreachable!(),
            }
            h.handle_extension_event(
                "activation-rewriter",
                TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                    action: InterceptAction::Pass(Some(Box::new(replacement.clone()))),
                })),
            )
            .expect("intercept reply");

            let events = event_log_events(&h);
            assert!(events.contains(&original));
            assert!(!events.contains(&replacement));
        }
    }
}

/// Interceptors may rewrite ordinary steered text but cannot change the
/// harness-stamped provenance that selects provider presentation.
#[test]
fn interception_rejects_steered_submission_source_forgery() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let interceptor = connect_test_tool(&mut h, "steered-source-rewriter");
    h.handle_extension_event(
        "steered-source-rewriter",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_STEERED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let original = Event::AgentPromptSteered(tau_proto::AgentPromptSteered {
        inference_activation: true,
        submission_source: tau_proto::PromptSubmissionSource::HumanUi,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        text: "steered".to_owned(),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        ctx_id: None,
    });

    h.publish_for_agent(&cid, original.clone());
    let mut replacement = original.clone();
    let Event::AgentPromptSteered(prompt) = &mut replacement else {
        unreachable!()
    };
    prompt.submission_source = tau_proto::PromptSubmissionSource::HarnessInternal;
    h.handle_extension_event(
        "steered-source-rewriter",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(replacement.clone()))),
        })),
    )
    .expect("intercept reply");

    let events = event_log_events(&h);
    assert!(events.contains(&original));
    assert!(!events.contains(&replacement));
    drop(interceptor);
    h.shutdown().expect("shutdown");
}

/// Interceptors cannot add or remove the harness-owned context-alert tag, and a
/// tagged alert keeps its configured text on both durable prompt shapes.
#[test]
fn interception_preserves_context_alert_tag_and_text() {
    let agent_id = tau_proto::AgentId::parse("main").expect("agent id");
    let tagged_cases = [
        (
            tau_proto::EventName::AGENT_PROMPT_SUBMITTED,
            Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                inference_activation: true,
                agent_id: agent_id.clone(),
                text: "configured submitted alert".to_owned(),
                message_class: tau_proto::PromptMessageClass::Internal,
                internal_kind: Some(tau_proto::InternalPromptKind::ContextSizeAlert),
                originator: tau_proto::PromptOriginator::User,
                submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
                display_name: None,
                ctx_id: None,
            }),
        ),
        (
            tau_proto::EventName::AGENT_PROMPT_STEERED,
            Event::AgentPromptSteered(tau_proto::AgentPromptSteered {
                inference_activation: true,
                submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
                agent_id,
                text: "configured steered alert".to_owned(),
                message_class: tau_proto::PromptMessageClass::Internal,
                internal_kind: Some(tau_proto::InternalPromptKind::ContextSizeAlert),
                ctx_id: None,
            }),
        ),
    ];

    for (event_name, tagged) in tagged_cases {
        let mut removed_tag = tagged.clone();
        let mut rewritten_text = tagged.clone();
        let mut untagged = tagged.clone();
        match &mut removed_tag {
            Event::AgentPromptSubmitted(prompt) => prompt.internal_kind = None,
            Event::AgentPromptSteered(prompt) => prompt.internal_kind = None,
            _ => unreachable!(),
        }
        match &mut rewritten_text {
            Event::AgentPromptSubmitted(prompt) => prompt.text = "rewritten alert".to_owned(),
            Event::AgentPromptSteered(prompt) => prompt.text = "rewritten alert".to_owned(),
            _ => unreachable!(),
        }
        match &mut untagged {
            Event::AgentPromptSubmitted(prompt) => prompt.internal_kind = None,
            Event::AgentPromptSteered(prompt) => prompt.internal_kind = None,
            _ => unreachable!(),
        }
        let mut rewritten_untagged = untagged.clone();
        match &mut rewritten_untagged {
            Event::AgentPromptSubmitted(prompt) => {
                prompt.text = "rewritten untagged prompt".to_owned();
            }
            Event::AgentPromptSteered(prompt) => {
                prompt.text = "rewritten untagged prompt".to_owned();
            }
            _ => unreachable!(),
        }
        let mut forged_tag = untagged.clone();
        match &mut forged_tag {
            Event::AgentPromptSubmitted(prompt) => {
                prompt.internal_kind = Some(tau_proto::InternalPromptKind::ContextSizeAlert);
            }
            Event::AgentPromptSteered(prompt) => {
                prompt.internal_kind = Some(tau_proto::InternalPromptKind::ContextSizeAlert);
            }
            _ => unreachable!(),
        }

        for (original, replacement) in [
            (tagged.clone(), removed_tag),
            (tagged.clone(), rewritten_text),
            (untagged.clone(), forged_tag),
        ] {
            let tmp = TempDir::new().expect("tempdir");
            let mut h = echo_harness(tmp.path()).expect("harness");
            let cid = ensure_test_user_agent(&mut h);
            let _interceptor = connect_test_tool(&mut h, "context-alert-rewriter");
            h.handle_extension_event(
                "context-alert-rewriter",
                TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                    selectors: vec![EventSelector::Exact(event_name.clone())],
                    priority: InterceptionPriority::new(0),
                })),
            )
            .expect("intercept registration");

            h.publish_for_agent(&cid, original.clone());
            h.handle_extension_event(
                "context-alert-rewriter",
                TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                    action: InterceptAction::Pass(Some(Box::new(replacement.clone()))),
                })),
            )
            .expect("intercept reply");

            let events = event_log_events(&h);
            assert!(events.contains(&original));
            assert!(!events.contains(&replacement));
        }

        let tmp = TempDir::new().expect("tempdir");
        let mut h = echo_harness(tmp.path()).expect("harness");
        let cid = ensure_test_user_agent(&mut h);
        let _interceptor = connect_test_tool(&mut h, "ordinary-prompt-rewriter");
        h.handle_extension_event(
            "ordinary-prompt-rewriter",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(event_name)],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("intercept registration");

        h.publish_for_agent(&cid, untagged.clone());
        h.handle_extension_event(
            "ordinary-prompt-rewriter",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(Some(Box::new(rewritten_untagged.clone()))),
            })),
        )
        .expect("intercept reply");

        let events = event_log_events(&h);
        assert!(!events.contains(&untagged));
        assert!(events.contains(&rewritten_untagged));
    }
}

/// Sink that rejects intercepted frames to exercise failed-delivery recovery.
struct FailingInterceptSink;

impl ConnectionSink for FailingInterceptSink {
    fn send(&mut self, _event: RoutedFrame) -> Result<(), ConnectionSendError> {
        Err(ConnectionSendError::new("test sink closed"))
    }
}

fn connect_named_test_tool(
    h: &mut Harness,
    connection_id: &str,
    component_name: &str,
) -> Arc<Mutex<Vec<RoutedFrame>>> {
    let events = Arc::new(Mutex::new(Vec::new()));
    h.bus.connect(Connection::new(
        ConnectionMetadata {
            id: connection_id.into(),
            name: component_name.to_owned(),
            kind: tau_proto::ClientKind::Tool,
            origin: ConnectionOrigin::InMemory,
        },
        Box::new(TestSink {
            events: Arc::clone(&events),
        }),
    ));
    events
}

fn connect_failing_test_tool(h: &mut Harness, name: &str) {
    h.bus.connect(Connection::new(
        ConnectionMetadata {
            id: name.into(),
            name: name.to_owned(),
            kind: tau_proto::ClientKind::Tool,
            origin: ConnectionOrigin::InMemory,
        },
        Box::new(FailingInterceptSink),
    ));
}

#[test]
fn interception_exact_selector_intercepts_before_log() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let interceptor = connect_test_tool(&mut h, "interceptor");
    let start_seq = h.event_log.next_seq();

    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let after_registration_seq = h.event_log.next_seq();

    h.publish_event(None, draft_event("held"));

    let (event, persist) = intercepted_payload(&interceptor);
    assert_eq!(event, draft_event("held"));
    assert!(!persist, "UiPromptDraft defaults to `persist=false`");
    assert_eq!(h.event_log.next_seq(), after_registration_seq);
    assert!(after_registration_seq.get() < start_seq.get() + 2);
}

#[test]
fn interception_drop_prevents_final_delivery() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let after_registration_seq = h.event_log.next_seq();

    // UiPromptDraft is not on the must-pass list, so an explicit Drop
    // really does drop it.
    h.publish_event(None, draft_event("dropped"));
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop reply");

    assert_eq!(h.event_log.next_seq(), after_registration_seq);
}

#[test]
fn interception_pass_through_reaches_log_after_last_interceptor() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let after_registration_seq = h.event_log.next_seq();

    h.publish_event(None, draft_event("released"));
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("pass reply");

    let entry = h
        .event_log
        .get_next_from(after_registration_seq)
        .expect("released event in log");
    assert_eq!(entry.event, draft_event("released"));
}

#[test]
fn interception_reply_can_modify_event() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let after_registration_seq = h.event_log.next_seq();

    h.publish_event(None, draft_event("original"));
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(draft_event("modified")))),
        })),
    )
    .expect("modifying reply");

    let entry = h
        .event_log
        .get_next_from(after_registration_seq)
        .expect("modified event in log");
    assert_eq!(entry.event, draft_event("modified"));
}

#[test]
fn interception_cannot_modify_mandatory_harness_notice() {
    // Mandatory harness diagnostics include extension config parse failures.
    // Interceptors may observe them, but must not be able to blank or downgrade
    // the message and recreate the same silent-fallback failure for live UIs.
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let after_registration_seq = h.event_log.next_seq();

    h.emit_info_important("extension core-shell rejected its config");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(Event::HarnessNotice(
                tau_proto::HarnessNotice {
                    kind: "test.info".to_owned(),
                    message: String::new(),
                    level: tau_proto::NoticeLevel::Info,
                    always_show: false,
                },
            )))),
        })),
    )
    .expect("mutating reply");

    let entry = h
        .event_log
        .get_next_from(after_registration_seq)
        .expect("important info in log");
    assert!(matches!(
        entry.event,
        Event::HarnessNotice(info)
            if info.level == tau_proto::NoticeLevel::Warning
                && info.message == "extension core-shell rejected its config"
    ));
}

#[test]
fn interception_cannot_modify_critical_harness_notice() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let after_registration_seq = h.event_log.next_seq();

    h.emit_notice(
        "test.critical",
        tau_proto::NoticeLevel::Critical,
        true,
        "critical failure",
    );
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(Event::HarnessNotice(
                tau_proto::HarnessNotice {
                    kind: "test.info".to_owned(),
                    message: "downgraded".to_owned(),
                    level: tau_proto::NoticeLevel::Info,
                    always_show: false,
                },
            )))),
        })),
    )
    .expect("mutating reply");

    let entry = h
        .event_log
        .get_next_from(after_registration_seq)
        .expect("critical notice in log");
    assert!(matches!(
        entry.event,
        Event::HarnessNotice(info)
            if info.level == tau_proto::NoticeLevel::Critical
                && info.kind == "test.critical"
                && info.always_show
                && info.message == "critical failure"
    ));
}

#[test]
fn interception_cannot_drop_critical_harness_notice() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let after_registration_seq = h.event_log.next_seq();

    h.emit_notice(
        "test.critical",
        tau_proto::NoticeLevel::Critical,
        true,
        "critical failure",
    );
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop reply");

    let entry = h
        .event_log
        .get_next_from(after_registration_seq)
        .expect("critical notice in log");
    assert!(matches!(
        entry.event,
        Event::HarnessNotice(info)
            if info.level == tau_proto::NoticeLevel::Critical
                && info.kind == "test.critical"
                && info.always_show
                && info.message == "critical failure"
    ));
}

#[test]
fn interception_cannot_escalate_non_mandatory_harness_notice() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let after_registration_seq = h.event_log.next_seq();

    h.emit_info("ordinary notice");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(Event::HarnessNotice(
                tau_proto::HarnessNotice {
                    kind: tau_proto::notice_kind::EXTENSION_CONFIG_ERROR.to_owned(),
                    message: "edited message".to_owned(),
                    level: tau_proto::NoticeLevel::Critical,
                    always_show: true,
                },
            )))),
        })),
    )
    .expect("mutating reply");

    let entry = h
        .event_log
        .get_next_from(after_registration_seq)
        .expect("notice in log");
    assert!(matches!(
        entry.event,
        Event::HarnessNotice(info)
            if info.level == tau_proto::NoticeLevel::Info
                && info.kind == tau_proto::notice_kind::HARNESS_NOTICE
                && !info.always_show
                && info.message == "edited message"
    ));
}

#[test]
fn interception_priority_orders_lower_values_first() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let low = connect_test_tool(&mut h, "low");
    let high = connect_test_tool(&mut h, "high");
    for (name, priority) in [("low", 10), ("high", 0)] {
        h.handle_extension_event(
            name,
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
                priority: InterceptionPriority::new(priority),
            })),
        )
        .expect("intercept registration");
    }

    h.publish_event(None, draft_event("ordered"));

    assert!(
        high.lock()
            .expect("high events")
            .iter()
            .any(|event| matches!(event.frame, HarnessOutputMessage::InterceptRequest(_)))
    );
    assert!(
        !low.lock()
            .expect("low events")
            .iter()
            .any(|event| matches!(event.frame, HarnessOutputMessage::InterceptRequest(_)))
    );
}

#[test]
fn interception_same_priority_orders_by_component_name_and_redelivery_continues() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let alpha = connect_test_tool(&mut h, "alpha");
    let beta = connect_test_tool(&mut h, "beta");
    for name in ["beta", "alpha"] {
        h.handle_extension_event(
            name,
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("intercept registration");
    }

    h.publish_event(None, draft_event("chain"));
    assert!(
        alpha
            .lock()
            .expect("alpha events")
            .iter()
            .any(|event| matches!(event.frame, HarnessOutputMessage::InterceptRequest(_)))
    );
    assert!(
        !beta
            .lock()
            .expect("beta events")
            .iter()
            .any(|event| matches!(event.frame, HarnessOutputMessage::InterceptRequest(_)))
    );

    h.handle_extension_event(
        "alpha",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("alpha pass");
    assert!(
        beta.lock()
            .expect("beta events")
            .iter()
            .any(|event| matches!(event.frame, HarnessOutputMessage::InterceptRequest(_)))
    );
}

#[test]
fn interception_exact_beats_prefix_even_with_lower_prefix_priority() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let exact = connect_test_tool(&mut h, "exact");
    let prefix = connect_test_tool(&mut h, "prefix");
    h.handle_extension_event(
        "prefix",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Prefix("ui".to_owned())],
            priority: InterceptionPriority::new(-100),
        })),
    )
    .expect("prefix registration");
    h.handle_extension_event(
        "exact",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(100),
        })),
    )
    .expect("exact registration");

    h.publish_event(None, draft_event("exact"));

    assert!(
        exact
            .lock()
            .expect("exact events")
            .iter()
            .any(|event| matches!(event.frame, HarnessOutputMessage::InterceptRequest(_)))
    );
    assert!(
        !prefix
            .lock()
            .expect("prefix events")
            .iter()
            .any(|event| matches!(event.frame, HarnessOutputMessage::InterceptRequest(_)))
    );
}

#[test]
fn interception_pass_advances_past_responding_interceptor() {
    // With the new InterceptReply protocol the cursor lives on the
    // harness side and always advances strictly past the interceptor
    // that just replied. The old "Emit with interception: None
    // restarts" pattern is gone — a Pass(None) reply does *not* loop
    // the event back through the same interceptor.
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");

    h.publish_event(None, draft_event("once"));
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("pass reply");

    let count = interceptor
        .lock()
        .expect("events")
        .iter()
        .filter(|event| matches!(event.frame, HarnessOutputMessage::InterceptRequest(_)))
        .count();
    assert_eq!(
        count, 1,
        "pass-through must not re-trigger the same interceptor"
    );
}

/// Ensures same-priority cursor advancement uses full registration order rather
/// than connection-id order alone.
#[test]
fn interception_cursor_uses_full_registration_order() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let first = connect_named_test_tool(&mut h, "z-conn", "alpha-component");
    let second = connect_named_test_tool(&mut h, "a-conn", "beta-component");

    for connection_id in ["z-conn", "a-conn"] {
        h.handle_extension_event(
            connection_id,
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("intercept registration");
    }

    h.publish_event(None, draft_event("ordered"));
    h.handle_extension_event(
        "z-conn",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("first pass reply");

    assert_eq!(
        first
            .lock()
            .expect("first events")
            .iter()
            .filter(|event| matches!(event.frame, HarnessOutputMessage::InterceptRequest(_)))
            .count(),
        1
    );
    assert_eq!(
        second
            .lock()
            .expect("second events")
            .iter()
            .filter(|event| matches!(event.frame, HarnessOutputMessage::InterceptRequest(_)))
            .count(),
        1,
        "same-priority cursor must follow component-name ordering, not connection-id ordering"
    );
}

#[test]
fn interception_defers_subsequent_publishes_until_reply() {
    // Regression for the "Ready" loop: while one publish is parked
    // waiting on an InterceptReply, the harness must defer any
    // subsequent publishes rather than commit them out of order.
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    // Publish two: the first parks in interception (matches the
    // selector); the second does NOT match and so would, in the
    // buggy world, race ahead of it.
    h.publish_event(None, draft_event("held"));
    h.publish_event(
        None,
        Event::HarnessNotice(tau_proto::HarnessNotice {
            kind: "test.info".to_owned(),
            message: "second".to_owned(),
            level: tau_proto::NoticeLevel::Info,
            always_show: false,
        }),
    );
    // Neither has committed yet — interception is in flight on the
    // first, the second is sitting in `deferred_publishes`.
    assert_eq!(h.event_log.next_seq(), baseline_seq);

    // Reply: pass-through. Both events should now commit, in order.
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("pass reply");

    let first = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("first event committed");
    assert_eq!(first.event, draft_event("held"));
    let second = h
        .event_log
        .get_next_from(first.seq.next())
        .expect("second event committed");
    assert!(matches!(
        &second.event,
        Event::HarnessNotice(info) if info.message == "second"
    ));
}

/// Regresses a rostra session failure where a parked unrelated event caused
/// routed-call tracking to clear before a deferred terminal report committed.
#[test]
fn deferred_tool_result_report_keeps_tracking_until_report_commit() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let session_id = h.current_session_id.clone();
    h.initialized_sessions.insert(session_id.clone());
    let cid = ensure_test_user_agent(&mut h);
    let call_id: ToolCallId = "call-read".into();
    let tool_name = ToolName::new("read");

    let agent_id = h
        .ensure_agent_id_for_agent(&cid)
        .expect("default conversation has an agent id");
    h.tool_agents.insert(call_id.clone(), cid.clone());
    h.pending_tools.insert(
        call_id.clone(),
        PendingTool {
            name: tool_name.clone(),
            internal_name: tool_name.clone(),
            tool_type: tau_proto::ToolType::Function,
            allows_provider_image: false,
        },
    );
    h.pending_tool_providers
        .insert(call_id.clone(), "tool-provider".into());
    let _provider = connect_ready_configured_extension(
        &mut h,
        "tool-provider",
        "configured-tool-provider",
        tau_proto::ClientKind::Tool,
    );
    h.publish_for_agent(
        &cid,
        Event::ProviderResponseFinished(ProviderResponseFinished {
            estimated_api_cost_rates: None,
            estimated_api_cost_increment: None,

            agent_prompt_id: "sp-main".into(),
            agent_id: crate::parse_agent_id(&agent_id),
            output_items: vec![ContextItem::ToolCall(ToolCallItem {
                call_id: call_id.clone(),
                name: tool_name.clone(),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            })],
            stop_reason: tau_proto::ProviderStopReason::ToolCalls,
            error: None,
            failure_kind: None,
            context_limit_telemetry: None,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            usage: None,
            originator: tau_proto::PromptOriginator::User,
            compaction_original_input_tokens: None,
            compaction_compacted_input_tokens: None,
            backend: None,
            provider_response_id: None,
            ws_pool_delta: None,
        }),
    );

    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    h.publish_event(None, draft_event("held"));
    assert!(
        h.pending_intercept.is_some(),
        "draft publish should be parked in interception"
    );

    h.handle_extension_event(
        "tool-provider",
        TestProtocolItem::Event(Event::ToolResultReported(ToolResult {
            call_id: call_id.clone(),
            tool_name: tool_name.clone(),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("ok".to_owned()),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            originator: tau_proto::PromptOriginator::User,

            display: None,
        })),
    )
    .expect("defer tool result");
    assert!(
        h.tool_agents.contains_key(&call_id),
        "tool call tracking must remain until the deferred report commits"
    );

    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("intercept reply");
    assert!(
        !h.tool_agents.contains_key(&call_id),
        "post-commit terminal processing clears routed-call tracking"
    );

    let has_result = default_agent_branch(&h).iter().any(|entry| {
        matches!(
            entry,
            AgentEntry::ToolResults { items }
                if items.iter().any(|item|
                    item.call_id == call_id && item.status == ToolResultStatus::Success
                )
        )
    });
    assert!(
        has_result,
        "deferred tool.result must persist despite cleared call tracking"
    );
}

#[test]
fn interception_drop_of_must_pass_event_is_overridden() {
    // AgentPromptSubmitted is on the MUST_PASS list — even if an
    // interceptor returns Drop, the harness must publish the
    // original event (with a warn).
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_SUBMITTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let prompt = Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
        inference_activation: true,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        text: "hello".to_owned(),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        display_name: None,
        ctx_id: None,
    });
    h.publish_event(None, prompt.clone());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("must-pass event still committed despite Drop");
    assert_eq!(entry.event, prompt);
}

fn agent_started_event(role: &str) -> Event {
    Event::AgentStarted(tau_proto::AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),

        parent_agent: None,
        agent_id: tau_proto::AgentId::parse("agent-started-test").expect("agent id"),
        role: role.to_owned(),
        display_name: Some("Started Test".to_owned()),
        metadata: Vec::new(),
        ephemeral: false,
    })
}

fn persisted_agent_started_events(h: &Harness) -> Vec<Event> {
    h.agent_store
        .agent_events("agent-started-test")
        .expect("agent.started durable log")
        .into_iter()
        .map(|entry| entry.event)
        .collect()
}

/// Ensures interceptors cannot drop agent creation facts now that
/// AgentStarted flows through the central publish/interception pipeline.
#[test]
fn interception_drop_of_agent_started_is_overridden() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::AGENT_STARTED)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let started = agent_started_event("engineer");
    h.publish_event(None, started.clone());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("agent.started still committed despite Drop");
    assert_eq!(entry.event, started);
    assert_eq!(persisted_agent_started_events(&h), vec![started]);
}

/// Ensures interceptors cannot rewrite immutable agent creation facts such as
/// the role attached to an AgentStarted event.
#[test]
fn interception_replacement_of_agent_started_publishes_original() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::AGENT_STARTED)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let started = agent_started_event("engineer");
    h.publish_event(None, started.clone());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(agent_started_event("reviewer")))),
        })),
    )
    .expect("replacement reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("agent.started committed");
    assert_eq!(entry.event, started);
    assert_eq!(persisted_agent_started_events(&h), vec![started]);
}

/// Accepted-visible interaction facts bind the monotonic live-routing update to
/// the same agent whose durable summary advances; interceptors must not
/// retarget that content-free fact.
#[test]
fn interception_cannot_retarget_user_interaction_fact() {
    let original = Event::AgentUserInteractionRecorded(tau_proto::AgentUserInteractionRecorded {
        agent_id: crate::parse_agent_id("agent-1"),
    });
    let replacement =
        Event::AgentUserInteractionRecorded(tau_proto::AgentUserInteractionRecorded {
            agent_id: crate::parse_agent_id("agent-2"),
        });

    assert!(
        crate::harness::interception::immutable_protected_fact_was_modified(
            &original,
            &replacement,
        )
    );
}

/// Ensures the initialization replacement and canonical projections
/// cannot be forged or rewritten before their runtime producers exist.
#[test]
fn discovery_canonical_events_are_protected() {
    let agent_id = crate::parse_agent_id("agent-1");
    let initialization_id = tau_proto::AgentInitializationId::new("init-1");
    let events = [
        Event::AgentInitializationContextSet(tau_proto::AgentInitializationContextSet {
            session_id: "session-1".into(),
            agent_id: agent_id.clone(),
            agent_initialization_id: initialization_id.clone(),
            agents_message: None,
            effective_skills: Vec::new(),
            agents_files: Vec::new(),
        }),
        Event::HarnessAgentContextInitialized(tau_proto::HarnessAgentContextInitialized {
            session_id: "session-1".into(),
            agent_id,
            agent_initialization_id: initialization_id,
            listed_skills: Vec::new(),
            agents_files: Vec::new(),
        }),
        Event::HarnessSessionSkillsAvailable(tau_proto::HarnessSessionSkillsAvailable {
            session_id: "session-1".into(),
            skills: Vec::new(),
        }),
    ];

    for event in &events {
        assert!(Harness::is_peer_forbidden_harness_fact(event));
        assert!(crate::harness::interception::event_must_pass_by_default(
            &event.name()
        ));
        let mut replacement = event.clone();
        match &mut replacement {
            Event::AgentInitializationContextSet(value) => {
                value.session_id = "forged-session".into();
            }
            Event::HarnessAgentContextInitialized(value) => {
                value.session_id = "forged-session".into();
            }
            Event::HarnessSessionSkillsAvailable(value) => {
                value.session_id = "forged-session".into();
            }
            _ => unreachable!("discovery scaffold fixture"),
        }
        assert!(
            crate::harness::interception::immutable_protected_fact_was_modified(
                event,
                &replacement,
            )
        );
    }

    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "configured-tool",
        "configured-tool",
        tau_proto::ClientKind::Tool,
    );
    connect_test_client_with_origin(
        &mut h,
        "attached-ui",
        tau_proto::ClientKind::Ui,
        ConnectionOrigin::Socket,
    );
    let baseline = h.event_log.next_seq();
    for event in &events {
        let emit = TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit::with_persist(
            event.clone(),
            event.defaults_to_persist(),
        )));
        h.handle_extension_event("configured-tool", emit.clone())
            .expect("configured extension forged event is ignored");
        h.handle_client_event("attached-ui", emit)
            .expect("attached UI forged event is ignored");
    }
    assert!(h.event_log.get_next_from(baseline).is_none());

    let interceptor = connect_test_tool(&mut h, "discovery-interceptor");
    h.handle_extension_event(
        "discovery-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: events
                .iter()
                .map(|event| EventSelector::Exact(event.name()))
                .collect(),
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register discovery interceptor");

    for event in events {
        let baseline = h.event_log.next_seq();
        h.publish_event(None, event.clone());
        let (intercepted, _) = intercepted_payload(&interceptor);
        assert_eq!(intercepted, event);
        h.handle_extension_event(
            "discovery-interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Drop,
            })),
        )
        .expect("drop is overridden");
        assert_eq!(
            h.event_log
                .get_next_from(baseline)
                .expect("must-pass event committed")
                .event,
            event
        );

        interceptor.lock().expect("interceptor frames").clear();
        let baseline = h.event_log.next_seq();
        h.publish_event(None, event.clone());
        let _ = intercepted_payload(&interceptor);
        let mut replacement = event.clone();
        match &mut replacement {
            Event::AgentInitializationContextSet(value) => {
                value.session_id = "forged-session".into();
            }
            Event::HarnessAgentContextInitialized(value) => {
                value.session_id = "forged-session".into();
            }
            Event::HarnessSessionSkillsAvailable(value) => {
                value.session_id = "forged-session".into();
            }
            _ => unreachable!("discovery scaffold fixture"),
        }
        h.handle_extension_event(
            "discovery-interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(Some(Box::new(replacement))),
            })),
        )
        .expect("same-variant mutation is rejected");
        assert_eq!(
            h.event_log
                .get_next_from(baseline)
                .expect("immutable event committed")
                .event,
            event
        );
        interceptor.lock().expect("interceptor frames").clear();
    }
}

/// Outer-turn accounting boundaries survive actual interception Drop and
/// replacement replies, while peer publication rejects both event families.
#[test]
fn outer_turn_accounting_facts_are_immutable_and_must_pass() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    let interceptor = connect_test_tool(&mut h, "outer-turn-interceptor");
    h.handle_extension_event(
        "outer-turn-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![
                EventSelector::Exact(tau_proto::EventName::AGENT_OUTER_TURN_STARTED),
                EventSelector::Exact(tau_proto::EventName::AGENT_OUTER_TURN_FINISHED),
            ],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");

    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("first".to_owned()))
        .expect("dispatch first");
    let (started, persist) = intercepted_payload(&interceptor);
    assert!(persist);
    assert!(matches!(started, Event::AgentOuterTurnStarted(_)));
    h.handle_extension_event(
        "outer-turn-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop start");
    let prompt_id = h.agents[&cid]
        .in_flight_prompt
        .clone()
        .expect("first prompt continued after start");
    interceptor.lock().expect("interceptor frames").clear();
    h.handle_provider_response_finished(provider_text_response(
        &prompt_id,
        agent_id.clone(),
        "first done",
    ))
    .expect("finish first");
    let (finished, persist) = intercepted_payload(&interceptor);
    assert!(persist);
    assert!(matches!(finished, Event::AgentOuterTurnFinished(_)));
    let mut replacement = finished.clone();
    if let Event::AgentOuterTurnFinished(turn) = &mut replacement {
        turn.session_id = "forged-session".into();
    }
    h.handle_extension_event(
        "outer-turn-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(replacement))),
        })),
    )
    .expect("replace finish");

    let records = h
        .agent_store
        .agent_events(agent_id.as_str())
        .expect("agent records");
    assert!(records.iter().any(|record| record.event == started));
    assert!(records.iter().any(|record| record.event == finished));

    let record_count = records.len();
    for forged in [started, finished] {
        assert!(Harness::is_peer_forbidden_harness_fact(&forged));
        h.handle_extension_event(
            "outer-turn-interceptor",
            TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit::with_persist(
                forged, true,
            ))),
        )
        .expect("forbidden peer emit is ignored");
    }
    assert_eq!(
        h.agent_store
            .agent_events(agent_id.as_str())
            .expect("records after peer emits")
            .len(),
        record_count,
        "peer-authored lifecycle facts must not reach durable admission"
    );
}

/// Visible UI acceptance durably records its content-free timestamp before the
/// corresponding prompt can remain parked in interception.
#[test]
fn parked_ui_prompt_has_precommitted_interaction_fact() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h
        .agents
        .get(&cid)
        .and_then(|agent| agent.agent_id.clone())
        .expect("agent id");
    let parsed_agent_id = crate::parse_agent_id(&agent_id);
    h.agent_navigation_modes.insert(
        parsed_agent_id.clone(),
        tau_proto::AgentNavigationMode::Suspended,
    );
    let observer = connect_test_client(&mut h, "interaction-observer", tau_proto::ClientKind::Ui);
    h.bus
        .set_subscriptions(
            "interaction-observer",
            Vec::new(),
            vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_STATS_UPDATED,
            )],
        )
        .expect("observer subscription");
    observer.lock().expect("observer frames").clear();
    let _interceptor = connect_test_tool(&mut h, "interaction-interceptor");
    h.handle_extension_event(
        "interaction-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_SUBMITTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");

    h.handle_authenticated_ui_prompt_submitted(tau_proto::UiPromptSubmitted {
        literal: false,
        session_id: h.current_session_id.clone(),
        text: "park me".to_owned(),
        agent_id: parsed_agent_id.clone(),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    })
    .expect("accept visible prompt");

    assert!(h.pending_intercept.is_some(), "prompt remains parked");
    let interactions: Vec<_> = h
        .agent_store
        .agent_events(&agent_id)
        .expect("agent journal")
        .into_iter()
        .filter(|record| matches!(record.event, Event::AgentUserInteractionRecorded(_)))
        .collect();
    assert_eq!(interactions.len(), 1);
    assert_ne!(interactions[0].recorded_at.get(), 0);
    assert_eq!(
        h.agent_navigation_modes.get(&parsed_agent_id),
        Some(&tau_proto::AgentNavigationMode::Active)
    );
    assert!(
        observer
            .lock()
            .expect("observer frames")
            .iter()
            .any(|frame| matches!(
                peel_inner_event(&frame.frame),
                Some(Event::AgentStatsUpdated(stats))
                    if stats.agent_id == parsed_agent_id
                        && stats.navigation_mode == tau_proto::AgentNavigationMode::Active
            )),
        "the Active snapshot must publish before prompt content can remain parked"
    );
}

fn session_agent_loaded_event(agent_id: &str) -> Event {
    Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
        agent_initialization_id: tau_proto::AgentInitializationId::new("test-init"),

        session_id: "session-intercept".into(),
        agent_id: tau_proto::AgentId::parse(agent_id).expect("agent id"),
        ephemeral: false,
    })
}

fn session_agent_unloaded_event(agent_id: &str) -> Event {
    Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
        session_id: "session-intercept".into(),
        agent_id: tau_proto::AgentId::parse(agent_id).expect("agent id"),
    })
}

/// Ensures interceptors cannot drop durable session membership load facts,
/// because resume state depends on the committed membership log matching live
/// delivery.
#[test]
fn interception_drop_of_session_agent_loaded_is_overridden() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::SESSION_AGENT_LOADED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let loaded = session_agent_loaded_event("agent-loaded-original");
    h.publish_event(None, loaded.clone());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("session.agent_loaded still committed despite Drop");
    assert_eq!(entry.event, loaded);
    let membership = h
        .store
        .session("session-intercept")
        .expect("session membership");
    assert!(
        membership
            .contains_agent(&tau_proto::AgentId::parse("agent-loaded-original").expect("agent id"))
    );
}

/// Ensures interceptors cannot rewrite durable session membership unload facts,
/// preventing one agent's unload from being persisted as another agent's
/// unload.
#[test]
fn interception_replacement_of_session_agent_unloaded_publishes_original() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::SESSION_AGENT_UNLOADED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let unloaded = session_agent_unloaded_event("agent-unloaded-original");
    h.publish_event(None, unloaded.clone());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(session_agent_unloaded_event(
                "agent-unloaded-replacement",
            )))),
        })),
    )
    .expect("replacement reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("session.agent_unloaded committed");
    assert_eq!(entry.event, unloaded);
    let events = h
        .store
        .session_events("session-intercept")
        .expect("session events")
        .into_iter()
        .map(|entry| entry.event)
        .collect::<Vec<_>>();
    assert_eq!(events, vec![unloaded]);
}

fn session_started_event(session_id: &str) -> Event {
    Event::SessionStarted(tau_proto::SessionStarted {
        session_id: session_id.into(),
        reason: tau_proto::SessionStartReason::New,
    })
}

fn session_shutdown_event(session_id: &str) -> Event {
    Event::SessionShutdown(tau_proto::SessionShutdown {
        session_id: session_id.into(),
    })
}

fn agent_message_sent_event(message: &str) -> Event {
    Event::AgentMessageSent(tau_proto::AgentMessageSent {
        message_id: tau_proto::AgentMessageId::from("msg-intercept"),
        sender_id: tau_proto::AgentId::parse("agent-message-sender").expect("agent id"),
        recipient: tau_proto::AgentMessageRecipient::Agent {
            agent_id: tau_proto::AgentId::parse("agent-message-recipient").expect("agent id"),
        },
        kind: tau_proto::AgentMessageKind::Message,
        message: message.to_owned(),
    })
}

fn agent_message_received_event(recipient_id: &str) -> Event {
    Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
        message_id: tau_proto::AgentMessageId::from("msg-intercept"),
        sender_id: tau_proto::AgentId::parse("agent-message-sender").expect("agent id"),
        sender_session_id: None,
        recipient_id: tau_proto::AgentId::parse(recipient_id).expect("agent id"),
        kind: tau_proto::AgentMessageKind::Message,
        watch_turn_state: None,
        watch_provider_status: None,
        message: "hello".to_owned(),
    })
}

/// Ensures interceptors cannot drop session lifecycle facts required by
/// extensions and context providers for per-session setup.
#[test]
fn interception_drop_of_session_started_is_overridden() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::SESSION_STARTED)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let started = session_started_event("session-lifecycle-original");
    h.publish_event(None, started.clone());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("session.started still committed despite Drop");
    assert_eq!(entry.event, started);
}

/// Ensures interceptors cannot rewrite session shutdown facts used to flush or
/// drop extension-owned per-session state.
#[test]
fn interception_replacement_of_session_shutdown_publishes_original() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::SESSION_SHUTDOWN)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let shutdown = session_shutdown_event("session-lifecycle-original");
    h.publish_event(None, shutdown.clone());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(session_shutdown_event(
                "session-lifecycle-replacement",
            )))),
        })),
    )
    .expect("replacement reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("session.shutdown committed");
    assert_eq!(entry.event, shutdown);
}

/// Ensures interceptors cannot drop harness-validated sender-side message
/// projections after recipient validation has already succeeded.
#[test]
fn interception_drop_of_agent_message_sent_is_overridden() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MESSAGE_SENT,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let sent = agent_message_sent_event("hello");
    h.publish_event(None, sent.clone());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("agent.message_sent still committed despite Drop");
    assert_eq!(entry.event, sent);
}

/// Ensures interceptors cannot rewrite harness-validated recipient-side message
/// projections, including attempts to route the projection to another agent.
#[test]
fn interception_replacement_of_agent_message_received_publishes_original() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MESSAGE_RECEIVED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let received = agent_message_received_event("agent-message-recipient");
    h.publish_event(None, received.clone());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(agent_message_received_event(
                "agent-message-other-recipient",
            )))),
        })),
    )
    .expect("replacement reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("agent.message_received committed");
    assert_eq!(entry.event, received);
}

fn tool_result_event(call_id: &str, text: &str) -> Event {
    Event::ToolResult(ToolResult {
        call_id: call_id.into(),
        tool_name: ToolName::new("test_tool"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text(text.to_owned()),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        originator: tau_proto::PromptOriginator::User,
        display: None,
    })
}

fn tool_cancelled_event(call_id: &str) -> Event {
    Event::ToolCancelled(tau_proto::ToolCancelled {
        call_id: call_id.into(),
        tool_name: ToolName::new("test_tool"),
        tool_type: tau_proto::ToolType::Function,
    })
}

/// Ensures interceptors cannot rewrite terminal tool transcript facts, because
/// changing the call id would detach the completion from the requested tool
/// use.
#[test]
fn interception_replacement_of_tool_result_publishes_original() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::TOOL_RESULT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let result = tool_result_event("call-original", "ok");
    h.publish_event(None, result.clone());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(tool_result_event(
                "call-rewritten",
                "rewritten",
            )))),
        })),
    )
    .expect("replacement reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("tool.result committed");
    assert_eq!(entry.event, result);
}

/// Ensures interceptors cannot drop cancellation facts for terminal tool calls.
#[test]
fn interception_drop_of_tool_cancelled_is_overridden() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::TOOL_CANCELLED)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let cancelled = tool_cancelled_event("call-cancelled");
    h.publish_event(None, cancelled.clone());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop reply");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("tool.cancelled still committed despite Drop");
    assert_eq!(entry.event, cancelled);
}

/// Ensures a failed intercept-request delivery does not park the publish
/// pipeline forever and subsequent publishes still commit.
#[test]
fn failed_intercept_request_delivery_skips_interceptor_and_drains_publishes() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    connect_failing_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    let first = draft_event("first");
    let second = draft_event("second");
    h.publish_event(None, first.clone());
    h.publish_event(None, second.clone());

    assert!(h.pending_intercept.is_none());
    let first_entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("first draft committed");
    assert_eq!(first_entry.event, first);
    let second_entry = h
        .event_log
        .get_next_from(first_entry.seq.next())
        .expect("second draft committed");
    assert_eq!(second_entry.event, second);
}

/// Disconnecting the responder is equivalent to `Pass(None)`: the original
/// publication commits and the interception FIFO drains instead of wedging.
#[test]
fn interception_disconnect_mid_reply_publishes_original() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let baseline_seq = h.event_log.next_seq();

    h.publish_event(None, draft_event("inflight"));
    // Disconnect before the interceptor replies. The harness should
    // treat this as Pass(None) and still commit the event.
    h.handle_disconnect("interceptor");

    let entry = h
        .event_log
        .get_next_from(baseline_seq)
        .expect("event committed after disconnect");
    assert_eq!(entry.event, draft_event("inflight"));
}

/// A prompt parked in interception cannot dispatch against the pre-commit
/// branch; release first folds the user input, then creates a prompt containing
/// that exact committed message.
#[test]
fn interception_user_prompt_dispatch_waits_for_commit() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let session_id = h.current_session_id.clone();
    h.initialized_sessions.insert(session_id.clone());

    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_SUBMITTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");

    let cid = ensure_test_user_agent(&mut h);
    let head_before_dispatch = h.agents.get(&cid).and_then(|c| c.head);
    let prompts_before = prompt_created_count(&h);

    // Drive the user-prompt path. The publish parks in interception.
    h.dispatch_prompt_for_agent(&cid, "real question".to_owned())
        .expect("dispatch");

    // While the intercept is in flight: no agent prompt was minted,
    // c.head hasn't moved, and the deferred-dispatch queue contains
    // our cid.
    assert_eq!(
        prompt_created_count(&h),
        prompts_before,
        "agent dispatch must wait until the prompt commits"
    );
    assert_eq!(
        h.agents.get(&cid).and_then(|c| c.head),
        head_before_dispatch,
        "c.head must not advance while the prompt is parked"
    );
    assert!(h.pending_intercept.as_ref().is_some_and(|pending| {
        pending
            .sync_head_for
            .as_ref()
            .is_some_and(|sync| sync.cid == cid && !sync.suppress_activation_dispatch)
    }));

    // Reply pass-through. Commit + react fires the deferred
    // dispatch, and the AgentPromptCreated is built from the
    // updated tree.
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("intercept reply");

    assert!(h.pending_intercept.is_none());
    assert_eq!(
        prompt_created_count(&h),
        prompts_before + 1,
        "agent dispatch fires once the prompt commits"
    );
    let head_after = h
        .agents
        .get(&cid)
        .and_then(|c| c.head)
        .expect("c.head advanced");
    let entry = default_agent_node(&h, head_after);
    assert!(
        matches!(
            &entry.entry,
            AgentEntry::UserInput { items, .. }
                if matches!(
                    items.as_slice(),
                    [ContextItem::Message(MessageItem {
                        role: ContextRole::User,
                        content,
                        ..
                    })] if matches!(content.as_slice(), [ContentPart::Text { text }] if text == "real question")
                )
        ),
        "c.head points at the just-committed user prompt"
    );
}

#[test]
fn passive_background_notice_and_user_prompt_dispatch_as_one_intercepted_batch() {
    // Regression: passive background notices published before a real user prompt
    // must not let interception wake provider dispatch before the user prompt
    // itself commits. The passive notice and user prompt are treated as one
    // publish batch and dispatch only after both intercepted submissions pass.
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let session_id = h.current_session_id.clone();
    h.initialized_sessions.insert(session_id);
    h.selected_model = Some("echo/model".into());
    let info = h
        .provider_model_info
        .get_mut(&"echo/model".into())
        .expect("echo model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    info.standalone_compaction_threshold = Some(900);

    let _interceptor = connect_test_tool(&mut h, "interceptor-passive-batch");
    h.handle_extension_event(
        "interceptor-passive-batch",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_SUBMITTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");

    let cid = ensure_test_user_agent(&mut h);
    {
        let conv = h.agents.get_mut(&cid).expect("conversation");
        conv.context_input_tokens = Some(900);
        conv.context_usage_head = conv.head;
        conv.context_usage_model = Some("echo/model".into());
        conv.context_cached_tokens = Some(450);
    }
    let passive_text = background_completion_prompt(&"passive-intercept-bg".into());
    h.agents
        .get_mut(&cid)
        .expect("conversation")
        .pending_prompts
        .push_back(PendingPrompt::passive_background_completion(
            passive_text.clone(),
        ));
    let prompts_before = prompt_created_count(&h);

    h.dispatch_prompt_for_agent(&cid, "real follow-up".to_owned())
        .expect("dispatch user prompt with passive notice");

    assert_eq!(prompt_created_count(&h), prompts_before);
    assert!(h.pending_intercept.is_some());
    assert_eq!(h.pending_publish_idle_dispatches.len(), 1);
    assert!(!h.pending_publish_idle_dispatches[0].committed_activation);

    h.handle_extension_event(
        "interceptor-passive-batch",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("pass passive notice");

    assert_eq!(
        prompt_created_count(&h),
        prompts_before,
        "provider dispatch must still wait for the real user prompt"
    );
    assert!(h.pending_intercept.as_ref().is_some_and(|pending| matches!(
        pending.event,
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            inference_activation: true,
            ..
        })
    )));
    assert_eq!(h.pending_publish_idle_dispatches.len(), 1);
    assert!(!h.pending_publish_idle_dispatches[0].committed_activation);

    h.handle_extension_event(
        "interceptor-passive-batch",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("pass user prompt");

    assert!(h.pending_intercept.is_none());
    assert!(h.pending_publish_idle_dispatches.is_empty());
    assert_eq!(prompt_created_count(&h), prompts_before + 1);
    let compact = read_nth_prompt_created(&h, prompts_before as usize);
    assert_eq!(
        compact.operation,
        tau_proto::PromptOperation::StandaloneCompaction
    );
    assert!(
        !event_log_events(&h)
            .into_iter()
            .any(|event| matches!(event, Event::AgentInferenceDispatchStarted(_)))
    );
    let active_head = h.agents[&cid].head.expect("active prompt head");
    let active_parent = default_agent_node(&h, active_head)
        .parent_id
        .expect("passive fact is active parent");
    assert!(event_log_events(&h).into_iter().any(|event| matches!(
        event,
        Event::AgentStandaloneCompactionStarted(started)
            if started.cut == tau_proto::AgentHead::Node(active_parent)
    )));
    let submitted: Vec<(String, bool)> = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentPromptSubmitted(submitted)
                if submitted.text == passive_text || submitted.text == "real follow-up" =>
            {
                Some((submitted.text, submitted.inference_activation))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        submitted,
        vec![(passive_text, false), ("real follow-up".to_owned(), true)],
        "passive notice should commit false immediately before the active user prompt"
    );
}

/// Navigation queued behind an intercepted activation commits after that
/// activation, leaving its exact branch-owned obligation dormant until the
/// original activation node is reselected.
#[test]
fn intercepted_activation_navigation_keeps_original_branch_dormant() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let session_id = h.current_session_id.clone();
    h.initialized_sessions.insert(session_id);
    h.selected_model = Some("echo/model".into());
    let _interceptor = connect_test_tool(&mut h, "interceptor-branch-activation");
    h.handle_extension_event(
        "interceptor-branch-activation",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_SUBMITTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);

    h.dispatch_prompt_for_agent(&cid, "branch A activation".to_owned())
        .expect("intercept activation");
    assert!(h.pending_intercept.as_ref().is_some_and(|pending| {
        pending
            .sync_head_for
            .as_ref()
            .is_some_and(|sync| sync.cid == cid && !sync.suppress_activation_dispatch)
    }));
    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: agent_id.clone(),
            head: tau_proto::AgentHead::Root,
        }),
    );
    h.handle_extension_event(
        "interceptor-branch-activation",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit activation then navigation");

    let activation_node = default_agent_tree(&h)
        .nodes()
        .iter()
        .find(|node| {
            matches!(
                &node.entry,
                AgentEntry::UserInput { items, .. }
                    if items.iter().any(|item| {
                        matches!(
                            item,
                            ContextItem::Message(MessageItem { content, .. })
                                if content.iter().any(|part| {
                                    matches!(
                                        part,
                                        ContentPart::Text { text }
                                            if text == "branch A activation"
                                    )
                                })
                        )
                    })
            )
        })
        .map(|node| node.id)
        .expect("activation node");
    assert_eq!(h.agents[&cid].head, None);
    assert_eq!(prompt_created_count(&h), 0);
    assert!(matches!(
        h.agents[&cid].activation_dispatch,
        crate::agent::ActivationDispatchState::None
    ));
    assert_eq!(h.pending_publish_idle_dispatches.len(), 1);

    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id,
            head: tau_proto::AgentHead::Node(activation_node),
        }),
    );
    assert_eq!(prompt_created_count(&h), 1);
    assert!(h.pending_publish_idle_dispatches.is_empty());
}

/// Two intercepted same-agent activations separated onto sibling branches
/// retain two precommit tokens and become independent branch-owned obligations.
#[test]
fn intercepted_sibling_activations_retain_distinct_obligations() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let session_id = h.current_session_id.clone();
    h.initialized_sessions.insert(session_id);
    h.selected_model = Some("echo/model".into());
    let _interceptor = connect_test_tool(&mut h, "interceptor-sibling-activations");
    h.handle_extension_event(
        "interceptor-sibling-activations",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_SUBMITTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);

    h.dispatch_prompt_for_agent(&cid, "branch A activation".to_owned())
        .expect("intercept branch A");
    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: agent_id.clone(),
            head: tau_proto::AgentHead::Root,
        }),
    );
    h.dispatch_prompt_for_agent(&cid, "branch B activation".to_owned())
        .expect("queue branch B");
    assert!(h.pending_intercept.is_some());
    assert!(h.deferred_publishes.iter().any(|publish| matches!(
        publish.event(),
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            inference_activation: true,
            text,
            ..
        }) if text == "branch B activation"
    )));

    h.handle_extension_event(
        "interceptor-sibling-activations",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit branch A and park branch B");
    assert!(h.pending_intercept.is_some());
    assert_eq!(h.pending_publish_idle_dispatches.len(), 1);
    h.handle_extension_event(
        "interceptor-sibling-activations",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit branch B");

    let nodes = default_agent_tree(&h)
        .nodes()
        .iter()
        .filter_map(|node| match &node.entry {
            AgentEntry::UserInput { items, .. } => items.iter().find_map(|item| match item {
                ContextItem::Message(MessageItem { content, .. }) => {
                    content.iter().find_map(|part| match part {
                        ContentPart::Text { text }
                            if text == "branch A activation" || text == "branch B activation" =>
                        {
                            Some((text.clone(), node.id))
                        }
                        _ => None,
                    })
                }
                _ => None,
            }),
            _ => None,
        })
        .collect::<std::collections::HashMap<_, _>>();
    let branch_a = nodes["branch A activation"];
    let branch_b = nodes["branch B activation"];
    assert_ne!(branch_a, branch_b);
    let branch_b_prompt = read_nth_prompt_created(&h, 0);
    assert_eq!(h.agents[&cid].head, Some(branch_b));
    assert_eq!(h.pending_publish_idle_dispatches.len(), 1);
    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::AgentInferenceDispatchStarted(started)
            if started.agent_prompt_id == branch_b_prompt.agent_prompt_id
                && started.through == tau_proto::AgentHead::Node(branch_b)
    )));

    h.handle_provider_response_finished(provider_text_response(
        &branch_b_prompt.agent_prompt_id,
        agent_id.clone(),
        "branch B complete",
    ))
    .expect("finish branch B");
    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id,
            head: tau_proto::AgentHead::Node(branch_a),
        }),
    );
    assert_eq!(prompt_created_count(&h), 2);
    let branch_a_prompt = read_nth_prompt_created(&h, 1);
    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::AgentInferenceDispatchStarted(started)
            if started.agent_prompt_id == branch_a_prompt.agent_prompt_id
                && started.through == tau_proto::AgentHead::Node(branch_a)
    )));
    assert!(h.pending_intercept.is_none());
    assert!(h.pending_publish_idle_dispatches.is_empty());
}

/// An activating fact rejected by the agent journal leaves no detached
/// activation ownership without fail-stopping later semantic work.
#[test]
fn rejected_activating_append_leaves_no_stale_dispatch() {
    let tmp = TempDir::new().expect("tempdir");
    let state_dir = tmp.path().join("state");
    let mut h = quiet_provider_harness(&state_dir).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    let event_path = state_dir
        .join("agents")
        .join(agent_id.as_str())
        .join("events.cbor");
    let backup_path = event_path.with_extension("cbor.activation-backup");
    std::fs::rename(&event_path, &backup_path).expect("park agent journal");
    std::fs::create_dir(&event_path).expect("reject activation append");
    h.publish_for_agent(
        &cid,
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            inference_activation: true,
            agent_id: agent_id.clone(),
            text: "rejected activation".to_owned(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: tau_proto::PromptSubmissionSource::HumanUi,
            display_name: None,
            ctx_id: None,
        }),
    );
    assert!(h.pending_publish_idle_dispatches.is_empty());
    let conv = h.agents.get(&cid).expect("agent");
    assert!(
        conv.pending_prompts
            .iter()
            .all(|prompt| prompt.text != "rejected activation" && !prompt.is_loop_guard())
    );
    assert_eq!(conv.loop_guard.consecutive_tool_failures(), 0);
    assert!(!conv.loop_guard.stop_automatic_continuation());
    assert!(h.pending_tools.is_empty());
    assert!(h.tool_agents.is_empty());
    assert!(h.peer_internal_tool_agents.is_empty());
    std::fs::remove_dir(&event_path).expect("remove append blocker");
    std::fs::rename(&backup_path, &event_path).expect("restore agent journal");

    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("committed activation".to_owned()))
        .expect("dispatch later activation");
    assert!(h.pending_publish_idle_dispatches.is_empty());
    assert_eq!(prompt_created_count(&h), 1);
}

#[test]
fn interception_mutating_prompt_reaches_agent() {
    // End-to-end check that mirrors the test-dummy's "Tao → Tau"
    // correction flow: an interceptor replies with
    // `Pass(Some(modified))` and the agent receives the modified
    // text in its message list. Verifies the full chain (intercept
    // request → reply with mutation → fold of mutated event →
    // c.head sync → agent dispatch with up-to-date branch) end-to-
    // end.
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let session_id = h.current_session_id.clone();
    h.initialized_sessions.insert(session_id.clone());

    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_SUBMITTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");

    let cid = ensure_test_user_agent(&mut h);
    h.dispatch_prompt_for_agent(&cid, "I love Tao".to_owned())
        .expect("dispatch");

    // Interceptor replies with the mutated event.
    let agent_id = h
        .agents
        .get(&cid)
        .and_then(|conv| conv.agent_id.as_ref())
        .expect("prompt publish assigned an agent id")
        .clone();
    let mutated = Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
        inference_activation: true,
        agent_id: crate::parse_agent_id(&agent_id),
        text: "I love Tau".to_owned(),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        display_name: None,
        ctx_id: None,
    });
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(mutated))),
        })),
    )
    .expect("intercept reply");

    // The committed user message reflects the *mutated* text — and
    // c.head points at it (see `interception_user_prompt_dispatch_
    // waits_for_commit` for the dispatch-side assertion).
    let head = h
        .agents
        .get(&cid)
        .and_then(|c| c.head)
        .expect("c.head advanced");
    let entry = default_agent_node(&h, head);
    assert!(
        matches!(
            &entry.entry,
            AgentEntry::UserInput { items, .. }
                if matches!(
                    items.as_slice(),
                    [ContextItem::Message(MessageItem {
                        role: ContextRole::User,
                        content,
                        ..
                    })] if matches!(content.as_slice(), [ContentPart::Text { text }] if text == "I love Tau")
                )
        ),
        "the agent will see the *interceptor-mutated* text, not the user's typo"
    );
}

#[test]
fn publish_for_agent_does_not_emit_navigate_tree() {
    // Phase 4: cross-conversation publishes used to bounce
    // `tree.head()` via a `UiNavigateTree` event before folding the
    // real event. With explicit-parent folds in
    // `AgentTree::apply_event_at`, the bounce is gone — the harness
    // stamps the conversation's `head` directly.
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let session_id = h.current_session_id.clone();
    h.initialized_sessions.insert(session_id.clone());

    let baseline_seq = h.event_log.next_seq();
    let cid = ensure_test_user_agent(&mut h);

    // Two prompts in a row on the same conversation. Either would
    // historically have caused `publish_for_agent_from` to
    // bounce `tree.head()` via `UiNavigateTree`.
    h.dispatch_prompt_for_agent(&cid, "first".to_owned())
        .expect("first dispatch");
    h.dispatch_prompt_for_agent(&cid, "second".to_owned())
        .expect("second dispatch");

    let mut navigates = 0;
    let mut user_msgs = 0;
    let mut id = baseline_seq;
    while let Some(entry) = h.event_log.get_next_from(id) {
        match &entry.event {
            Event::UiNavigateTree(_) => navigates += 1,
            Event::AgentPromptSubmitted(_) => user_msgs += 1,
            _ => {}
        }
        id = entry.seq.next();
    }
    assert_eq!(
        navigates, 0,
        "cross-conversation publishes must not emit UiNavigateTree anymore"
    );
    assert_eq!(user_msgs, 2);
}

#[test]
fn interception_disconnect_clears_registration() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    h.handle_disconnect("interceptor");
    let after_disconnect_seq = h.event_log.next_seq();

    h.publish_event(None, draft_event("not intercepted"));

    let entry = h
        .event_log
        .get_next_from(after_disconnect_seq)
        .expect("event reaches log");
    assert_eq!(entry.event, draft_event("not intercepted"));
}

#[test]
fn agent_metadata_set_and_unset_events_are_interceptable() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let interceptor = connect_test_tool(&mut h, "metadata-interceptor");
    h.handle_extension_event(
        "metadata-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![
                EventSelector::Exact(tau_proto::EventName::AGENT_METADATA_SET),
                EventSelector::Exact(tau_proto::EventName::AGENT_METADATA_UNSET),
            ],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");

    let agent_id = tau_proto::AgentId::parse("metadata-agent").expect("agent id");
    h.session_loaded_agents.insert(agent_id.clone());
    let key = tau_proto::AgentMetadataKey::new("ext_core-shell_cwd");
    let set = Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
        agent_id: agent_id.clone(),
        key: key.clone(),
        value: CborValue::Text("/tmp".to_owned()),
        mutation_id: Some(
            tau_proto::AgentMetadataMutationId::parse("mutation-1").expect("mutation id"),
        ),
        inheritable: true,
    });
    h.publish_event(None, set.clone());
    let (event, persist) = intercepted_payload(&interceptor);
    assert_eq!(event, set);
    assert!(persist, "metadata set must be durable by default");
    h.handle_extension_event(
        "metadata-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(Event::AgentMetadataSet(
                tau_proto::AgentMetadataSet {
                    agent_id: tau_proto::AgentId::parse("rewritten-agent").expect("agent id"),
                    key: tau_proto::AgentMetadataKey::new("rewritten-key"),
                    value: CborValue::Text("/rewritten".to_owned()),
                    mutation_id: None,
                    inheritable: false,
                },
            )))),
        })),
    )
    .expect("pass set");
    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::AgentMetadataSet(committed)
            if committed.value == CborValue::Text("/rewritten".to_owned())
                && committed.agent_id == agent_id
                && committed.key == key
                && committed.inheritable
                && committed.mutation_id.as_ref().is_some_and(|id| id.as_str() == "mutation-1")
    )));

    interceptor.lock().expect("events").clear();
    let must_pass = Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
        agent_id: agent_id.clone(),
        key: key.clone(),
        value: CborValue::Text("/must-pass".to_owned()),
        mutation_id: Some(
            tau_proto::AgentMetadataMutationId::parse("mutation-2").expect("mutation id"),
        ),
        inheritable: true,
    });
    h.publish_event(None, must_pass.clone());
    let _ = intercepted_payload(&interceptor);
    h.handle_extension_event(
        "metadata-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop tokened set");
    assert!(event_log_events(&h).contains(&must_pass));

    interceptor.lock().expect("events").clear();
    let unset = Event::AgentMetadataUnset(tau_proto::AgentMetadataUnset { agent_id, key });
    h.publish_event(None, unset.clone());
    let (event, persist) = intercepted_payload(&interceptor);
    assert_eq!(event, unset);
    assert!(persist, "metadata unset must be durable by default");

    h.shutdown().expect("shutdown");
}

/// Rollover preserves a deferred mutation-correlated metadata set under the
/// same effective must-pass policy used by an interceptor Drop.
#[test]
fn rollover_commits_deferred_mutation_correlated_metadata_set() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let agent_id = tau_proto::AgentId::parse("metadata-rollover-agent").expect("agent id");
    h.session_loaded_agents.insert(agent_id.clone());
    let _interceptor = connect_test_tool(&mut h, "metadata-rollover-blocker");
    h.handle_extension_event(
        "metadata-rollover-blocker",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register rollover blocker");
    h.publish_event(None, draft_event("block metadata mutation"));
    let metadata = Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
        agent_id,
        key: tau_proto::AgentMetadataKey::new("rollover-key"),
        value: CborValue::Text("retained".to_owned()),
        mutation_id: Some(
            tau_proto::AgentMetadataMutationId::parse("rollover-mutation").expect("mutation id"),
        ),
        inheritable: true,
    });
    h.publish_event(None, metadata.clone());
    assert!(!event_log_events(&h).contains(&metadata));

    h.switch_session("replacement".into(), tau_proto::SessionStartReason::New)
        .expect("switch session");

    assert!(event_log_events(&h).contains(&metadata));
}

/// Deferred peer observation families commit across rollover while the advanced
/// admission generation suppresses internal-prompt and context-registration
/// effects uniformly.
#[test]
fn rollover_commits_deferred_peer_observations_without_semantic_effects() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    connect_ready_configured_extension(
        &mut h,
        "observation-owner",
        "configured-observation-owner",
        tau_proto::ClientKind::Core,
    );
    let _interceptor = connect_test_tool(&mut h, "observation-rollover-blocker");
    h.handle_extension_event(
        "observation-rollover-blocker",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register rollover blocker");
    h.publish_event(None, draft_event("block peer observations"));
    let observations = [
        Event::ExtInternalPromptSubmitRequest(tau_proto::ExtInternalPromptSubmitRequest {
            agent_id,
            text: "stale internal prompt".to_owned(),
            ctx_id: Some("stale-context".to_owned()),
        }),
        Event::ExtensionContextProviderRegister(tau_proto::ExtensionContextProviderRegister {}),
        Event::ExtensionSessionContextProviderRegister(
            tau_proto::ExtensionSessionContextProviderRegister {},
        ),
    ];
    for event in observations.clone() {
        h.handle_extension_event_inner("observation-owner", event)
            .expect("defer observation");
    }

    h.switch_session("replacement".into(), tau_proto::SessionStartReason::New)
        .expect("switch session");

    let events = event_log_events(&h);
    for observation in observations {
        assert!(events.contains(&observation));
    }
    assert!(!events.iter().any(|event| {
        matches!(
            event,
            Event::AgentPromptSubmitted(prompt)
                if prompt.text == "stale internal prompt"
        ) || matches!(
            event,
            Event::AgentPromptSteered(prompt)
                if prompt.text == "stale internal prompt"
        )
    }));
    let source = tau_proto::ConnectionId::from("observation-owner");
    assert!(!h.agent_context_providers.contains(&source));
    assert!(!h.session_context_providers.contains(&source));
}

/// Interceptors may rewrite progress payloads, but shell correlation/target
/// identity remains canonical and validated terminal delivery is immutable and
/// must-pass.
#[test]
fn shell_command_interception_preserves_identity_and_terminal_delivery() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let interceptor = connect_test_tool(&mut h, "shell-interceptor");
    h.handle_extension_event(
        "shell-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![
                EventSelector::Exact(tau_proto::EventName::SHELL_COMMAND_PROGRESS),
                EventSelector::Exact(tau_proto::EventName::SHELL_COMMAND_FINISHED),
            ],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");

    let agent_id = tau_proto::AgentId::parse("shell-agent").expect("agent id");
    let progress = Event::ShellCommandProgress(tau_proto::ShellCommandProgress {
        command_id: "shell-progress".into(),
        stream: tau_proto::ShellStream::Stdout,
        chunk: "original".to_owned(),
        target_agent_id: Some(agent_id.clone()),
    });
    h.publish_event(None, progress.clone());
    let _ = intercepted_payload(&interceptor);
    h.handle_extension_event(
        "shell-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(Event::ShellCommandProgress(
                tau_proto::ShellCommandProgress {
                    command_id: "redirected".into(),
                    stream: tau_proto::ShellStream::Stderr,
                    chunk: "rewritten".to_owned(),
                    target_agent_id: Some(
                        tau_proto::AgentId::parse("redirected-agent").expect("agent id"),
                    ),
                },
            )))),
        })),
    )
    .expect("rewrite progress");
    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::ShellCommandProgress(committed)
            if committed.command_id.as_str() == "shell-progress"
                && committed.target_agent_id.as_ref() == Some(&agent_id)
                && committed.chunk == "rewritten"
                && committed.stream == tau_proto::ShellStream::Stderr
    )));

    interceptor.lock().expect("events").clear();
    let finished = Event::ShellCommandFinished(tau_proto::ShellCommandFinished {
        command_id: "shell-finished".into(),
        session_id: "s1".into(),
        command: "pwd".to_owned(),
        include_in_context: false,
        target_agent_id: Some(agent_id.clone()),
        output: "original".to_owned(),
        exit_code: Some(0),
        cancelled: false,
    });
    h.publish_event(None, finished.clone());
    let _ = intercepted_payload(&interceptor);
    h.handle_extension_event(
        "shell-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(Event::ShellCommandFinished(
                tau_proto::ShellCommandFinished {
                    command_id: "redirected".into(),
                    session_id: "other-session".into(),
                    command: "malicious".to_owned(),
                    include_in_context: true,
                    target_agent_id: Some(
                        tau_proto::AgentId::parse("redirected-agent").expect("agent id"),
                    ),
                    output: "rewritten".to_owned(),
                    exit_code: Some(7),
                    cancelled: true,
                },
            )))),
        })),
    )
    .expect("rewrite terminal");
    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::ShellCommandFinished(committed)
            if committed.command_id.as_str() == "shell-finished"
                && committed.session_id.as_str() == "s1"
                && committed.command == "pwd"
                && !committed.include_in_context
                && committed.target_agent_id.as_ref() == Some(&agent_id)
                && committed.output == "original"
                && committed.exit_code == Some(0)
                && !committed.cancelled
    )));

    interceptor.lock().expect("events").clear();
    let must_pass = Event::ShellCommandFinished(tau_proto::ShellCommandFinished {
        command_id: "shell-must-pass".into(),
        session_id: "s1".into(),
        command: "pwd".to_owned(),
        include_in_context: false,
        target_agent_id: Some(agent_id),
        output: "must pass".to_owned(),
        exit_code: Some(0),
        cancelled: false,
    });
    h.publish_event(None, must_pass.clone());
    let _ = intercepted_payload(&interceptor);
    h.handle_extension_event(
        "shell-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop terminal");
    assert!(event_log_events(&h).contains(&must_pass));
}

/// A UI shell id remains reserved while its immutable terminal is parked in
/// interception, then becomes reusable only after that terminal commits.
#[test]
fn shell_command_ui_id_reservation_extends_through_terminal_commit() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let interceptor = connect_test_tool(&mut h, "shell-terminal-interceptor");
    h.handle_extension_event(
        "shell-terminal-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::SHELL_COMMAND_FINISHED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");
    let ui = connect_test_client(&mut h, "shell-terminal-ui", tau_proto::ClientKind::Ui);
    h.bus
        .set_subscriptions(
            "shell-terminal-ui",
            Vec::new(),
            vec![EventSelector::Exact(tau_proto::EventName::UI_SHELL_COMMAND)],
        )
        .expect("subscribe ui");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = crate::parse_agent_id(
        h.agents[&cid]
            .agent_id
            .as_deref()
            .expect("durable agent id"),
    );
    let command = tau_proto::UiShellCommand {
        session_id: h.current_session_id.clone(),
        command_id: "parked-ui-id".into(),
        command: "pwd".to_owned(),
        include_in_context: false,
        target_agent_id: Some(agent_id.clone()),
    };
    h.handle_ui_shell_command("ui", command.clone());
    let provider_id = super::super::ui_shell_provider_ids(&h.registry)
        .into_iter()
        .next()
        .expect("shell provider");
    let first_route = h
        .pending_ui_shell_commands
        .keys()
        .next()
        .expect("first route")
        .clone();
    let terminal = tau_proto::ShellCommandFinished {
        command_id: first_route.as_protocol_id().clone(),
        session_id: command.session_id.clone(),
        command: command.command.clone(),
        include_in_context: false,
        target_agent_id: Some(agent_id.clone()),
        output: "first".to_owned(),
        exit_code: Some(0),
        cancelled: false,
    };
    h.canonicalize_committed_shell_command_report(
        provider_id.as_str(),
        Event::ShellCommandFinishedReported(terminal),
    );
    let _ = intercepted_payload(&interceptor);
    assert!(h.pending_ui_shell_commands.is_empty());
    assert!(h.active_ui_shell_command_ids.contains(&command.command_id));

    h.handle_ui_shell_command("ui", command.clone());
    assert!(h.pending_ui_shell_commands.is_empty());
    assert_eq!(
        ui.lock()
            .expect("ui sink")
            .iter()
            .filter(|routed| matches!(
                peel_inner_event(&routed.frame),
                Some(Event::UiShellCommand(projected))
                    if projected.command_id == command.command_id
            ))
            .count(),
        1,
        "parked terminal keeps same-id reuse from reaching the UI"
    );

    h.handle_extension_event(
        "shell-terminal-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("must-pass terminal");
    assert!(!h.active_ui_shell_command_ids.contains(&command.command_id));

    h.handle_ui_shell_command("ui", command.clone());
    assert_eq!(h.pending_ui_shell_commands.len(), 1);
    assert_eq!(
        ui.lock()
            .expect("ui sink")
            .iter()
            .filter(|routed| matches!(
                peel_inner_event(&routed.frame),
                Some(Event::UiShellCommand(projected))
                    if projected.command_id == command.command_id
            ))
            .count(),
        2
    );

    let second_route = h
        .pending_ui_shell_commands
        .keys()
        .next()
        .expect("second route")
        .clone();
    h.canonicalize_committed_shell_command_report(
        provider_id.as_str(),
        Event::ShellCommandFinishedReported(tau_proto::ShellCommandFinished {
            command_id: second_route.as_protocol_id().clone(),
            session_id: command.session_id,
            command: command.command,
            include_in_context: false,
            target_agent_id: Some(agent_id),
            output: "second".to_owned(),
            exit_code: Some(0),
            cancelled: false,
        }),
    );
    let _ = intercepted_payload(&interceptor);
    h.handle_extension_event(
        "shell-terminal-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit second terminal");
    assert!(
        !h.active_ui_shell_command_ids
            .contains(&tau_proto::ShellCommandId::new("parked-ui-id"))
    );
}

#[test]
fn invalid_metadata_interceptor_replacements_fall_back_to_original() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "metadata-rewriter");
    h.handle_extension_event(
        "metadata-rewriter",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_METADATA_SET,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept registration");

    let agent_id = tau_proto::AgentId::parse("metadata-agent").expect("agent id");
    h.session_loaded_agents.insert(agent_id.clone());
    let original = Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
        agent_id: agent_id.clone(),
        key: tau_proto::AgentMetadataKey::new("valid"),
        value: CborValue::Text("ok".to_owned()),
        mutation_id: None,
        inheritable: true,
    });
    h.publish_event(None, original.clone());
    let replacement = Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
        agent_id,
        key: tau_proto::AgentMetadataKey::new("too-large"),
        value: CborValue::Bytes(vec![0; tau_proto::MAX_AGENT_METADATA_VALUE_BYTES + 1]),
        mutation_id: None,
        inheritable: true,
    });
    h.handle_extension_event(
        "metadata-rewriter",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(replacement))),
        })),
    )
    .expect("replace with invalid metadata");

    let events = event_log_events(&h);
    assert!(events.iter().any(|event| event == &original));
    assert!(!events.iter().any(|event| matches!(
        event,
        Event::AgentMetadataSet(set) if set.key.as_str() == "too-large"
    )));

    h.shutdown().expect("shutdown");
}
