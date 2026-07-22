use super::*;

fn submitted(prompt_id: &str) -> Event {
    Event::ProviderPromptSubmittedReported(tau_proto::ProviderPromptSubmitted {
        agent_prompt_id: prompt_id.into(),
        originator: tau_proto::PromptOriginator::User,
    })
}

fn prompt_submission_events(
    harness: &Harness,
) -> Vec<(Option<tau_proto::ConnectionId>, tau_proto::EventName)> {
    let mut events = Vec::new();
    let mut seq = crate::event_log::EventLogSeq::new(0);
    while let Some(entry) = harness.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if matches!(
            entry.event,
            Event::ProviderPromptSubmittedReported(_) | Event::ProviderPromptSubmitted(_)
        ) {
            events.push((entry.source, entry.event.name()));
        }
    }
    events
}

fn committed_events(harness: &Harness) -> Vec<(Option<tau_proto::ConnectionId>, Event)> {
    let mut events = Vec::new();
    let mut seq = crate::event_log::EventLogSeq::new(0);
    while let Some(entry) = harness.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        events.push((entry.source, entry.event));
    }
    events
}

/// A configured Provider report is observable before its separately authored
/// canonical fact, and both records retain exact source authority.
#[test]
fn prompt_submitted_report_commits_before_harness_canonical_fact() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "provider",
        "provider",
        tau_proto::ClientKind::Provider,
    );
    harness
        .pending_provider_prompts
        .insert("prompt-1".into(), "provider".into());

    harness
        .handle_extension_event_inner_with_persist("provider", submitted("prompt-1"), Some(true))
        .expect("submit report");

    assert_eq!(
        prompt_submission_events(&harness),
        [
            (
                Some("provider".into()),
                tau_proto::EventName::PROVIDER_PROMPT_SUBMITTED_REPORTED,
            ),
            (
                Some(HARNESS_CONNECTION_ID.into()),
                tau_proto::EventName::PROVIDER_PROMPT_SUBMITTED,
            ),
        ]
    );
    assert!(
        harness
            .store
            .session_restore_events(harness.current_session_id.as_str())
            .expect("restore events")
            .is_empty(),
        "explicit persist=true must not make reports restore facts"
    );
}

/// Pre-Ready execution reports retain their complete `Emit` envelope and only
/// cross the generic commit/correlation boundary after activation.
#[test]
fn pre_ready_provider_execution_report_retains_persistence_envelope() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "provider",
        "provider",
        tau_proto::ClientKind::Provider,
    );
    harness
        .extensions
        .entries
        .get_mut("provider")
        .expect("provider")
        .state = crate::extension::ExtensionState::Handshaking;
    harness
        .pending_provider_prompts
        .insert("prompt-1".into(), "provider".into());
    let report = submitted("prompt-1");
    let expected_bytes = Harness::encoded_emit_size(&report, false);

    harness
        .handle_extension_event(
            "provider",
            TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
                event: Box::new(report),
                persist: false,
            })),
        )
        .expect("stage report");
    let stage = &harness.extensions.activation_staging["provider"];
    assert_eq!(stage.retained_message_count, 1);
    assert_eq!(stage.retained_message_bytes, expected_bytes);
    assert!(prompt_submission_events(&harness).is_empty());

    harness
        .handle_extension_message("provider", TestMessage::Ready(Default::default()))
        .expect("activate and drain report");
    assert!(matches!(
        prompt_submission_events(&harness).as_slice(),
        [
            (Some(report_source), reported),
            (Some(canonical_source), canonical),
        ] if report_source.as_str() == "provider"
            && reported == &tau_proto::EventName::PROVIDER_PROMPT_SUBMITTED_REPORTED
            && canonical_source.as_str() == HARNESS_CONNECTION_ID
            && canonical == &tau_proto::EventName::PROVIDER_PROMPT_SUBMITTED
    ));
    assert!(
        harness
            .store
            .session_restore_events(harness.current_session_id.as_str())
            .expect("restore events")
            .is_empty(),
        "the staged persist=false envelope must remain live-only"
    );
}

/// Canonical provider facts reject peer authorship, while an unconfigured
/// Provider-kind claim cannot submit the corresponding report.
#[test]
fn provider_execution_authority_is_default_deny() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    connect_test_client(
        &mut harness,
        "kind-only-provider",
        tau_proto::ClientKind::Provider,
    );
    connect_ready_configured_extension(
        &mut harness,
        "configured-provider",
        "configured-provider",
        tau_proto::ClientKind::Provider,
    );
    harness
        .pending_provider_prompts
        .insert("prompt-1".into(), "configured-provider".into());

    harness
        .handle_extension_event_inner_with_persist(
            "kind-only-provider",
            submitted("prompt-1"),
            Some(false),
        )
        .expect("kind-only report");
    harness
        .handle_extension_event_inner_with_persist(
            "configured-provider",
            Event::ProviderPromptSubmitted(tau_proto::ProviderPromptSubmitted {
                agent_prompt_id: "prompt-1".into(),
                originator: tau_proto::PromptOriginator::User,
            }),
            Some(false),
        )
        .expect("canonical spoof");

    assert!(prompt_submission_events(&harness).is_empty());
}

/// Cache diagnostics intentionally remain owner-valid after cancellation until
/// the eventual terminal response removes the prompt route.
#[test]
fn canceled_prompt_still_accepts_owned_cache_diagnostic() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "provider",
        "provider",
        tau_proto::ClientKind::Provider,
    );
    harness
        .pending_provider_prompts
        .insert("prompt-1".into(), "provider".into());
    harness.canceled_prompts.insert("prompt-1".into());
    let diagnostic = tau_proto::ProviderCacheMissDiagnostic {
        agent_prompt_id: "prompt-1".into(),
        model: "provider/model".into(),
        originator: tau_proto::PromptOriginator::User,
        tool_choice: tau_proto::ToolChoice::default(),
        ws_pool_delta: None,
        input_tokens: 1,
        cached_tokens: 0,
        previous_input_tokens: 1,
        cacheable_input_tokens: 1,
        corrected_cache_efficiency: 0.0,
    };

    harness
        .handle_extension_event_inner(
            "provider",
            Event::ProviderCacheMissDiagnosticReported(diagnostic),
        )
        .expect("cache report");

    let mut seq = crate::event_log::EventLogSeq::new(0);
    let mut found = false;
    while let Some(entry) = harness.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        found |= entry.source.as_deref() == Some(HARNESS_CONNECTION_ID)
            && matches!(entry.event, Event::ProviderCacheMissDiagnostic(_));
    }
    assert!(found);

    harness.pending_provider_prompts.remove("prompt-1");
    harness
        .handle_extension_event_inner(
            "provider",
            Event::ProviderCacheMissDiagnosticReported(tau_proto::ProviderCacheMissDiagnostic {
                agent_prompt_id: "prompt-1".into(),
                model: "provider/model".into(),
                originator: tau_proto::PromptOriginator::User,
                tool_choice: tau_proto::ToolChoice::default(),
                ws_pool_delta: None,
                input_tokens: 2,
                cached_tokens: 0,
                previous_input_tokens: 1,
                cacheable_input_tokens: 2,
                corrected_cache_efficiency: 0.0,
            }),
        )
        .expect("post-closure cache report");
    let canonical_count = committed_events(&harness)
        .iter()
        .filter(|(_, event)| matches!(event, Event::ProviderCacheMissDiagnostic(_)))
        .count();
    assert_eq!(
        canonical_count, 1,
        "a report after route closure must remain observation-only"
    );
}

/// Submitted and updated observations still commit after cancellation, but the
/// canceled prompt cannot derive canonical execution facts.
#[test]
fn canceled_submitted_and_updated_reports_are_observation_only() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "provider",
        "provider",
        tau_proto::ClientKind::Provider,
    );
    harness
        .pending_provider_prompts
        .insert("prompt-1".into(), "provider".into());
    harness.canceled_prompts.insert("prompt-1".into());

    harness
        .handle_extension_event_inner("provider", submitted("prompt-1"))
        .expect("submitted report");
    harness
        .handle_extension_event_inner(
            "provider",
            Event::ProviderResponseUpdatedReported(tau_proto::ProviderResponseUpdated {
                agent_prompt_id: "prompt-1".into(),
                agent_id: crate::parse_agent_id("spoofed"),
                deltas: vec![tau_proto::ProviderResponseTextDelta::Message {
                    output_index: 0,
                    text: "ignored".to_owned(),
                    phase: None,
                }],
                compaction: None,
                status: None,
                response_stats: None,
                originator: tau_proto::PromptOriginator::User,
            }),
        )
        .expect("updated report");

    let events = committed_events(&harness)
        .iter()
        .map(|(_, event)| event.name())
        .collect::<Vec<_>>();
    assert!(events.contains(&tau_proto::EventName::PROVIDER_PROMPT_SUBMITTED_REPORTED));
    assert!(events.contains(&tau_proto::EventName::PROVIDER_RESPONSE_UPDATED_REPORTED));
    assert!(!events.contains(&tau_proto::EventName::PROVIDER_PROMPT_SUBMITTED));
    assert!(!events.contains(&tau_proto::EventName::PROVIDER_RESPONSE_UPDATED));
}

/// An update cannot use its provider-claimed agent id when the harness has no
/// prompt-to-agent correlation.
#[test]
fn response_update_without_harness_agent_identity_is_observation_only() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "provider",
        "provider",
        tau_proto::ClientKind::Provider,
    );
    harness
        .pending_provider_prompts
        .insert("prompt-1".into(), "provider".into());
    harness
        .handle_extension_event_inner(
            "provider",
            Event::ProviderResponseUpdatedReported(tau_proto::ProviderResponseUpdated {
                agent_prompt_id: "prompt-1".into(),
                agent_id: crate::parse_agent_id("spoofed"),
                deltas: vec![tau_proto::ProviderResponseTextDelta::Message {
                    output_index: 0,
                    text: "ignored".to_owned(),
                    phase: None,
                }],
                compaction: None,
                status: None,
                response_stats: None,
                originator: tau_proto::PromptOriginator::User,
            }),
        )
        .expect("updated report");

    assert!(
        committed_events(&harness)
            .iter()
            .any(|(_, event)| matches!(event, Event::ProviderResponseUpdatedReported(_)))
    );
    assert!(
        committed_events(&harness)
            .iter()
            .all(|(_, event)| !matches!(event, Event::ProviderResponseUpdated(_)))
    );
}

/// A terminal report commits before a parked canonical response while terminal
/// route cleanup proceeds non-transactionally; release retains harness source.
#[test]
fn finished_report_parks_canonical_after_terminal_side_effects() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "provider",
        "provider",
        tau_proto::ClientKind::Provider,
    );
    let cid = ensure_test_user_agent(&mut harness);
    seed_agent_thinking(&mut harness, &cid, "prompt-1");
    harness.prompt_agents.insert("prompt-1".into(), cid.clone());
    harness
        .agents
        .get_mut(&cid)
        .expect("agent")
        .in_flight_prompt = Some("prompt-1".into());
    harness.agents.get_mut(&cid).expect("agent").last_prompt_id = Some("prompt-1".into());
    harness
        .pending_provider_prompts
        .insert("prompt-1".into(), "provider".into());
    connect_test_tool(&mut harness, "interceptor");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::PROVIDER_RESPONSE_FINISHED,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register interceptor");

    harness
        .handle_extension_event_inner(
            "provider",
            Event::ProviderResponseFinishedReported(super::dispatch::provider_text_response(
                &"prompt-1".into(),
                crate::parse_agent_id("spoofed"),
                "done",
            )),
        )
        .expect("finished report");
    assert!(
        !harness.pending_provider_prompts.contains_key("prompt-1"),
        "events={:?}, parked={:?}, notices={:?}",
        committed_events(&harness)
            .iter()
            .map(|(_, event)| event.name())
            .collect::<Vec<_>>(),
        harness
            .pending_intercept
            .as_ref()
            .map(|pending| pending.event.name()),
        committed_events(&harness)
            .iter()
            .filter_map(|(_, event)| match event {
                Event::HarnessNotice(notice) => Some(notice.message.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
    );
    assert!(committed_events(&harness).iter().any(|(source, event)| {
        source.as_deref() == Some("provider")
            && matches!(event, Event::ProviderResponseFinishedReported(_))
    }));
    assert!(
        committed_events(&harness)
            .iter()
            .all(|(_, event)| !matches!(event, Event::ProviderResponseFinished(_)))
    );

    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("release canonical");
    assert!(committed_events(&harness).iter().any(|(source, event)| {
        source.as_deref() == Some(HARNESS_CONNECTION_ID)
            && matches!(event, Event::ProviderResponseFinished(_))
    }));
}

/// Tool-routing facts derived from a terminal report remain explicitly
/// harness-authored even when the requested tool is unavailable.
#[test]
fn finished_report_tool_rejection_successors_use_harness_source() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "provider",
        "provider",
        tau_proto::ClientKind::Provider,
    );
    let cid = ensure_test_user_agent(&mut harness);
    seed_agent_thinking(&mut harness, &cid, "prompt-1");
    harness.prompt_agents.insert("prompt-1".into(), cid.clone());
    harness
        .agents
        .get_mut(&cid)
        .expect("agent")
        .in_flight_prompt = Some("prompt-1".into());
    harness.agents.get_mut(&cid).expect("agent").last_prompt_id = Some("prompt-1".into());
    harness
        .pending_provider_prompts
        .insert("prompt-1".into(), "provider".into());
    let response = tau_proto::ProviderResponseFinished {
        agent_prompt_id: "prompt-1".into(),
        agent_id: crate::parse_agent_id("spoofed"),
        output_items: vec![tau_proto::ContextItem::ToolCall(tau_proto::ToolCallItem {
            call_id: "missing-call".into(),
            name: tau_proto::ToolName::new("missing_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: tau_proto::CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        originator: tau_proto::PromptOriginator::User,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    };
    let before_report = committed_events(&harness).len();

    harness
        .handle_extension_event_inner(
            "provider",
            Event::ProviderResponseFinishedReported(response),
        )
        .expect("finished report");

    let derived = committed_events(&harness)
        .into_iter()
        .skip(before_report)
        .collect::<Vec<_>>();
    assert!(derived.iter().any(|(_, event)| matches!(
        event,
        Event::ToolRequest(_)
            | Event::ToolRejected(_)
            | Event::ToolError(_)
            | Event::ProviderToolError(_)
    )));
    for (source, event) in derived {
        let expected = if matches!(event, Event::ProviderResponseFinishedReported(_)) {
            "provider"
        } else {
            HARNESS_CONNECTION_ID
        };
        assert_eq!(
            source.as_deref(),
            Some(expected),
            "terminal report or successor {:?} has wrong authority",
            event.name(),
        );
    }
}

/// Startup drains a downstream terminal publish error exactly once instead of
/// carrying it into an unrelated later runtime event.
#[test]
fn startup_connection_handling_surfaces_pending_publish_error_once() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    harness.pending_publish_error = Some(HarnessError::Participant(
        "terminal dispatch failed".to_owned(),
    ));

    let error = harness
        .handle_startup_from_connection("unknown", HarnessInputMessage::Ready(Default::default()))
        .expect_err("startup must surface pending publish error");
    assert!(error.to_string().contains("terminal dispatch failed"));
    assert!(
        !harness
            .handle_startup_from_connection(
                "unknown",
                HarnessInputMessage::Ready(Default::default()),
            )
            .expect("pending error was consumed")
    );
}

/// A required canonical journal failure cannot roll back terminal route cleanup
/// or the already committed report, and it prevents canonical broadcast/commit.
#[test]
fn finished_report_keeps_terminal_effects_when_canonical_store_fails() {
    let temp = TempDir::new().expect("temp dir");
    let state_dir = temp.path().join("state");
    let mut harness = quiet_provider_harness(&state_dir).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "provider",
        "provider",
        tau_proto::ClientKind::Provider,
    );
    let observer = connect_test_client(&mut harness, "observer", tau_proto::ClientKind::Ui);
    harness
        .handle_client_event(
            "observer",
            TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
                historical_selectors: Vec::new(),
                live_selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::PROVIDER_RESPONSE_FINISHED,
                )],
            })),
        )
        .expect("subscribe observer");
    let cid = ensure_test_user_agent(&mut harness);
    seed_agent_thinking(&mut harness, &cid, "prompt-1");
    harness.prompt_agents.insert("prompt-1".into(), cid.clone());
    {
        let agent = harness.agents.get_mut(&cid).expect("agent");
        agent.in_flight_prompt = Some("prompt-1".into());
        agent.last_prompt_id = Some("prompt-1".into());
    }
    harness
        .pending_provider_prompts
        .insert("prompt-1".into(), "provider".into());
    let agent_id = durable_agent_id_for_conversation(&harness, &cid);
    let failure_store = state_dir.join("failure-agent-store");
    let mut agent_store = tau_core::AgentStore::open(&failure_store).expect("failure agent store");
    agent_store
        .append_agent_event(
            agent_id.as_str(),
            None,
            Event::AgentStarted(tau_proto::AgentStarted {
                parent_agent: None,
                agent_id: agent_id.clone(),
                role: "engineer".to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: false,
            }),
        )
        .expect("seed failure agent");
    let event_path = failure_store.join(agent_id.as_str()).join("events.cbor");
    std::fs::remove_file(&event_path).expect("remove agent stream");
    std::fs::create_dir_all(&event_path).expect("block agent stream with directory");
    harness.agent_store = agent_store;

    harness
        .handle_extension_event_inner(
            "provider",
            Event::ProviderResponseFinishedReported(super::dispatch::provider_text_response(
                &"prompt-1".into(),
                crate::parse_agent_id("spoofed"),
                "done",
            )),
        )
        .expect("finished report");

    let events = committed_events(&harness);
    assert!(events.iter().any(|(source, event)| {
        source.as_deref() == Some("provider")
            && matches!(event, Event::ProviderResponseFinishedReported(_))
    }));
    assert!(
        observer
            .lock()
            .expect("observer")
            .iter()
            .filter_map(|frame| peel_inner_event(&frame.frame))
            .all(|event| !matches!(event, Event::ProviderResponseFinished(_)))
    );
    assert!(!harness.pending_provider_prompts.contains_key("prompt-1"));
    assert!(
        harness
            .agents
            .get(&cid)
            .expect("agent")
            .in_flight_prompt
            .is_none()
    );
}

/// Live subscribers receive terminal reports with provider-image bytes removed,
/// independently of terminal prompt correlation.
#[test]
fn finished_report_live_delivery_clears_provider_image_bytes() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "provider",
        "provider",
        tau_proto::ClientKind::Provider,
    );
    let observer = connect_test_client(&mut harness, "observer", tau_proto::ClientKind::Ui);
    harness
        .handle_client_event(
            "observer",
            TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
                historical_selectors: Vec::new(),
                live_selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::PROVIDER_RESPONSE_FINISHED_REPORTED,
                )],
            })),
        )
        .expect("subscribe observer");
    let response = tau_proto::ProviderResponseFinished {
        agent_prompt_id: "unknown-prompt".into(),
        agent_id: crate::parse_agent_id("spoofed"),
        output_items: vec![tau_proto::ContextItem::ToolResult(
            tau_proto::ToolResultItem {
                call_id: "image-call".into(),
                tool_type: tau_proto::ToolType::Function,
                status: tau_proto::ToolResultStatus::Success,
                output: tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Text(
                    "image".into(),
                )),
                provider_content: vec![tau_proto::ToolResultContentPart::Image(
                    tau_proto::ImageContent {
                        media_type: tau_proto::ImageMediaType::Png,
                        data: vec![1, 2, 3].into(),
                        width: 1,
                        height: 1,
                        detail: tau_proto::ImageDetail::High,
                    },
                )],
            },
        )],
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        originator: tau_proto::PromptOriginator::User,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    };
    harness
        .handle_extension_event_inner(
            "provider",
            Event::ProviderResponseFinishedReported(response),
        )
        .expect("finished report");

    let delivered = observer
        .lock()
        .expect("observer")
        .iter()
        .filter_map(|frame| peel_inner_event(&frame.frame))
        .find_map(|event| match event {
            Event::ProviderResponseFinishedReported(response) => Some(response.clone()),
            _ => None,
        })
        .expect("delivered terminal report");
    let tau_proto::ContextItem::ToolResult(result) = &delivered.output_items[0] else {
        panic!("tool result");
    };
    let tau_proto::ToolResultContentPart::Image(image) = &result.provider_content[0];
    assert!(image.data.is_empty());
}

/// Stale-response termination derived from a committed report carries explicit
/// harness provenance rather than inheriting or dropping the provider source.
#[test]
fn stale_finished_report_termination_uses_harness_source() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "provider",
        "provider",
        tau_proto::ClientKind::Provider,
    );
    let cid = ensure_test_user_agent(&mut harness);
    seed_agent_thinking(&mut harness, &cid, "prompt-1");
    harness.prompt_agents.insert("prompt-1".into(), cid.clone());
    {
        let agent = harness.agents.get_mut(&cid).expect("agent");
        agent.in_flight_prompt = Some("prompt-1".into());
        agent.last_prompt_id = Some("newer-prompt".into());
    }
    harness
        .pending_provider_prompts
        .insert("prompt-1".into(), "provider".into());

    harness
        .handle_extension_event_inner(
            "provider",
            Event::ProviderResponseFinishedReported(super::dispatch::provider_text_response(
                &"prompt-1".into(),
                crate::parse_agent_id("spoofed"),
                "stale",
            )),
        )
        .expect("stale finished report");

    assert!(committed_events(&harness).iter().any(|(source, event)| {
        source.as_deref() == Some(HARNESS_CONNECTION_ID)
            && matches!(
                event,
                Event::AgentPromptTerminated(terminated)
                    if terminated.agent_prompt_id.as_str() == "prompt-1"
                        && terminated.reason == tau_proto::AgentPromptTerminationReason::Stale
            )
    }));
}

/// Interception replacement changes the committed report payload before prompt
/// correlation; dropping the next report prevents both commit and successor.
#[test]
fn provider_execution_report_replacement_and_drop_control_semantics() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "provider",
        "provider",
        tau_proto::ClientKind::Provider,
    );
    harness
        .pending_provider_prompts
        .insert("prompt-2".into(), "provider".into());
    connect_test_tool(&mut harness, "interceptor");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::PROVIDER_PROMPT_SUBMITTED_REPORTED,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register interceptor");

    harness
        .handle_extension_event_inner("provider", submitted("prompt-1"))
        .expect("park report");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(Some(Box::new(submitted("prompt-2")))),
            })),
        )
        .expect("replace report");
    assert!(matches!(
        prompt_submission_events(&harness).as_slice(),
        [
            (_, reported),
            (_, canonical),
        ] if reported == &tau_proto::EventName::PROVIDER_PROMPT_SUBMITTED_REPORTED
            && canonical == &tau_proto::EventName::PROVIDER_PROMPT_SUBMITTED
    ));

    harness
        .handle_extension_event_inner("provider", submitted("prompt-2"))
        .expect("park second report");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Drop,
            })),
        )
        .expect("drop report");
    assert_eq!(prompt_submission_events(&harness).len(), 2);
}

/// A parked report retains the old configured logical instance and cannot
/// acquire authority from a replacement using the same connection/name.
#[test]
fn parked_stale_provider_execution_report_commits_without_successor() {
    let temp = TempDir::new().expect("temp dir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "provider",
        "provider",
        tau_proto::ClientKind::Provider,
    );
    harness
        .pending_provider_prompts
        .insert("prompt-1".into(), "provider".into());
    connect_test_tool(&mut harness, "interceptor");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::PROVIDER_PROMPT_SUBMITTED_REPORTED,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register interceptor");
    harness
        .handle_extension_event_inner("provider", submitted("prompt-1"))
        .expect("park report");

    harness.handle_disconnect("provider");
    connect_ready_configured_extension(
        &mut harness,
        "provider",
        "provider",
        tau_proto::ClientKind::Provider,
    );
    harness
        .extensions
        .entries
        .get_mut("provider")
        .expect("replacement")
        .instance_id = 43.into();
    harness
        .pending_provider_prompts
        .insert("prompt-1".into(), "provider".into());
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("release stale report");

    assert!(matches!(
        prompt_submission_events(&harness).as_slice(),
        [(Some(source), name)]
            if source.as_str() == "provider"
                && name == &tau_proto::EventName::PROVIDER_PROMPT_SUBMITTED_REPORTED
    ));
}
