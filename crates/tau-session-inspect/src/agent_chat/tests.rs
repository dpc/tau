use tau_core::{AgentEventParent, PersistedAgentEvent, PersistedAgentEventSeq};
use tau_proto::{
    AgentHead, AgentHeadMoved, AgentPromptSubmitted, AgentStarted, ContentPart, ContextItem,
    ContextRole, Event, MessageItem, PromptMessageClass, PromptOriginator, PromptSubmissionSource,
    ProviderResponseFinished, ProviderStopReason, UnixMicros,
};

use super::project_agent_chat;

fn record(seq: u64, event: Event) -> PersistedAgentEvent {
    record_at(
        seq,
        AgentEventParent::InheritHead,
        tau_core::AgentJournalFoldSemantics::CommitOrder,
        event,
    )
}

fn record_at(
    seq: u64,
    parent: AgentEventParent,
    fold_semantics: tau_core::AgentJournalFoldSemantics,
    event: Event,
) -> PersistedAgentEvent {
    PersistedAgentEvent {
        observation_id: tau_proto::ObservationId::from_bytes([seq as u8; 16]),
        seq: PersistedAgentEventSeq::new(seq),
        recorded_at: UnixMicros::new(1_700_000_000_000_000 + seq),
        parent,
        fold_semantics,
        source: None,
        event,
    }
}

fn started(agent_id: &tau_proto::AgentId) -> Event {
    Event::AgentStarted(AgentStarted {
        agent_id: agent_id.clone(),
        creator: Some(tau_proto::AgentCreator::User),
        parent_agent: None,
        role: "engineer".to_owned(),
        display_name: None,
        metadata: Vec::new(),
        ephemeral: false,
    })
}

fn message(role: ContextRole, text: &str) -> ContextItem {
    ContextItem::Message(MessageItem {
        role,
        content: vec![ContentPart::Text {
            text: text.to_owned(),
        }],
        phase: None,
        responses_raw_json: None,
    })
}

fn prompt(agent_id: &tau_proto::AgentId, text: &str, source: PromptSubmissionSource) -> Event {
    prompt_with_class(agent_id, text, source, PromptMessageClass::User)
}

fn prompt_with_class(
    agent_id: &tau_proto::AgentId,
    text: &str,
    source: PromptSubmissionSource,
    message_class: PromptMessageClass,
) -> Event {
    Event::AgentPromptSubmitted(AgentPromptSubmitted {
        agent_id: agent_id.clone(),
        inference_activation: false,
        text: text.to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class,
        internal_kind: None,
        originator: PromptOriginator::User,
        submission_source: source,
        display_name: None,
        ctx_id: None,
    })
}

fn response(agent_id: &tau_proto::AgentId, prompt_id: &str, text: &str) -> Event {
    response_with_originator(agent_id, prompt_id, text, PromptOriginator::User)
}

fn response_with_originator(
    agent_id: &tau_proto::AgentId,
    prompt_id: &str,
    text: &str,
    originator: PromptOriginator,
) -> Event {
    Event::ProviderResponseFinished(ProviderResponseFinished {
        agent_prompt_id: prompt_id.parse().expect("prompt id"),
        agent_id: agent_id.clone(),
        output_items: vec![
            ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
                text: "private thought".to_owned(),
                kind: tau_proto::ReasoningTextKind::Summary,
            }),
            message(ContextRole::Assistant, text),
        ],
        stop_reason: ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: Default::default(),
        output_length_disposition: Default::default(),
        provider_attempt: Default::default(),
        automatic_compaction_decision: None,
        originator,
        usage: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
}

/// The export follows the selected durable branch and excludes abandoned
/// branches, internal prompts, reasoning, and tools.
#[test]
fn selected_branch_projects_only_human_conversation() {
    let agent_id = tau_proto::AgentId::parse("chat-agent").expect("agent id");
    let records = vec![
        record(
            0,
            Event::AgentStarted(AgentStarted {
                agent_id: agent_id.clone(),
                creator: Some(tau_proto::AgentCreator::User),
                parent_agent: None,
                role: "engineer".to_owned(),
                display_name: Some("Helper".to_owned()),
                metadata: Vec::new(),
                ephemeral: false,
            }),
        ),
        record(
            1,
            prompt(&agent_id, "first question", PromptSubmissionSource::HumanUi),
        ),
        record(2, response(&agent_id, "prompt-1", "first answer")),
        record(
            3,
            Event::AgentHeadMoved(AgentHeadMoved {
                agent_id: agent_id.clone(),
                head: AgentHead::Node(tau_core::NodeId::new(0)),
            }),
        ),
        record(
            4,
            prompt(
                &agent_id,
                "replacement question",
                PromptSubmissionSource::HumanUi,
            ),
        ),
        record(
            5,
            prompt_with_class(
                &agent_id,
                "hidden instruction",
                PromptSubmissionSource::HumanUi,
                PromptMessageClass::Internal,
            ),
        ),
        record(
            6,
            response_with_originator(
                &agent_id,
                "prompt-internal",
                "hidden response",
                PromptOriginator::Extension {
                    name: tau_proto::ExtensionName::parse("test-extension")
                        .expect("extension name"),
                    query_id: "query-1".to_owned(),
                },
            ),
        ),
        record(7, response(&agent_id, "prompt-2", "replacement answer")),
    ];

    let chat = project_agent_chat(&agent_id, &records).expect("chat projection");

    assert_eq!(chat.role, "engineer");
    assert_eq!(chat.display_name.as_deref(), Some("Helper"));
    assert_eq!(
        chat.messages
            .iter()
            .map(|message| message.text.as_str())
            .collect::<Vec<_>>(),
        [
            "first question",
            "replacement question",
            "replacement answer"
        ]
    );
    assert!(!chat.to_markdown().contains("private thought"));
    assert!(!chat.to_markdown().contains("hidden response"));
    assert_eq!(chat.response_relative(0), Some("replacement answer"));
}

/// Reverse response indexing is stable for `:edit-prompt N`: zero is newest,
/// one is the previous assistant response, and out-of-range values fail closed.
#[test]
fn response_relative_indexes_from_newest() {
    let agent_id = tau_proto::AgentId::parse("chat-agent").expect("agent id");
    let records = vec![
        record(
            0,
            Event::AgentStarted(AgentStarted {
                agent_id: agent_id.clone(),
                creator: Some(tau_proto::AgentCreator::User),
                parent_agent: None,
                role: "engineer".to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: false,
            }),
        ),
        record(1, response(&agent_id, "prompt-1", "older")),
        record(2, response(&agent_id, "prompt-2", "newest")),
    ];
    let chat = project_agent_chat(&agent_id, &records).expect("chat projection");

    assert_eq!(chat.response_relative(0), Some("newest"));
    assert_eq!(chat.response_relative(1), Some("older"));
    assert_eq!(chat.response_relative(2), None);
    let toon: serde_json::Value =
        serde_toon::from_str(&chat.to_toon().expect("TOON export")).expect("strict TOON");
    assert_eq!(toon["schema"], "tau.agent_chat");
    assert_eq!(toon["messages"][1]["text"], "newest");
}

/// Inference-deferred human input materializes after its owning response but
/// retains the prompt occurrence's provenance and branch order.
#[test]
fn inference_deferred_prompt_keeps_response_and_prompt_classification() {
    let agent_id = tau_proto::AgentId::parse("chat-agent").expect("agent id");
    let owner_prompt_id: tau_proto::AgentPromptId = "prompt-owner".parse().expect("prompt id");
    let mut initial = match prompt(
        &agent_id,
        "initial question",
        PromptSubmissionSource::HumanUi,
    ) {
        Event::AgentPromptSubmitted(prompt) => prompt,
        _ => unreachable!("prompt helper"),
    };
    initial.inference_activation = true;
    let records = vec![
        record(0, started(&agent_id)),
        record(1, Event::AgentPromptSubmitted(initial)),
        record_at(
            2,
            AgentEventParent::InheritHead,
            tau_core::AgentJournalFoldSemantics::InferenceDeferredInputV1,
            Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
                agent_id: agent_id.clone(),
                transaction_id: None,
                agent_prompt_id: owner_prompt_id.clone(),
                through: AgentHead::Node(tau_core::NodeId::new(0)),
                model: "provider/model".into(),
                operation: tau_proto::PromptOperation::Inference,
                activation_cut: AgentHead::Node(tau_core::NodeId::new(0)),
                output_length_continuation: None,
            }),
        ),
        record(
            3,
            prompt(
                &agent_id,
                "queued follow-up",
                PromptSubmissionSource::HumanUi,
            ),
        ),
        record_at(
            4,
            AgentEventParent::Under(tau_core::NodeId::new(0)),
            tau_core::AgentJournalFoldSemantics::CommitOrder,
            response(&agent_id, owner_prompt_id.as_str(), "first answer"),
        ),
    ];

    let chat = project_agent_chat(&agent_id, &records).expect("deferred chat projection");

    assert_eq!(
        chat.messages
            .iter()
            .map(|message| message.text.as_str())
            .collect::<Vec<_>>(),
        ["initial question", "first answer", "queued follow-up"]
    );
    assert_eq!(chat.response_relative(0), Some("first answer"));
}
