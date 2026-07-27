use tau_proto::{
    AgentStarted, CborValue, Event, ExtensionName, PromptOriginator, ProviderCacheMissDiagnostic,
    ProviderModelsDeclared, ProviderModelsUpdated, ProviderName, ProviderPromptSubmitted,
    ProviderQuotaClear, ProviderQuotaEpoch, ProviderQuotaPatch, ProviderQuotaReplace,
    ProviderResponseFinished, ProviderResponseUpdated, ProviderRetryPromptResult,
    ProviderStopReason, RetryPromptRequestId, RetryPromptStatus, SessionAgentLoaded,
    SessionAgentUnloaded, ToolCancelled, ToolError, ToolName, ToolProgress, ToolResult,
    ToolResultKind, ToolType,
};

use super::semantic_event_router::{session_membership_id_for_event, should_persist_event};
use crate::parse_agent_id;

/// Metadata requests never enter semantic history for either caller-selected
/// persistence value, while their canonical successors remain durable.
#[test]
fn metadata_requests_are_never_semantically_persisted() {
    let agent_id = tau_proto::AgentId::parse("metadata-agent").expect("agent id");
    let set = tau_proto::AgentMetadataSet {
        agent_id: agent_id.clone(),
        key: tau_proto::AgentMetadataKey::new("ext_test_value"),
        value: CborValue::Text("value".to_owned()),
        mutation_id: None,
        inheritable: true,
    };
    let unset = tau_proto::AgentMetadataUnset {
        agent_id,
        key: tau_proto::AgentMetadataKey::new("ext_test_value"),
    };
    for request in [
        Event::AgentMetadataSetRequest(set.clone()),
        Event::AgentMetadataUnsetRequest(unset.clone()),
    ] {
        assert!(!should_persist_event(&request, true));
        assert!(!should_persist_event(&request, false));
    }
    assert!(should_persist_event(&Event::AgentMetadataSet(set), true));
    assert!(should_persist_event(
        &Event::AgentMetadataUnset(unset),
        true
    ));
}

/// Provider declaration and canonical model snapshots are reconstructed current
/// state, never durable semantic history.
#[test]
fn provider_model_state_never_enters_semantic_history() {
    for event in [
        Event::ProviderModelsDeclared(ProviderModelsDeclared { models: Vec::new() }),
        Event::ProviderModelsUpdated(ProviderModelsUpdated {
            publisher_extension_id: ExtensionName::from("provider"),
            models: Vec::new(),
        }),
    ] {
        assert!(!should_persist_event(&event, true));
        assert!(!should_persist_event(&event, false));
    }
}

/// Extension prompt-fragment declarations are runtime prompt-assembly inputs,
/// never semantic history, even when a peer requests durable publication.
#[test]
fn prompt_fragment_declarations_never_enter_semantic_history() {
    let event = Event::ExtPromptFragmentPublish(tau_proto::ExtPromptFragmentPublish {
        fragment: tau_proto::PromptFragment::new(
            "extension.instructions",
            tau_proto::PromptPriority::new(10),
            "runtime instructions",
        ),
    });

    assert!(!event.defaults_to_persist());
    assert!(!should_persist_event(&event, true));
    assert!(!should_persist_event(&event, false));
}

/// Complete discovery declarations are runtime replacement inputs and never
/// enter semantic history, even when a peer requests durable publication.
#[test]
fn discovery_snapshot_declarations_never_enter_semantic_history() {
    let events = [
        Event::ExtensionSessionDiscoverySnapshotDeclared(
            tau_proto::ExtensionSessionDiscoverySnapshotDeclared {
                session_id: "test-session".into(),
                skills: Vec::new(),
                agents_files: Vec::new(),
            },
        ),
        Event::ExtensionAgentDiscoverySnapshotDeclared(
            tau_proto::ExtensionAgentDiscoverySnapshotDeclared {
                session_id: "test-session".into(),
                agent_id: parse_agent_id("agent-1"),
                agent_initialization_id: tau_proto::AgentInitializationId::new("init-1"),
                skills: Vec::new(),
                agents_files: Vec::new(),
            },
        ),
    ];

    for event in events {
        assert!(!event.defaults_to_persist());
        assert!(!should_persist_event(&event, true));
        assert!(!should_persist_event(&event, false));
    }
}

/// Per-agent context declarations and readiness remain runtime-only even when
/// a raw peer asks for durable publication.
#[test]
fn per_agent_context_events_never_enter_semantic_history() {
    for event in [
        Event::ExtensionContextProviderRegister(tau_proto::ExtensionContextProviderRegister {}),
        Event::ExtAgentContextPublish(tau_proto::ExtAgentContextPublish {
            session_id: tau_proto::SessionId::new("test-session"),
            agent_initialization_id: tau_proto::AgentInitializationId::new("test-init"),

            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            key: "workdir".into(),
            value: tau_proto::AgentContextValue(serde_json::json!("/tmp")),
        }),
        Event::ExtensionContextReady(tau_proto::ExtensionContextReady {
            agent_initialization_id: tau_proto::AgentInitializationId::new("test-init"),

            session_id: "session-1".into(),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
        }),
    ] {
        assert!(!event.defaults_to_persist());
        assert!(!should_persist_event(&event, true));
        assert!(!should_persist_event(&event, false));
    }
}

/// Raw internal-prompt requests are runtime-only inputs even when a peer asks
/// for durable publication; only harness-owned prompt facts enter history.
#[test]
fn internal_prompt_requests_never_enter_semantic_history() {
    let event = Event::ExtInternalPromptSubmitRequest(tau_proto::ExtInternalPromptSubmitRequest {
        agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
        text: "wake".to_owned(),
        ctx_id: Some("timer:1".to_owned()),
        activation_kind: None,
    });
    assert!(!event.defaults_to_persist());
    assert!(!should_persist_event(&event, true));
    assert!(!should_persist_event(&event, false));
}

/// Terminal-output events are live side effects and never enter semantic
/// history, regardless of caller-selected persistence metadata.
#[test]
fn terminal_output_events_never_enter_semantic_history() {
    for event in [
        Event::TermBell(tau_proto::TermBell {}),
        Event::Osc1337SetUserVar(tau_proto::Osc1337SetUserVar {
            name: "status".to_owned(),
            value: "ready".to_owned(),
        }),
    ] {
        assert!(!should_persist_event(&event, true));
        assert!(!should_persist_event(&event, false));
    }
}

/// Opaque custom extension events are runtime subscriber traffic and never
/// enter semantic history for either caller-selected persistence value.
#[test]
fn custom_extension_events_never_enter_semantic_history() {
    let event = Event::ExtensionEvent(
        tau_proto::CustomEvent::try_new(
            "demo.observation".parse().expect("event name"),
            Some("s1".into()),
            CborValue::Text("opaque".to_owned()),
        )
        .expect("custom event"),
    );
    assert!(!should_persist_event(&event, true));
    assert!(!should_persist_event(&event, false));
}

/// UI draft and focus observations are live liveness signals, never semantic
/// history, for either caller-selected persistence value.
#[test]
fn ui_liveness_events_never_enter_semantic_history() {
    for event in [
        Event::UiPromptDraft(tau_proto::UiPromptDraft {
            session_id: "s1".into(),
            target_agent_id: None,
            text: "typing".to_owned(),
        }),
        Event::UiFocusChanged(tau_proto::UiFocusChanged {
            session_id: "s1".into(),
            focused: true,
        }),
    ] {
        assert!(!event.defaults_to_persist());
        assert!(!should_persist_event(&event, true));
        assert!(!should_persist_event(&event, false));
    }
}

/// Provider quota observations and canonical current snapshots remain outside
/// semantic history even when a peer incorrectly requests durable publication.
#[test]
fn provider_quota_state_never_enters_semantic_history() {
    let provider = ProviderName::new("chatgpt");
    let epoch = ProviderQuotaEpoch::parse("epoch-1").expect("epoch");
    for event in [
        Event::ProviderQuotaReplaceReported(ProviderQuotaReplace {
            provider: provider.clone(),
            profile_epoch: epoch.clone(),
            sequence: 1,
            establishes_new_epoch: true,
            windows: Vec::new(),
            route_bindings: Vec::new(),
        }),
        Event::ProviderQuotaPatchReported(ProviderQuotaPatch {
            provider: provider.clone(),
            profile_epoch: epoch.clone(),
            sequence: 2,
            windows: Vec::new(),
            removed_window_keys: Vec::new(),
            route_bindings: Vec::new(),
        }),
        Event::ProviderQuotaClearReported(ProviderQuotaClear {
            provider: provider.clone(),
            profile_epoch: epoch.clone(),
            sequence: 3,
        }),
        Event::HarnessProviderQuotaChanged(tau_proto::HarnessProviderQuotaChanged {
            provider,
            profile_epoch: epoch,
            sequence: 3,
            windows: Vec::new(),
            route_bindings: Vec::new(),
        }),
    ] {
        assert!(!should_persist_event(&event, true));
        assert!(!should_persist_event(&event, false));
    }
}

/// Provider execution reports are committed observations, never replayable
/// semantic facts, regardless of the peer-selected persistence value.
#[test]
fn provider_execution_reports_never_enter_semantic_history() {
    let prompt_id = tau_proto::AgentPromptId::from("prompt-1");
    let agent_id = parse_agent_id("agent-1");
    for event in [
        Event::ProviderPromptSubmittedReported(ProviderPromptSubmitted {
            agent_prompt_id: prompt_id.clone(),
            originator: PromptOriginator::User,
        }),
        Event::ProviderResponseUpdatedReported(ProviderResponseUpdated {
            agent_prompt_id: prompt_id.clone(),
            agent_id: agent_id.clone(),
            deltas: Vec::new(),
            compaction: None,
            status: None,
            response_stats: None,
            originator: PromptOriginator::User,
        }),
        Event::ProviderResponseFinishedReported(ProviderResponseFinished {
            estimated_api_cost_rates: None,
            estimated_api_cost_increment: None,

            agent_prompt_id: prompt_id.clone(),
            agent_id,
            output_items: Vec::new(),
            stop_reason: ProviderStopReason::EndTurn,
            error: None,
            failure_kind: None,
            context_limit_telemetry: None,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            originator: PromptOriginator::User,
            usage: None,
            compaction_original_input_tokens: None,
            compaction_compacted_input_tokens: None,
            backend: None,
            provider_response_id: None,
            ws_pool_delta: None,
        }),
        Event::ProviderRetryPromptResultReported(ProviderRetryPromptResult {
            request_id: RetryPromptRequestId::parse("retry-1").expect("retry id"),
            agent_prompt_id: prompt_id.clone(),
            status: RetryPromptStatus::Accepted,
        }),
        Event::ProviderCacheMissDiagnosticReported(ProviderCacheMissDiagnostic {
            agent_prompt_id: prompt_id,
            model: "provider/model".into(),
            originator: PromptOriginator::User,
            tool_choice: tau_proto::ToolChoice::default(),
            ws_pool_delta: None,
            input_tokens: 1,
            cached_tokens: 0,
            previous_input_tokens: 1,
            cacheable_input_tokens: 1,
            corrected_cache_efficiency: 0.0,
        }),
    ] {
        assert!(!event.defaults_to_persist());
        assert!(!should_persist_event(&event, true));
        assert!(!should_persist_event(&event, false));
    }
}

/// Tool declarations and canonical registry state are runtime lifecycle only,
/// even if a peer sends an incorrect durable Emit override.
#[test]
fn tool_lifecycle_state_never_enters_semantic_history() {
    let declaration = tau_proto::ToolRegistrationDeclared {
        tool: tau_proto::ToolSpec {
            name: ToolName::new("runtime_tool"),
            model_visible_name: None,
            description: None,
            parameters: None,
            format: None,
            tool_type: ToolType::Function,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
        tool_group: None,
        prompt_fragment: None,
    };
    for event in [
        Event::ToolRegistrationDeclared(declaration.clone()),
        Event::ToolRegister(tau_proto::ToolRegister {
            publisher_extension_id: ExtensionName::from("tool"),
            publisher_instance_id: 1.into(),
            tool: declaration.tool,
            tool_group: None,
            prompt_fragment: None,
        }),
        Event::ToolUnregistrationDeclared(tau_proto::ToolUnregistrationDeclared {
            tool_name: ToolName::new("runtime_tool"),
        }),
        Event::ToolUnregister(tau_proto::ToolUnregister {
            publisher_extension_id: ExtensionName::from("tool"),
            publisher_instance_id: 1.into(),
            tool_name: ToolName::new("runtime_tool"),
        }),
    ] {
        assert!(!should_persist_event(&event, true));
        assert!(!should_persist_event(&event, false));
    }
}

/// Peer progress observations and canonical harness progress stay live-only
/// even when a sender incorrectly requests durable publication.
#[test]
fn tool_progress_never_enters_semantic_history() {
    let progress = ToolProgress {
        call_id: "progress-call".into(),
        tool_name: ToolName::new("runtime_tool"),
        message: Some("running".to_owned()),
        progress: None,
        display: None,
    };
    for event in [
        Event::ToolProgressReported(progress.clone()),
        Event::ToolProgress(progress),
    ] {
        assert!(!should_persist_event(&event, true));
        assert!(!should_persist_event(&event, false));
    }
}

/// Peer shell reports never enter semantic history, while the existing
/// canonical progress/completion classifications stay unchanged.
#[test]
fn shell_reports_never_enter_semantic_history() {
    let progress = tau_proto::ShellCommandProgress {
        command_id: "shell-route".into(),
        stream: tau_proto::ShellStream::Stdout,
        chunk: "chunk".to_owned(),
        target_agent_id: Some(parse_agent_id("agent-1")),
    };
    let finished = tau_proto::ShellCommandFinished {
        command_id: "shell-route".into(),
        session_id: "session-1".into(),
        command: "pwd".to_owned(),
        include_in_context: true,
        target_agent_id: Some(parse_agent_id("agent-1")),
        output: "/tmp\n".to_owned(),
        exit_code: Some(0),
        cancelled: false,
    };
    for report in [
        Event::ShellCommandProgressReported(progress.clone()),
        Event::ShellCommandFinishedReported(finished.clone()),
    ] {
        assert!(!should_persist_event(&report, true));
        assert!(!should_persist_event(&report, false));
    }
    assert!(!should_persist_event(
        &Event::ShellCommandProgress(progress),
        false
    ));
    assert!(should_persist_event(
        &Event::ShellCommandFinished(finished),
        true
    ));
}

/// Ensures ordinary facts with `persist=false` remain live-only so
/// progress/status updates cannot accidentally enter durable session or agent
/// replay logs.
#[test]
fn persist_false_non_tool_event_is_not_persisted() {
    let event = Event::AgentStarted(AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),

        parent_agent: None,
        agent_id: parse_agent_id("agent-1"),
        role: "default".into(),
        display_name: None,
        metadata: Vec::new(),
        ephemeral: false,
    });

    assert!(!should_persist_event(&event, false));
    assert!(should_persist_event(&event, true));
}

/// Ensures raw extension-owned tool completions stay live-only semantic events
/// even when published with `persist=true`; their
/// provider-owned counterparts are the durable transcript facts.
#[test]
fn raw_tool_terminal_events_are_not_persisted() {
    let result = ToolResult {
        call_id: "call-1".into(),
        tool_name: ToolName::new("tool"),
        tool_type: ToolType::Function,
        result: CborValue::Null,
        provider_content: Vec::new(),
        kind: ToolResultKind::Final,
        display: None,
        originator: PromptOriginator::User,
    };
    let error = ToolError {
        call_id: "call-1".into(),
        tool_name: ToolName::new("tool"),
        tool_type: ToolType::Function,
        message: "failed".to_owned(),
        details: None,
        display: None,
        originator: PromptOriginator::User,
    };
    let cancelled = ToolCancelled {
        call_id: "call-1".into(),
        tool_name: ToolName::new("tool"),
        tool_type: ToolType::Function,
    };

    for event in [
        Event::ToolResultReported(result.clone()),
        Event::ToolResult(result),
        Event::ToolErrorReported(error.clone()),
        Event::ToolError(error),
        Event::ToolCancelledReported(cancelled),
    ] {
        assert!(!should_persist_event(&event, true));
        assert!(!should_persist_event(&event, false));
    }
}

/// Start-agent requests are committed live observations, not replayable
/// semantic facts, for either caller-supplied persistence value.
#[test]
fn raw_start_agent_requests_are_not_persisted() {
    let event = Event::StartAgentRequest(tau_proto::StartAgentRequest {
        query_id: "query-1".to_owned(),
        instruction: "delegate this".to_owned(),
        role: None,
        input_stats: tau_proto::ToolUseStats::default(),
        tool_call_id: None,
        task_name: None,
        parent_agent: None,
    });

    assert!(!should_persist_event(&event, true));
    assert!(!should_persist_event(&event, false));
}

/// Every established exception remains durable with `persist=false`, while an
/// ordinary eligible event follows the positive metadata value.
#[test]
fn persist_false_preserves_every_persistence_exception() {
    let result = ToolResult {
        call_id: "call-1".into(),
        tool_name: ToolName::new("tool"),
        tool_type: ToolType::Function,
        result: CborValue::Text("done".to_owned()),
        provider_content: Vec::new(),
        kind: ToolResultKind::Final,
        display: None,
        originator: PromptOriginator::User,
    };
    let error = ToolError {
        call_id: "call-1".into(),
        tool_name: ToolName::new("tool"),
        tool_type: ToolType::Function,
        message: "failed".to_owned(),
        details: None,
        display: None,
        originator: PromptOriginator::User,
    };
    let exceptions = [
        Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
            model_params: Some(tau_proto::ModelParams::default()),
            outer_turn_id: None,

            agent_prompt_id: "prompt-1".into(),
            agent_id: parse_agent_id("agent-1"),
            session_id: "session-1".into(),
            model: "test/model".parse().expect("model id"),
            operation: tau_proto::PromptOperation::Inference,
            originator: PromptOriginator::User,
            ctx_id: None,
        }),
        Event::ProviderToolResult(result),
        Event::ProviderToolError(error),
        Event::ToolCancelled(ToolCancelled {
            call_id: "call-1".into(),
            tool_name: ToolName::new("tool"),
            tool_type: ToolType::Function,
        }),
        Event::ToolBackgroundResult(tau_proto::ToolBackgroundResult {
            call_id: "call-1".into(),
            tool_name: ToolName::new("tool"),
            tool_type: ToolType::Function,
            result: CborValue::Text("done".to_owned()),
            display: None,
            originator: PromptOriginator::User,
        }),
        Event::ToolBackgroundError(tau_proto::ToolBackgroundError {
            call_id: "call-1".into(),
            tool_name: ToolName::new("tool"),
            tool_type: ToolType::Function,
            message: "failed".to_owned(),
            details: None,
            display: None,
            originator: PromptOriginator::User,
        }),
    ];

    for event in exceptions {
        assert!(
            should_persist_event(&event, false),
            "{} must retain its persistence exception",
            event.name()
        );
    }

    let ordinary = Event::AgentStarted(AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),

        parent_agent: None,
        agent_id: parse_agent_id("agent-1"),
        role: "default".into(),
        display_name: None,
        metadata: Vec::new(),
        ephemeral: false,
    });
    assert!(!should_persist_event(&ordinary, false));
    assert!(should_persist_event(&ordinary, true));
}

/// A decoded `persist=false` envelope still persists the established canonical
/// terminal-tool exception, while an ordinary fact follows the positive bit.
#[test]
fn inverted_wire_metadata_preserves_persistence_exceptions() {
    let exception = Event::ProviderToolError(ToolError {
        call_id: "call-1".into(),
        tool_name: ToolName::new("tool"),
        tool_type: ToolType::Function,
        message: "failed".to_owned(),
        details: None,
        display: None,
        originator: PromptOriginator::User,
    });
    let wire = serde_json::to_value(tau_proto::Emit::with_persist(exception, false))
        .expect("encode live-only metadata");
    assert_eq!(wire["persist"], false);
    assert!(wire.get("transient").is_none());
    let decoded: tau_proto::Emit =
        serde_json::from_value(wire).expect("decode positive persistence schema");
    let (exception, persist) = decoded.into_parts();
    assert!(!persist);
    assert!(should_persist_event(&exception, persist));

    let ordinary = Event::AgentStarted(AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),

        parent_agent: None,
        agent_id: parse_agent_id("agent-1"),
        role: "default".into(),
        display_name: None,
        metadata: Vec::new(),
        ephemeral: false,
    });
    assert!(!should_persist_event(&ordinary, false));
    assert!(should_persist_event(&ordinary, true));
}

/// Ensures session membership facts continue routing by their embedded session
/// id instead of falling through to agent transcript persistence.
#[test]
fn session_membership_events_route_to_session_log() {
    let loaded = Event::SessionAgentLoaded(SessionAgentLoaded {
        agent_initialization_id: tau_proto::AgentInitializationId::new("test-init"),

        session_id: "session-1".into(),
        agent_id: parse_agent_id("agent-1"),
        ephemeral: false,
    });
    let unloaded = Event::SessionAgentUnloaded(SessionAgentUnloaded {
        session_id: "session-2".into(),
        agent_id: parse_agent_id("agent-1"),
    });

    assert_eq!(
        session_membership_id_for_event(&loaded).as_deref(),
        Some("session-1")
    );
    assert_eq!(
        session_membership_id_for_event(&unloaded).as_deref(),
        Some("session-2")
    );
}
