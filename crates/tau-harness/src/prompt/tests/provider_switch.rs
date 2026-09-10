//! Provider conversion tests use canonical records, not JSON-origin guesses.

use super::*;

/// Validated live history and its serialized cold-replay source.
struct History {
    /// Current canonical fold.
    tree: tau_core::AgentTree,
    /// Exact accepted journal records.
    records: Vec<tau_core::PersistedAgentEvent>,
}

impl History {
    /// Creates an empty synthetic agent without contacting any provider.
    fn new() -> Self {
        Self {
            tree: tau_core::AgentTree::from_events(crate::parse_agent_id("main"), &[]),
            records: Vec::new(),
        }
    }

    /// Appends with the same validation used by cold replay.
    fn append(&mut self, event: Event) {
        let record = tau_core::PersistedAgentEvent {
            observation_id: tau_proto::ObservationId::from_bytes([0; 16]),
            seq: tau_core::PersistedAgentEventSeq::new(self.records.len() as u64),
            source: None,
            event,
            parent: tau_core::AgentEventParent::InheritHead,
            fold_semantics: tau_core::AgentJournalFoldSemantics::CommitOrder,
            recorded_at: tau_proto::UnixMicros::default(),
        };
        self.tree
            .apply_persisted_record(&record)
            .expect("valid record");
        self.records.push(record);
    }

    /// Records a prompt owner and its captured provider-qualified model.
    fn start(&mut self, prompt: &str, model: &str) {
        let through = self
            .tree
            .head()
            .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
        self.append(Event::AgentInferenceDispatchStarted(
            tau_proto::AgentInferenceDispatchStarted {
                agent_id: crate::parse_agent_id("main"),
                transaction_id: None,
                agent_prompt_id: prompt.parse().expect("prompt"),
                through,
                model: model.into(),
                operation: tau_proto::PromptOperation::Inference,
                activation_cut: through,
                output_length_continuation: None,
            },
        ));
        self.append(Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
            agent_prompt_id: prompt.parse().expect("prompt"),
            agent_id: crate::parse_agent_id("main"),
            session_id: "switch-test".parse().expect("session"),
            model: model.into(),
            model_params: Some(Default::default()),
            outer_turn_id: None,
            operation: tau_proto::PromptOperation::Inference,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }));
    }

    /// Finishes with selected items, retaining the ordinary canonical terminal.
    fn finish(&mut self, prompt: &str, items: Vec<ContextItem>) {
        let Event::ProviderResponseFinished(mut response) = finished_tool_call(prompt, "unused")
        else {
            unreachable!()
        };
        response.stop_reason = tau_proto::ProviderStopReason::EndTurn;
        response.output_items = items;
        self.append(Event::ProviderResponseFinished(response));
    }

    /// Reconstructs from encoded records to catch non-durable provenance.
    fn cold(&self) -> tau_core::AgentTree {
        let bytes = serde_json::to_vec(&self.records).expect("encode records");
        let records: Vec<tau_core::PersistedAgentEvent> =
            serde_json::from_slice(&bytes).expect("decode records");
        tau_core::AgentTree::from_events(crate::parse_agent_id("main"), &records)
    }
}

fn opaque(kind: &str, marker: &str) -> ContextItem {
    let item = tau_proto::OpaqueProviderItem::from_raw_json(format!(
        "{{ \"type\" : \"{kind}\", \"encrypted_content\" : \"{marker}\" }}"
    ))
    .expect("raw item");
    match kind {
        "reasoning" => ContextItem::Reasoning(item),
        "compaction" => ContextItem::Compaction(item),
        _ => ContextItem::UnknownProviderItem(item),
    }
}

fn project(tree: &tau_core::AgentTree, provider: &str) -> (tau_proto::PromptContext, bool) {
    let (assembled, omitted) = assemble_prompt_context_for_provider(
        tree,
        tree.head(),
        None,
        &tau_proto::ProviderName::new(provider),
    )
    .unwrap_or_else(|error| panic!("{error}"));
    (assembled.context, omitted)
}

/// A→B→A preserves portable history and each provider's exact raw reasoning,
/// without mutating canonical history or losing provenance on cold replay.
#[test]
fn provider_switch_round_trip_preserves_canonical_reasoning_and_portable_messages() {
    let mut history = History::new();
    history.append(user_prompt("portable question"));
    let a = opaque("reasoning", "A");
    history.start("ap-a", "a/first");
    history.finish("ap-a", vec![a.clone(), assistant_message("answer A")]);
    let before = assemble_prompt_context_from(&history.tree, history.tree.head()).context;
    assert_eq!(project(&history.tree, "a"), (before, false));
    let b = opaque("reasoning", "B");
    history.start("ap-b", "b/second");
    history.finish("ap-b", vec![b.clone(), assistant_message("answer B")]);
    let canonical = assemble_prompt_context_from(&history.tree, history.tree.head()).context;
    for tree in [&history.tree, &history.cold()] {
        for (provider, retained, omitted) in [("a", &a, &b), ("b", &b, &a)] {
            let (context, warned) = project(tree, provider);
            assert!(warned);
            let items = context.flatten();
            assert!(items.contains(retained));
            assert!(!items.contains(omitted));
            assert!(items.contains(&assistant_message("answer A")));
            assert!(items.contains(&assistant_message("answer B")));
            assert_eq!(
                items
                    .iter()
                    .filter(|item| matches!(item, ContextItem::Message(_)))
                    .count(),
                3
            );
        }
        assert_eq!(
            assemble_prompt_context_from(tree, tree.head()).context,
            canonical
        );
    }
}

/// Unknown origin never becomes compatible merely because its raw JSON looks
/// familiar; visible messages survive and the omission is reported.
#[test]
fn provider_switch_unknown_origin_drops_opaque_material_not_portable_content() {
    let mut history = History::new();
    history.finish(
        "ap-legacy",
        vec![
            opaque("reasoning", "a"),
            opaque("future_kind", "a"),
            assistant_message("portable"),
        ],
    );
    for tree in [&history.tree, &history.cold()] {
        let (context, omitted) = project(tree, "a");
        assert!(omitted);
        assert_eq!(context.flatten(), vec![assistant_message("portable")]);
    }
}

/// Clearing foreign tool/message replay envelopes must not alter semantic
/// correlation IDs, raw argument spelling, results, or narrative.
#[test]
fn provider_switch_clears_sidecars_without_changing_closed_tool_history() {
    let mut history = History::new();
    history.start("ap-tools", "a/model");
    let ContextItem::ToolCall(mut call) = web_tool_call("call-1") else {
        unreachable!()
    };
    call.raw_arguments_json = Some(" { \"q\" : \"test\" } ".to_owned());
    call.arguments = tau_proto::json_to_cbor(&serde_json::json!({"q":"test"}));
    call.responses_envelope = Some(tau_proto::ResponsesToolCallEnvelope {
        item_id: Some("foreign-id".to_owned()),
        ..Default::default()
    });
    let mut message = assistant_message("portable answer");
    let ContextItem::Message(ref mut text) = message else {
        unreachable!()
    };
    text.responses_raw_json = Some(r#"{"type":"message","id":"foreign"}"#.to_owned());
    let result = web_tool_result("call-1", "portable result");
    history.finish(
        "ap-tools",
        vec![message.clone(), ContextItem::ToolCall(call.clone())],
    );
    history.append(Event::ProviderToolResult(tau_proto::ToolResult {
        call_id: call.call_id.clone(),
        tool_name: call.name.clone(),
        tool_type: call.tool_type,
        result: CborValue::Text("portable result".to_owned()),
        presentation: Default::default(),
        provider_content: Vec::new(),
        kind: Default::default(),
        display: None,
        originator: Default::default(),
    }));
    let mut portable_call = call.clone();
    portable_call.responses_envelope = None;
    let ContextItem::Message(ref mut text) = message else {
        unreachable!()
    };
    text.responses_raw_json = None;
    for tree in [&history.tree, &history.cold()] {
        let (foreign, omitted) = project(tree, "b");
        assert!(omitted);
        assert_eq!(
            foreign.flatten(),
            vec![
                message.clone(),
                ContextItem::ToolCall(portable_call.clone()),
                result.clone()
            ]
        );
        let (same, omitted) = project(tree, "a");
        assert!(!omitted);
        assert!(
            same.flatten()
                .contains(&ContextItem::ToolCall(call.clone()))
        );
    }
}

/// Warnings are per destination tenure, including first dispatch and unknown
/// origin; retries, continuations, and model-only switches do not repeat them.
#[test]
fn provider_switch_warning_tracks_actual_omission_and_resets_on_return() {
    let a = tau_proto::ProviderName::new("a");
    let b = tau_proto::ProviderName::new("b");
    let mut warning = provider_switch_warning::ProviderSwitchWarning::default();
    assert!(!warning.observe(&a, false));
    assert!(warning.observe(&a, true));
    assert!(!warning.observe(&a, true));
    assert!(!warning.observe(&b, false));
    assert!(warning.observe(&b, true));
    assert!(warning.observe(&a, true));
    assert!(provider_switch_warning::ProviderSwitchWarning::default().observe(&a, true));
}

/// Backend metadata describes the actual historical source, not upstream
/// replay syntax. Portable-only responses must retain it without a loss
/// warning.
#[test]
fn provider_switch_preserves_backend_metadata_without_false_omission_warning() {
    let mut history = History::new();
    history.start("ap-metadata", "a/model");
    let Event::ProviderResponseFinished(mut response) = finished_tool_call("ap-metadata", "unused")
    else {
        unreachable!()
    };
    response.output_items = vec![assistant_message("portable")];
    response.stop_reason = tau_proto::ProviderStopReason::EndTurn;
    response.backend = Some(tau_proto::ProviderBackend {
        kind: tau_proto::ProviderBackendKind::PublicResponses,
        base_url: "https://synthetic.invalid".to_owned(),
        transport: tau_proto::ProviderBackendTransport::HttpSse,
        stale_chain_fallback: false,
    });
    history.append(Event::ProviderResponseFinished(response));
    for tree in [&history.tree, &history.cold()] {
        let expected = assemble_prompt_context_from(tree, tree.head()).context;
        assert_eq!(project(tree, "b"), (expected, false));
    }
}

/// A native replacement is retained exactly for its producing provider but
/// cannot be stripped on a foreign request, including after cold
/// reconstruction.
#[test]
fn provider_switch_refuses_opaque_replacement_and_preserves_compatible_raw_bytes() {
    let replacement = opaque("compaction", "native prefix");
    let mut history = History::new();
    history.append(compaction_start_event(tau_proto::AgentHead::Root));
    history.append(compacted_event(vec![replacement.clone()]));
    history.append(user_prompt("retained suffix"));
    for tree in [&history.tree, &history.cold()] {
        let (same, omitted) = project(tree, "provider");
        assert!(!omitted);
        assert_eq!(same.flatten()[0], replacement);
        for cut in [
            None,
            Some(tau_proto::AgentHead::Node(tau_proto::NodeId::new(0))),
        ] {
            let rejected = assemble_prompt_context_for_provider(
                tree,
                tree.head(),
                cut,
                &tau_proto::ProviderName::new("foreign"),
            );
            assert!(matches!(rejected, Err(message) if message.contains("opaque compaction")));
        }
    }
}

/// Portable local summaries remain usable across providers; a native inline
/// sidecar may be omitted only while the full ordinary history remains present.
#[test]
fn provider_switch_preserves_portable_summary_and_drops_nonreplacement_compaction() {
    let mut summary = History::new();
    summary.append(compaction_start_event(tau_proto::AgentHead::Root));
    summary.append(compacted_event(vec![materialized_message(
        "portable summary",
    )]));
    assert_eq!(
        project(&summary.tree, "foreign").0.flatten(),
        vec![materialized_message("portable summary")]
    );

    let mut inline = History::new();
    inline.append(user_prompt("complete retained prefix"));
    inline.start("ap-inline", "a/model");
    inline.finish(
        "ap-inline",
        vec![opaque("compaction", "sidecar"), assistant_message("answer")],
    );
    let (context, omitted) = project(&inline.tree, "b");
    assert!(omitted);
    assert_eq!(context.flatten().len(), 2);
    assert!(context.flatten().contains(&assistant_message("answer")));
}
