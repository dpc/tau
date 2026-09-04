use super::*;
use crate::{event_log as path_crate_event_log, extension as path_crate_extension};

/// Build one extension prompt-fragment declaration with observable content.
fn prompt_fragment(name: &str, template: &str) -> Event {
    Event::ExtPromptFragmentPublish(tau_proto::ExtPromptFragmentPublish {
        fragment: tau_proto::PromptFragment::new(
            name,
            tau_proto::PromptPriority::new(10),
            template,
        ),
    })
}

/// Register the effective workdir capability required to consume the shared
/// shell fragment from one test contributor.
fn register_workdir_capability(h: &mut Harness, source: &str) {
    h.tool_routing.registry.register(
        &crate::test_connection_id(source),
        tau_proto::ToolSpec {
            name: tau_proto::ToolName::new(format!("{}_workdir", source.replace('-', "_"))),
            model_visible_name: None,
            description: None,
            tool_type: tau_proto::ToolType::Function,
            parameters: None,
            format: None,
            tags: vec![tau_proto::ToolTag::new("shell:workdir")],
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );
}

/// Return the current template for one source/name projection slot.
fn projected_template<'a>(h: &'a Harness, source: &str, name: &str) -> Option<&'a str> {
    h.prompt_coordination
        .context_discovery
        .prompt_fragments
        .get(source)
        .and_then(|fragments| fragments.get(name))
        .map(|fragment| fragment.template.as_str())
}

/// Collect committed prompt-fragment declarations for one fragment name.
fn committed_fragments(h: &Harness, name: &str) -> Vec<(Option<tau_proto::ConnectionId>, String)> {
    let mut events = Vec::new();
    let mut seq = path_crate_event_log::EventLogSeq::new(0);
    while let Some(entry) = h.runtime_io.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if let Event::ExtPromptFragmentPublish(publish) = entry.event
            && publish.fragment.name == name
        {
            events.push((entry.source, publish.fragment.template.to_string()));
        }
    }
    events
}

/// Connect one interceptor for extension prompt-fragment declarations.
fn connect_prompt_fragment_interceptor(h: &mut Harness) {
    connect_test_tool(h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::EXTENSION_PROMPT_FRAGMENT_PUBLISH,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
}

/// A dropped declaration remains absent from both the committed stream and
/// prompt assembly projection.
#[test]
fn dropped_prompt_fragment_does_not_mutate_projection() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "fragment-owner",
        "configured-fragment-owner",
        tau_proto::ClientKind::Tool,
    );
    connect_prompt_fragment_interceptor(&mut h);

    h.handle_extension_event_inner_with_persist(
        &crate::test_connection_id("fragment-owner"),
        prompt_fragment("test.drop", "DROPPED"),
        Some(true),
    )
    .expect("park declaration");
    assert_eq!(projected_template(&h, "fragment-owner", "test.drop"), None);
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop declaration");

    assert_eq!(projected_template(&h, "fragment-owner", "test.drop"), None);
    assert!(committed_fragments(&h, "test.drop").is_empty());
}

/// A same-name replacement is the only payload that commits and becomes
/// visible to prompt assembly.
#[test]
fn replacement_prompt_fragment_projects_only_after_commit() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "fragment-owner",
        "configured-fragment-owner",
        tau_proto::ClientKind::Provider,
    );
    connect_prompt_fragment_interceptor(&mut h);
    h.handle_extension_event_inner_with_persist(
        &crate::test_connection_id("fragment-owner"),
        prompt_fragment("test.replace", "ORIGINAL"),
        Some(true),
    )
    .expect("park declaration");

    assert_eq!(
        projected_template(&h, "fragment-owner", "test.replace"),
        None
    );
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(prompt_fragment(
                "test.replace",
                "REPLACEMENT",
            )))),
        })),
    )
    .expect("replace declaration");

    assert_eq!(
        projected_template(&h, "fragment-owner", "test.replace"),
        Some("REPLACEMENT")
    );
    assert_eq!(
        committed_fragments(&h, "test.replace"),
        vec![(
            Some(
                tau_proto::ConnectionId::parse("fragment-owner")
                    .expect("test connection id must satisfy the identifier grammar")
            ),
            "REPLACEMENT".to_owned()
        )]
    );
}

/// Every authenticated configured extension kind owns prompt-fragment slots
/// without a separate capability bit.
#[test]
fn every_configured_extension_kind_may_publish_prompt_fragments() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let kinds = [
        tau_proto::ClientKind::Provider,
        tau_proto::ClientKind::Tool,
        tau_proto::ClientKind::Action,
        tau_proto::ClientKind::Ui,
        tau_proto::ClientKind::Core,
        tau_proto::ClientKind::External,
    ];

    for (index, kind) in kinds.into_iter().enumerate() {
        let source = format!("configured-kind-{index}");
        connect_ready_configured_extension(&mut h, &source, &source, kind);
        h.handle_extension_event_inner_with_persist(
            &crate::test_connection_id(&source),
            prompt_fragment(&format!("test.kind.{index}"), &source),
            None,
        )
        .expect("publish configured fragment");
        assert_eq!(
            projected_template(&h, &source, &format!("test.kind.{index}")),
            Some(source.as_str())
        );
    }
}

/// A connected peer without authenticated configured identity cannot gain
/// prompt-fragment authority from its transport kind claim.
#[test]
fn unconfigured_peer_cannot_publish_prompt_fragment() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_test_client(&mut h, "unconfigured-core", tau_proto::ClientKind::Core);

    h.handle_extension_event_inner_with_persist(
        &crate::test_connection_id("unconfigured-core"),
        prompt_fragment("test.unauthorized", "UNAUTHORIZED"),
        None,
    )
    .expect("unauthorized declaration is ignored");

    assert_eq!(
        projected_template(&h, "unconfigured-core", "test.unauthorized"),
        None
    );
    assert!(committed_fragments(&h, "test.unauthorized").is_empty());
}

/// A pre-Ready declaration reserves activation through interception so Ready
/// cannot expose a prompt fragment before its declaration commits.
#[test]
fn parked_startup_prompt_fragment_blocks_ready_until_commit() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "fragment-owner",
        "configured-fragment-owner",
        tau_proto::ClientKind::Core,
    );
    h.extensions
        .entries
        .get_mut("fragment-owner")
        .expect("fragment owner")
        .state = path_crate_extension::ExtensionState::Handshaking;
    connect_prompt_fragment_interceptor(&mut h);

    h.handle_extension_event(
        "fragment-owner",
        TestProtocolItem::Event(prompt_fragment("test.startup", "STARTUP")),
    )
    .expect("park declaration");
    h.handle_extension_message(
        &crate::test_connection_id("fragment-owner"),
        TestMessage::Ready(Default::default()),
    )
    .expect("Ready waits");

    assert_eq!(
        h.extensions.entries["fragment-owner"].state,
        crate::extension::ExtensionState::Handshaking
    );
    assert_eq!(
        h.extensions
            .pending_prompt_fragment_declarations
            .get("fragment-owner"),
        Some(&1)
    );
    assert_eq!(
        projected_template(&h, "fragment-owner", "test.startup"),
        None
    );

    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit declaration");

    assert_eq!(
        h.extensions.entries["fragment-owner"].state,
        crate::extension::ExtensionState::Ready
    );
    assert_eq!(
        projected_template(&h, "fragment-owner", "test.startup"),
        Some("STARTUP")
    );
    assert!(
        !h.extensions
            .pending_prompt_fragment_declarations
            .contains_key("fragment-owner")
    );
}

/// Multiple pre-Ready declarations each commit in wire order while activation
/// coalesces one source/name slot to the final committed value.
#[test]
fn startup_prompt_fragment_commits_all_updates_and_activates_latest() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "fragment-owner",
        "configured-fragment-owner",
        tau_proto::ClientKind::Tool,
    );
    h.extensions
        .entries
        .get_mut("fragment-owner")
        .expect("fragment owner")
        .state = path_crate_extension::ExtensionState::Handshaking;

    for template in ["FIRST", "SECOND"] {
        h.handle_extension_event(
            "fragment-owner",
            TestProtocolItem::Event(prompt_fragment("test.coalesced", template)),
        )
        .expect("commit startup declaration");
    }

    assert_eq!(
        committed_fragments(&h, "test.coalesced"),
        vec![
            (
                Some(
                    tau_proto::ConnectionId::parse("fragment-owner")
                        .expect("test connection id must satisfy the identifier grammar")
                ),
                "FIRST".to_owned()
            ),
            (
                Some(
                    tau_proto::ConnectionId::parse("fragment-owner")
                        .expect("test connection id must satisfy the identifier grammar")
                ),
                "SECOND".to_owned()
            )
        ]
    );
    assert_eq!(
        projected_template(&h, "fragment-owner", "test.coalesced"),
        None
    );
    assert_eq!(
        h.extensions.activation_staging["fragment-owner"].prompt_fragments["test.coalesced"]
            .template
            .as_str(),
        "SECOND"
    );

    h.handle_extension_message(
        &crate::test_connection_id("fragment-owner"),
        TestMessage::Ready(Default::default()),
    )
    .expect("activate final fragment");

    assert_eq!(
        projected_template(&h, "fragment-owner", "test.coalesced"),
        Some("SECOND")
    );
}

/// Dropping a parked startup declaration releases its reservation and permits
/// Ready without activating the dropped fragment.
#[test]
fn dropped_startup_prompt_fragment_releases_activation_reservation() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "fragment-owner",
        "configured-fragment-owner",
        tau_proto::ClientKind::Tool,
    );
    h.extensions
        .entries
        .get_mut("fragment-owner")
        .expect("fragment owner")
        .state = path_crate_extension::ExtensionState::Handshaking;
    connect_prompt_fragment_interceptor(&mut h);
    h.handle_extension_event(
        "fragment-owner",
        TestProtocolItem::Event(prompt_fragment("test.startup.drop", "DROP")),
    )
    .expect("park declaration");
    h.handle_extension_message(
        &crate::test_connection_id("fragment-owner"),
        TestMessage::Ready(Default::default()),
    )
    .expect("Ready waits");

    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop declaration");

    assert_eq!(
        h.extensions.entries["fragment-owner"].state,
        crate::extension::ExtensionState::Ready
    );
    assert!(
        !h.extensions
            .pending_prompt_fragment_declarations
            .contains_key("fragment-owner")
    );
    let stage = h.extensions.activation_staging.get("fragment-owner");
    assert!(stage.is_none());
    assert_eq!(
        projected_template(&h, "fragment-owner", "test.startup.drop"),
        None
    );
}

/// A replacement is charged at its committed encoded size and an overflow
/// releases the declaration reservation rather than exposing oversized prompt
/// state.
#[test]
fn oversized_startup_prompt_fragment_replacement_fails_activation() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    h.extensions.initial_tool_preflight_complete = false;
    connect_ready_configured_extension(
        &mut h,
        "fragment-owner",
        "configured-fragment-owner",
        tau_proto::ClientKind::Tool,
    );
    h.extensions
        .entries
        .get_mut("fragment-owner")
        .expect("fragment owner")
        .state = path_crate_extension::ExtensionState::Handshaking;
    connect_prompt_fragment_interceptor(&mut h);
    h.handle_extension_event(
        "fragment-owner",
        TestProtocolItem::Event(prompt_fragment("test.overflow", "small")),
    )
    .expect("park declaration");

    let error = h
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(Some(Box::new(prompt_fragment(
                    "test.overflow",
                    &"x".repeat(crate::harness::MAX_EXTENSION_ACTIVATION_BYTES),
                )))),
            })),
        )
        .expect_err("oversized replacement must fail required startup");

    assert!(error.to_string().contains("activation staging exceeds"));
    assert_eq!(
        projected_template(&h, "fragment-owner", "test.overflow"),
        None
    );
    assert!(
        !h.extensions
            .pending_prompt_fragment_declarations
            .contains_key("fragment-owner")
    );
    let stage = &h.extensions.activation_staging["fragment-owner"];
    assert_eq!(stage.retained_message_count, 0);
    assert_eq!(stage.retained_message_bytes, 0);
}

/// Late subscribers receive no synthesized or historical prompt-fragment state,
/// even when the publisher requested non-transient delivery.
#[test]
fn prompt_fragment_declaration_has_no_late_subscriber_replay() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "fragment-owner",
        "configured-fragment-owner",
        tau_proto::ClientKind::Tool,
    );
    h.handle_extension_event_inner_with_persist(
        &crate::test_connection_id("fragment-owner"),
        prompt_fragment("test.no-replay", "LIVE ONLY"),
        Some(true),
    )
    .expect("commit declaration");

    let observer = connect_test_client(&mut h, "late-observer", tau_proto::ClientKind::Ui);
    h.handle_client_message(
        &crate::test_connection_id("late-observer"),
        TestMessage::Subscribe(Subscribe {
            historical_selectors: vec![EventSelector::Exact(
                tau_proto::EventName::EXTENSION_PROMPT_FRAGMENT_PUBLISH,
            )],
            live_selectors: vec![EventSelector::Exact(
                tau_proto::EventName::EXTENSION_PROMPT_FRAGMENT_PUBLISH,
            )],
        })
        .into_input_message(),
    )
    .expect("subscribe");

    assert!(
        observer
            .lock()
            .expect("observer")
            .iter()
            .all(|routed| !matches!(
                peel_inner_event(&routed.frame),
                Some(Event::ExtPromptFragmentPublish(_))
            ))
    );
}

/// The prompt consumer keeps one cross-source `shell.workdir` fragment and
/// exposes the remaining committed source after the first disconnects.
#[test]
fn shell_workdir_visibility_recomputes_after_contributor_disconnect() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    for (source, template) in [("shell-a", "WORKDIR A"), ("shell-b", "WORKDIR B")] {
        connect_ready_configured_extension(&mut h, source, source, tau_proto::ClientKind::Tool);
        register_workdir_capability(&mut h, source);
        h.handle_extension_event_inner_with_persist(
            &crate::test_connection_id(source),
            prompt_fragment("shell.workdir", template),
            None,
        )
        .expect("commit workdir fragment");
    }

    let visible = h
        .gather_prompt_fragments()
        .into_iter()
        .filter(|fragment| fragment.name == "shell.workdir")
        .collect::<Vec<_>>();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].template.as_str(), "WORKDIR A");

    h.handle_disconnect(&crate::test_connection_id("shell-a"));

    let visible = h
        .gather_prompt_fragments()
        .into_iter()
        .filter(|fragment| fragment.name == "shell.workdir")
        .collect::<Vec<_>>();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].template.as_str(), "WORKDIR B");

    connect_ready_configured_extension(&mut h, "shell-0", "shell-a", tau_proto::ClientKind::Tool);
    register_workdir_capability(&mut h, "shell-0");
    h.handle_extension_event_inner_with_persist(
        &crate::test_connection_id("shell-0"),
        prompt_fragment("shell.workdir", "WORKDIR RESPAWN"),
        None,
    )
    .expect("commit respawned workdir fragment");

    let visible = h
        .gather_prompt_fragments()
        .into_iter()
        .filter(|fragment| fragment.name == "shell.workdir")
        .collect::<Vec<_>>();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].template.as_str(), "WORKDIR RESPAWN");
}

/// A declaration from a disconnected connection generation may still commit
/// as an observation but cannot mutate a replacement generation's slots.
#[test]
fn stale_prompt_fragment_generation_cannot_mutate_projection() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "old-generation",
        "configured-fragment-owner",
        tau_proto::ClientKind::Tool,
    );
    connect_prompt_fragment_interceptor(&mut h);
    h.handle_extension_event_inner_with_persist(
        &crate::test_connection_id("old-generation"),
        prompt_fragment("test.stale", "STALE"),
        None,
    )
    .expect("park stale declaration");

    h.handle_disconnect(&crate::test_connection_id("old-generation"));
    connect_ready_configured_extension(
        &mut h,
        "new-generation",
        "configured-fragment-owner",
        tau_proto::ClientKind::Tool,
    );
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit stale observation");

    assert_eq!(projected_template(&h, "new-generation", "test.stale"), None);
    assert_eq!(
        committed_fragments(&h, "test.stale"),
        vec![(
            Some(
                tau_proto::ConnectionId::parse("old-generation")
                    .expect("test connection id must satisfy the identifier grammar")
            ),
            "STALE".to_owned()
        )]
    );
}

/// Resolving a parked pre-Ready declaration from a disconnected generation
/// cannot release or reaccount the successor generation's reservation.
#[test]
fn stale_startup_generation_cannot_consume_successor_reservation() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "old-generation",
        "configured-fragment-owner",
        tau_proto::ClientKind::Tool,
    );
    h.extensions
        .entries
        .get_mut("old-generation")
        .expect("old generation")
        .state = path_crate_extension::ExtensionState::Handshaking;
    connect_prompt_fragment_interceptor(&mut h);
    h.handle_extension_event(
        "old-generation",
        TestProtocolItem::Event(prompt_fragment("test.generation", "STALE")),
    )
    .expect("park old declaration");

    h.handle_disconnect(&crate::test_connection_id("old-generation"));
    connect_ready_configured_extension(
        &mut h,
        "new-generation",
        "configured-fragment-owner",
        tau_proto::ClientKind::Tool,
    );
    h.extensions
        .entries
        .get_mut("new-generation")
        .expect("new generation")
        .state = path_crate_extension::ExtensionState::Handshaking;
    h.handle_extension_event(
        "new-generation",
        TestProtocolItem::Event(prompt_fragment("test.generation", "CURRENT")),
    )
    .expect("queue successor declaration");
    h.handle_extension_message(
        &crate::test_connection_id("new-generation"),
        TestMessage::Ready(Default::default()),
    )
    .expect("Ready waits");
    assert_eq!(
        h.extensions
            .pending_prompt_fragment_declarations
            .get("new-generation"),
        Some(&1)
    );

    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit stale declaration");

    assert_eq!(
        h.extensions
            .pending_prompt_fragment_declarations
            .get("new-generation"),
        Some(&1)
    );
    assert_eq!(
        h.extensions.entries["new-generation"].state,
        crate::extension::ExtensionState::Handshaking
    );
    assert_eq!(
        projected_template(&h, "new-generation", "test.generation"),
        None
    );

    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit successor declaration");

    assert_eq!(
        h.extensions.entries["new-generation"].state,
        crate::extension::ExtensionState::Ready
    );
    assert_eq!(
        projected_template(&h, "new-generation", "test.generation"),
        Some("CURRENT")
    );
}
