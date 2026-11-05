//! Canonical-main bootstrap admission, independent of ordinary UI creation.

use super::*;
use crate::agent::PendingPrompt;

/// Submit the same private, literal create request used by the serve worker.
fn bootstrap(h: &mut Harness, id: &str) {
    let request_id = format!("bootstrap-{id}");
    h.register_bootstrap_create(request_id.clone(), id.to_owned());
    h.handle_ui_create_agent_from(
        &crate::test_connection_id("bootstrap-ui"),
        tau_proto::UiCreateAgent {
            request_id,
            session_id: h.session_runtime.current_session_id.clone(),
            role: h.bootstrap_role().expect("bootstrap role"),
            model_override: None,
            effort_override: None,
            metadata: Vec::new(),
            initial_prompt: Some(format!("literal bootstrap {id}\n")),
            literal: true,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: Some(format!("bootstrap-{id}-prompt")),
            parent_agent: None,
            ephemeral: false,
        },
    )
    .expect("bootstrap admission");
}

/// UI/peer-first and bootstrap-first demand select the same main; subsequent
/// generations reuse it and ordinary same-role creation remains independent.
#[test]
fn canonical_bootstrap_reuses_main_and_preserves_ordinary_creation() {
    for main_first in [false, true] {
        let td = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(td.path()).expect("harness");
        configure_main_role(&mut h, "engineer");
        let existing = main_first.then(|| h.ensure_main_agent().expect("ensure").expect("main"));
        bootstrap(&mut h, "v1");
        let main = h.main_agent_id().cloned().expect("canonical binding");
        assert!(existing.is_none_or(|existing| existing == main));
        assert_eq!(
            h.ensure_main_agent().expect("UI/peer demand"),
            Some(main.clone())
        );
        assert_eq!(
            h.bootstrap_agent_for_id("v1").expect("marker"),
            Some(main.clone())
        );
        bootstrap(&mut h, "v1");
        bootstrap(&mut h, "v2");
        assert_eq!(
            h.bootstrap_agent_for_id("v2").expect("marker"),
            Some(main.clone())
        );
        assert_eq!(h.agent_runtime.agent_registry.agents.len(), 1);
        let records = h
            .session_runtime
            .agent_store
            .snapshot_agent_events_for_test(main.as_str())
            .expect("journal");
        assert!(records.iter().any(|record| matches!(&record.event,
            Event::AgentPromptSubmitted(prompt) if prompt.text == "literal bootstrap v1\n"
                && prompt.agent_id == main)));
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(&record.event,
            Event::AgentMetadataSet(set) if set.key.as_str() == "tau.bootstrap_prompt.v1"))
                .count(),
            1
        );
        let worker =
            h.create_durable_user_agent(h.session_runtime.current_session_id.clone(), "engineer");
        assert_ne!(worker, main);
        assert_eq!(h.main_agent_id(), Some(&main));
        h.shutdown().expect("shutdown");
        drop(h);
        let mut resumed = quiet_provider_harness_with_start_reason(
            td.path(),
            tau_proto::SessionStartReason::Resume,
        )
        .expect("resume");
        assert_eq!(resumed.main_agent_id(), Some(&main));
        for id in ["v1", "v2"] {
            assert_eq!(
                resumed.bootstrap_agent_for_id(id).expect("cold admission"),
                Some(main.clone())
            );
        }
        resumed.shutdown().expect("shutdown resumed");
    }
}

/// An intercepted main load is shared by bootstrap and concurrent UI/peer
/// demand; bootstrap cannot publish admission before the binding commits.
#[test]
fn canonical_bootstrap_waits_for_shared_main_candidate() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    configure_main_role(&mut h, "engineer");
    connect_test_tool(&mut h, "bootstrap-gate");
    h.handle_extension_event(
        "bootstrap-gate",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::SESSION_AGENT_LOADED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept main membership");
    bootstrap(&mut h, "shared");
    assert!(h.main_agent_id().is_none());
    assert!(
        h.bootstrap_agent_for_id("shared")
            .expect("no admission")
            .is_none()
    );
    let candidate = h
        .ensure_main_agent()
        .expect("concurrent demand")
        .expect("candidate");
    assert_eq!(h.agent_runtime.agent_registry.agents.len(), 1);
    h.handle_extension_event(
        "bootstrap-gate",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release membership");
    assert_eq!(h.main_agent_id(), Some(&candidate));
    assert_eq!(
        h.bootstrap_agent_for_id("shared").expect("admission"),
        Some(candidate)
    );
    assert!(h.ui_runtime.pending_main_bootstrap.is_none());
    h.shutdown().expect("shutdown");
}

/// A crash before marker acceptance leaves the generation eligible; a crash
/// after marker acceptance but before interaction/prompt never reconstructs
/// work.
#[test]
fn canonical_bootstrap_restart_cuts_do_not_reconstruct_admitted_work() {
    for marker_committed in [false, true] {
        let td = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(td.path()).expect("harness");
        configure_main_role(&mut h, "engineer");
        let main = h.ensure_main_agent().expect("ensure").expect("main");
        connect_test_tool(&mut h, "bootstrap-gate");
        h.handle_extension_event(
            "bootstrap-gate",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(if marker_committed {
                    tau_proto::EventName::AGENT_USER_INTERACTION_RECORDED
                } else {
                    tau_proto::EventName::AGENT_METADATA_SET
                })],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("intercept bootstrap phase");
        bootstrap(&mut h, "cut");
        assert_eq!(
            h.bootstrap_agent_for_id("cut").expect("live marker"),
            marker_committed.then_some(main.clone())
        );
        let records = h
            .session_runtime
            .agent_store
            .snapshot_agent_events_for_test(main.as_str())
            .expect("journal");
        assert!(!records.iter().any(|record| matches!(
            record.event,
            Event::AgentPromptSubmitted(_) | Event::AgentUserInteractionRecorded(_)
        )));
        h.shutdown().expect("shutdown at publication cut");
        drop(h);
        let mut resumed = quiet_provider_harness_with_start_reason(
            td.path(),
            tau_proto::SessionStartReason::Resume,
        )
        .expect("cold resume");
        assert_eq!(
            resumed.bootstrap_agent_for_id("cut").expect("cold marker"),
            marker_committed.then_some(main.clone())
        );
        let records = resumed
            .session_runtime
            .agent_store
            .snapshot_agent_events_for_test(main.as_str())
            .expect("cold journal");
        assert!(!records.iter().any(|record| matches!(
            record.event,
            Event::AgentPromptSubmitted(_) | Event::AgentUserInteractionRecorded(_)
        )));
        resumed.shutdown().expect("shutdown resumed");
    }
}

/// Explicit conflicting startup roles fail before creating any agent, while
/// the retained main's role wins over a changed priority configuration.
#[test]
fn canonical_bootstrap_role_conflicts_fail_without_creating_agents() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    configure_main_role(&mut h, "engineer");
    h.config.accepted_harness_settings.startup_role = Some("other".to_owned());
    assert!(h.bootstrap_role().is_err());
    assert!(h.agent_runtime.agent_registry.agents.is_empty());
    h.config.accepted_harness_settings.startup_role = None;
    assert_eq!(h.bootstrap_role().expect("implicit role"), "engineer");
    let main = h.ensure_main_agent().expect("ensure").expect("main");
    configure_main_role(&mut h, "no-longer-configured");
    assert_eq!(h.bootstrap_role().expect("retained main role"), "engineer");
    assert_eq!(h.main_agent_id(), Some(&main));
    h.shutdown().expect("shutdown");
}

/// Ordinary metadata clients cannot forge or clear restart suppression, and
/// invalid retained admission metadata fails closed rather than rerunning work.
#[test]
fn canonical_bootstrap_admission_metadata_is_reserved_and_validated() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    configure_main_role(&mut h, "engineer");
    let main = h.ensure_main_agent().expect("ensure").expect("main");
    let key = tau_proto::AgentMetadataKey::new("tau.bootstrap_prompt.reserved");
    let set = tau_proto::AgentMetadataSet {
        agent_id: main.clone(),
        key: key.clone(),
        value: CborValue::Bool(true),
        inheritable: false,
        mutation_id: None,
    };
    assert!(h.validate_agent_metadata_set(&set).is_err());
    assert!(
        h.validate_agent_metadata_unset(&tau_proto::AgentMetadataUnset {
            agent_id: main.clone(),
            key: key.clone(),
        })
        .is_err()
    );
    assert!(
        h.validate_initial_agent_metadata(&[tau_proto::AgentInitialMetadata {
            key,
            value: CborValue::Bool(true),
            inheritable: false,
        }])
        .is_err()
    );
    h.publish_event(
        None,
        Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
            value: CborValue::Bool(false),
            ..set
        }),
    );
    assert!(h.bootstrap_agent_for_id("reserved").is_err());
    h.shutdown().expect("shutdown");
}

/// A delayed main initialization failure rejects bootstrap and retires its
/// pending owner instead of hanging admission or creating a fallback agent.
#[test]
fn canonical_bootstrap_main_failure_releases_pending_admission() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    configure_main_role(&mut h, "engineer");
    let client = connect_test_client(&mut h, "bootstrap-ui", tau_proto::ClientKind::Ui);
    connect_test_tool(&mut h, "bootstrap-gate");
    h.handle_extension_event(
        "bootstrap-gate",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::SESSION_AGENT_LOADED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("hold membership");
    bootstrap(&mut h, "failed");
    h.config
        .available_roles
        .get_mut("engineer")
        .expect("role")
        .required_skills = vec!["missing-bootstrap-main-skill".into()];
    h.handle_extension_event(
        "bootstrap-gate",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release membership");
    assert!(h.ui_runtime.pending_main_bootstrap.is_none());
    assert!(
        h.bootstrap_agent_for_id("failed")
            .expect("no marker")
            .is_none()
    );
    assert!(client.lock().expect("client").iter().any(
        |frame| matches!(peel_inner_event(&frame.frame), Some(Event::UiCreateAgentResult(result))
            if matches!(result.outcome, tau_proto::UiCreateAgentOutcome::Rejected { .. }))
    ));
    h.shutdown().expect("shutdown");
}

/// Bootstrap folded into a busy main's tool continuation labels that exact
/// materialization, rather than losing correlation when eager assignment is
/// removed.
#[test]
fn canonical_bootstrap_tool_fold_preserves_prompt_correlation() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    configure_main_role(&mut h, "engineer");
    let main = h.ensure_main_agent().expect("ensure").expect("main");
    h.dispatch_prompt_for_agent(&main, PendingPrompt::human_ui("existing turn".to_owned()))
        .expect("existing input");
    let first = read_nth_prompt_created(&h, 0);
    assert!(
        h.agent_runtime.agent_registry.agents[&main]
            .dispatch
            .next_ctx_id
            .is_none()
    );
    bootstrap(&mut h, "busy");
    assert!(
        h.agent_runtime.agent_registry.agents[&main]
            .dispatch
            .next_ctx_id
            .is_none(),
        "queued bootstrap cannot label an unrelated materialization"
    );
    h.handle_provider_response_finished(dispatch::provider_tool_response(
        &first,
        "busy-status",
        "status",
        CborValue::Map(vec![
            (
                CborValue::Text("state".into()),
                CborValue::Text("working".into()),
            ),
            (
                CborValue::Text("task_name".into()),
                CborValue::Text("finish existing turn".into()),
            ),
        ]),
    ))
    .expect("complete tool round and fold bootstrap");
    let folded = read_nth_prompt_created(&h, 1);
    assert_eq!(folded.ctx_id.as_deref(), Some("bootstrap-busy-prompt"));
    assert!(event_log_events(&h).iter().any(|event| matches!(event,
        Event::AgentPromptSteered(prompt) if prompt.ctx_id.as_deref() == Some("bootstrap-busy-prompt")
            && prompt.text == "literal bootstrap busy\n")));
    h.shutdown().expect("shutdown");
}

/// A bootstrap on an idle main with completed output is fresh, not delayed by
/// responses that predate the bootstrap's acceptance.
#[test]
fn canonical_bootstrap_uses_existing_main_response_baseline() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    configure_main_role(&mut h, "engineer");
    let main = h.ensure_main_agent().expect("ensure").expect("main");
    h.dispatch_prompt_for_agent(&main, PendingPrompt::human_ui("prior input".to_owned()))
        .expect("prior input");
    let first = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(dispatch::provider_text_response(
        &first.agent_prompt_id,
        main.clone(),
        "prior completed output",
    ))
    .expect("finish prior response");
    assert!(
        h.agent_runtime.agent_registry.agents[&main]
            .dispatch
            .visible_response_generation
            > 0
    );
    bootstrap(&mut h, "fresh");
    assert!(event_log_events(&h).iter().any(|event| matches!(event,
        Event::AgentPromptSubmitted(prompt) if prompt.ctx_id.as_deref() == Some("bootstrap-fresh-prompt")
            && !prompt.delayed)));
    h.shutdown().expect("shutdown");
}

/// Malformed private bootstrap requests fail before main construction or
/// installation of an admission owner, rather than panicking later.
#[test]
fn canonical_bootstrap_missing_prompt_rejects_before_main_creation() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    configure_main_role(&mut h, "engineer");
    let client = connect_test_client(&mut h, "bootstrap-ui", tau_proto::ClientKind::Ui);
    h.register_bootstrap_create("missing".to_owned(), "missing".to_owned());
    h.handle_ui_create_agent_from(
        &crate::test_connection_id("bootstrap-ui"),
        tau_proto::UiCreateAgent {
            request_id: "missing".to_owned(),
            session_id: h.session_runtime.current_session_id.clone(),
            role: "engineer".to_owned(),
            model_override: None,
            effort_override: None,
            metadata: Vec::new(),
            initial_prompt: None,
            literal: true,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
            parent_agent: None,
            ephemeral: false,
        },
    )
    .expect("directed rejection");
    assert!(h.main_agent_id().is_none());
    assert!(h.agent_runtime.agent_registry.agents.is_empty());
    assert!(h.ui_runtime.pending_main_bootstrap.is_none());
    assert!(client.lock().expect("client").iter().any(
        |frame| matches!(peel_inner_event(&frame.frame), Some(Event::UiCreateAgentResult(result))
            if matches!(result.outcome, tau_proto::UiCreateAgentOutcome::Rejected { .. }))
    ));
    h.shutdown().expect("shutdown");
}
