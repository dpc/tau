use std::os::unix::net as path_std_os_unix_net;
use std::sync::{atomic as path_std_sync_atomic, mpsc as path_std_sync_mpsc};
use std::{
    collections as path_std_collections, fs as path_std_fs, path as path_std_path,
    sync as path_std_sync, time as path_std_time,
};

use tau_config::settings as path_tau_config_settings;
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;

use super::*;
use crate::agent::{ActivationDispatchState, Agent, AgentTurnState, PendingPrompt};
use crate::harness::interception::{AgentPublishCompletion, DeferredActivationObligation};
use crate::harness::prompt_materialization::{
    PromptSurfaceError, dispatch_provider_sort_count, reset_dispatch_provider_sort_count,
};
use crate::harness::{
    BackgroundCompletionPromptMode, GatedFinalDisposition, HarnessCommand, PendingRenderedPreview,
    PendingRenderedPrompt, PendingTool, RestoredCheckpointAuthority, STATUS_REMINDER,
    agent_message_activation_class, background_completion_prompt,
    extension_disconnected_background_tool_call_error_message,
    extension_disconnected_tool_call_error_message, final_status_reminder,
    is_restore_notice_prompt_text, restore_notice_prompt_for_elapsed,
    self_compaction_terminal_pending_prompt, self_compaction_terminal_prompt,
    unavailable_tool_error_message,
};
use crate::prompt::{
    prompt_template_parse_count, prompt_template_render_count, reset_prompt_template_test_counters,
};
use crate::{
    AgentId, agent as path_crate_agent, discovery as path_crate_discovery,
    event as path_crate_event, event_log as path_crate_event_log,
    extension as path_crate_extension, harness as path_crate_harness,
    internal_tools as path_crate_internal_tools,
};

/// Shared structured events captured by one thread-local tracing subscriber.
#[derive(Clone, Default)]
struct TraceCapture {
    /// Events accepted by the test tracing layer.
    events: path_std_sync::Arc<path_std_sync::Mutex<Vec<CapturedTrace>>>,
}

/// One content-free prompt-acceptance trace with its fixed target and fields.
#[derive(Debug)]
struct CapturedTrace {
    /// Tracing target selected by the callsite.
    target: String,
    /// Exact structured field names and rendered values.
    fields: path_std_collections::BTreeMap<String, String>,
}

/// Subscriber layer that retains only structured prompt-acceptance trace
/// events.
struct TraceCaptureLayer {
    /// Destination for this layer's captured events.
    capture: TraceCapture,
}

impl<S> Layer<S> for TraceCaptureLayer
where
    S: tracing::Subscriber,
{
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        if event.metadata().target() != "tau_harness::prompt_acceptance" {
            return;
        }
        let mut visitor = TraceFieldVisitor::default();
        event.record(&mut visitor);
        self.capture
            .events
            .lock()
            .expect("trace capture lock")
            .push(CapturedTrace {
                target: event.metadata().target().to_owned(),
                fields: visitor.fields,
            });
    }
}

/// Field visitor that retains only a trace event's rendered, content-free
/// values.
#[derive(Default)]
struct TraceFieldVisitor {
    /// Field names and values recorded by the tracing callsite.
    fields: path_std_collections::BTreeMap<String, String>,
}

impl TraceFieldVisitor {
    /// Record one field under its static tracing name.
    fn record(&mut self, field: &Field, value: String) {
        self.fields.insert(field.name().to_owned(), value);
    }
}

impl Visit for TraceFieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record(field, value.to_owned());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.record(field, format!("{value:?}"));
    }
}

/// Submit one prompt through the authenticated UI acceptance boundary.
fn submit_authenticated_ui_prompt(
    harness: &mut Harness,
    agent_id: tau_proto::AgentId,
    text: &str,
    message_class: tau_proto::PromptMessageClass,
) -> Result<bool, HarnessError> {
    harness.handle_authenticated_ui_prompt_submitted(
        crate::harness::harness_connection_id(),
        tau_proto::UiPromptSubmitted {
            literal: true,
            session_id: harness.session_runtime.current_session_id.clone(),
            text: text.to_owned(),
            agent_id,
            message_class,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        },
    )
}

/// Replace the current session manifest with a deterministic stale retention
/// hint while retaining a recognizable canonical creation time.
fn stale_session_manifest(h: &Harness) -> path_std_path::PathBuf {
    let path = h
        .session_runtime
        .store
        .sessions_dir()
        .join(h.session_runtime.current_session_id.as_str())
        .join("meta.json");
    path_std_fs::write(
        &path,
        serde_json::to_vec(&tau_core::SessionMeta {
            created_at: 7,
            last_touched: 8,
        })
        .expect("encode stale manifest"),
    )
    .expect("write stale manifest");
    path
}

/// Assert that operational use advanced only the derived retention hint.
fn assert_session_manifest_refreshed(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let meta: tau_core::SessionMeta = serde_json::from_slice(
            &path_std_fs::read(path).expect("read refreshed session manifest"),
        )
        .expect("decode refreshed session manifest");
        if 8 < meta.last_touched {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "coalesced session activity debt was not published"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn test_session_id(value: impl Into<String>) -> tau_proto::SessionId {
    tau_proto::SessionId::parse(value).expect("test session id")
}

fn test_agent_prompt_id(value: impl Into<String>) -> tau_proto::AgentPromptId {
    tau_proto::AgentPromptId::parse(value).expect("test agent prompt id")
}

/// Publish the canonical declaration required before a background terminal can
/// commit and trigger its dependent runtime effects.
fn publish_test_tool_declaration(harness: &mut Harness, cid: &AgentId, call_id: &str) {
    let agent_id = harness.agent_runtime.agent_registry.agents[cid]
        .identity
        .agent_id
        .clone()
        .expect("durable agent id");
    harness.publish_for_agent(
        cid,
        Event::ProviderResponseFinished(ProviderResponseFinished {
            automatic_compaction_decision: None,
            output_length_disposition: tau_proto::OutputLengthDisposition::None,
            estimated_api_cost_rates: None,
            estimated_api_cost_increment: None,
            agent_prompt_id: test_agent_prompt_id(format!("prompt-{call_id}")),
            agent_id: crate::parse_agent_id(&agent_id),
            output_items: vec![ContextItem::ToolCall(ToolCallItem {
                call_id: call_id.into(),
                name: ToolName::new("read"),
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
            compaction_output_tokens: None,
            backend: None,
            provider_attempt: Default::default(),
            provider_response_id: None,
            ws_pool_delta: None,
        }),
    );
}

fn responses_backend() -> tau_proto::ProviderBackend {
    tau_proto::ProviderBackend {
        kind: tau_proto::ProviderBackendKind::Responses,
        base_url: "https://api.example.test".to_owned(),
        transport: tau_proto::ProviderBackendTransport::HttpSse,
        stale_chain_fallback: false,
    }
}

fn publish_pending_agent_discovery(h: &mut Harness, agent_id: &str) {
    let agent_id = tau_proto::AgentId::parse(agent_id).expect("agent id");
    let Some((source_id, initialization_id)) = h
        .prompt_coordination
        .context_discovery
        .pending_agents
        .get(&agent_id)
        .and_then(|pending| {
            pending
                .waiting_on
                .iter()
                .next()
                .cloned()
                .map(|source_id| (source_id, pending.initialization_id.clone()))
        })
    else {
        return;
    };
    h.handle_extension_event(
        source_id.as_str(),
        TestProtocolItem::Event(Event::ExtensionContextReady(
            tau_proto::ExtensionContextReady {
                agent_initialization_id: initialization_id,

                session_id: h.session_runtime.current_session_id.clone(),
                agent_id,
            },
        )),
    )
    .expect("context ready");
}

fn shell_workdir_prompt_fixture() -> (TempDir, Harness, tau_proto::AgentId) {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = crate::parse_agent_id(
        h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .agent_id
            .as_deref()
            .expect("durable agent id"),
    );
    h.prompt_coordination
        .context_discovery
        .agent_context
        .clear();
    (td, h, agent_id)
}

fn publish_shell_workdir_context(
    h: &mut Harness,
    agent_id: &tau_proto::AgentId,
    workdir_tool_name: &str,
    extension_name: &str,
    label: &str,
    path: &str,
    status: &str,
) {
    let contributor = h
        .tool_routing
        .registry
        .all_tool_providers()
        .into_iter()
        .find(|provider| provider.tool.name.as_str() == workdir_tool_name)
        .map(|provider| provider.connection_id.clone())
        .expect("workdir tool provider");
    h.prompt_coordination
        .context_discovery
        .agent_context
        .publish(
            agent_id.clone(),
            tau_proto::AgentContextKey::new("workdir"),
            contributor,
            extension_name.to_owned(),
            tau_proto::AgentContextValue(serde_json::json!({
                "label": label,
                "path": path,
                "status": status,
            })),
        );
}

fn render_shell_workdir_prompt(h: &Harness, agent_id: &tau_proto::AgentId) -> String {
    let role_name = h.config.selected_role.as_str();
    let model = crate::model::model_for_role(
        &h.provider_runtime.model_info,
        &h.config.available_roles,
        role_name,
    );
    let specs = h.gather_effective_tool_specs_for_role_model(role_name, model.as_ref());
    h.try_build_system_prompt_for_role_and_agent(
        role_name,
        Some(agent_id),
        Some(agent_id),
        &specs,
        model.as_ref(),
        false,
    )
    .expect("render shell workdir prompt")
}

fn configure_test_ui_shell_provider(
    h: &mut Harness,
    connection_id: &str,
) -> Arc<Mutex<Vec<RoutedFrame>>> {
    for provider in super::super::ui_shell_provider_ids(&h.tool_routing.registry) {
        h.tool_routing.registry.unregister_connection(&provider);
    }
    let sink = connect_test_tool(h, connection_id);
    h.runtime_io
        .bus
        .set_subscriptions(
            &crate::test_connection_id(connection_id),
            Vec::new(),
            vec![tau_proto::EventSelector::Exact(
                tau_proto::EventName::UI_SHELL_COMMAND,
            )],
        )
        .expect("subscribe shell provider");
    h.tool_routing.registry.register(
        &crate::test_connection_id(connection_id),
        tau_proto::ToolSpec {
            name: tau_proto::ToolName::new("shell"),
            model_visible_name: None,
            description: None,
            tool_type: tau_proto::ToolType::Function,
            parameters: None,
            format: None,
            tags: vec![tau_proto::ToolTag::new("shell:exec:generic")],
            enabled_by_default: true,
            background_support: None,
            examples: Vec::new(),
        },
    );
    sink
}

fn routed_ui_shell_command(
    h: &mut Harness,
    command_id: &str,
    include_in_context: bool,
) -> tau_proto::UiShellCommand {
    let cid = ensure_test_user_agent(h);
    let agent_id = crate::parse_agent_id(
        h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .agent_id
            .as_deref()
            .expect("durable agent id"),
    );
    let command = tau_proto::UiShellCommand {
        session_id: h.session_runtime.current_session_id.clone(),
        command_id: test_shell_command_id(command_id),
        command: "pwd".to_owned(),
        include_in_context,
        target_agent_id: Some(agent_id),
    };
    h.handle_ui_shell_command(&crate::test_connection_id("ui"), command.clone());
    command
}

fn text_part(item: &ContextItem) -> Option<&str> {
    match item {
        ContextItem::Message(message) => message.content.first().map(|part| match part {
            ContentPart::Text { text }
            | ContentPart::SyntheticCompactionSummary { text }
            | ContentPart::HarnessInternalText { text } => text.as_str(),
            ContentPart::UrlCitation { .. } | ContentPart::CitationMetadataInvalid => "",
        }),
        ContextItem::ToolResult(result) => match &result.output.raw {
            CborValue::Text(text) => Some(text.as_str()),
            _ => None,
        },
        _ => None,
    }
}

fn tool_call_id(item: &ContextItem) -> Option<&str> {
    match item {
        ContextItem::ToolCall(call) => Some(call.call_id.as_str()),
        _ => None,
    }
}

fn tool_result_id(item: &ContextItem) -> Option<&str> {
    match item {
        ContextItem::ToolResult(result) => Some(result.call_id.as_str()),
        _ => None,
    }
}

fn cbor_map_text<'a>(value: &'a CborValue, key: &str) -> Option<&'a str> {
    let CborValue::Map(entries) = value else {
        return None;
    };
    entries.iter().find_map(|(entry_key, entry_value)| {
        matches!(entry_key, CborValue::Text(text) if text == key)
            .then_some(entry_value)
            .and_then(|value| match value {
                CborValue::Text(text) => Some(text.as_str()),
                _ => None,
            })
    })
}

pub(super) fn provider_text_response(
    spid: &AgentPromptId,
    agent_id: tau_proto::AgentId,
    text: &str,
) -> ProviderResponseFinished {
    ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid.clone(),
        agent_id,
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: text.to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    }
}

fn provider_repetition_response(
    spid: &AgentPromptId,
    agent_id: tau_proto::AgentId,
) -> ProviderResponseFinished {
    ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid.clone(),
        agent_id,
        output_items: Vec::new(),
        stop_reason: tau_proto::ProviderStopReason::RepetitionDetected,
        error: Some("provider stream repetition detected".to_owned()),
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    }
}

fn seed_prior_user_message(state_dir: &Path, text: &str) {
    seed_prior_user_message_at(state_dir, text, tau_proto::UnixMicros::now());
}

fn seed_prior_user_message_at(state_dir: &Path, text: &str, recorded_at: tau_proto::UnixMicros) {
    seed_main_agent_loaded(state_dir);
    let mut agent_store =
        tau_core::AgentStore::open(state_dir.join("agents")).expect("agent store");
    agent_store
        .append_agent_event_at(
            "main",
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                inference_activation: false,
                agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
                text: text.to_owned(),
                trusted_internal_spans: Vec::new(),
                message_class: tau_proto::PromptMessageClass::User,
                internal_kind: None,
                originator: tau_proto::PromptOriginator::User,
                submission_source: Default::default(),
                display_name: None,
                ctx_id: None,
            }),
            recorded_at,
        )
        .expect("seed prior user message");
}

fn seed_inference_activation_event(state_dir: &Path, event: Event) {
    seed_main_agent_loaded(state_dir);
    let mut agent_store =
        tau_core::AgentStore::open(state_dir.join("agents")).expect("agent store");
    agent_store
        .append_agent_event_at(
            "main",
            None,
            tau_core::AgentEventParent::InheritHead,
            event,
            tau_proto::UnixMicros::now(),
        )
        .expect("seed pending activation");
}

fn append_seed_agent_event(store: &mut tau_core::AgentStore, event: Event) {
    store
        .append_agent_event_at(
            "main",
            None,
            tau_core::AgentEventParent::InheritHead,
            event,
            tau_proto::UnixMicros::now(),
        )
        .expect("append seed event");
}

fn seed_agent_context_usage(state_dir: &Path, model: Option<&str>, input_tokens: u64) {
    seed_main_agent_loaded(state_dir);
    let mut store = tau_core::AgentStore::open(state_dir.join("agents")).expect("agent store");
    append_seed_agent_event(
        &mut store,
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            inference_activation: false,
            agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
            text: "usage prompt".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
    );
    let prompt_id = test_agent_prompt_id("ap-main-usage");
    let through = store
        .agent("main")
        .and_then(tau_core::AgentTree::head)
        .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
    if let Some(model) = model {
        append_seed_agent_event(
            &mut store,
            Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
                agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
                transaction_id: None,
                agent_prompt_id: prompt_id.clone(),
                through,
                model: Some(model.into()),
                operation: Some(tau_proto::PromptOperation::Inference),
                activation_cut: Some(through),
                output_length_continuation: None,
            }),
        );
    }
    let mut response = provider_text_response(
        &prompt_id,
        tau_proto::AgentId::parse("main").expect("agent id"),
        "usage response",
    );
    response.usage = Some(tau_proto::ProviderTokenUsage {
        model: model.map(Into::into),
        prompt_sent_tokens: input_tokens,
        prompt_cached_tokens: input_tokens / 2,
        prompt_cache_read_ceiling_tokens: None,
        cache: None,
        response_received_tokens: 10,
        stats: Default::default(),
    });
    append_seed_agent_event(&mut store, Event::ProviderResponseFinished(response));
}

fn seed_main_agent_loaded(state_dir: &Path) {
    seed_agent_loaded(state_dir, "s1", "main");
}

fn seed_agent_loaded(state_dir: &Path, session_id: &str, agent_id: &str) {
    let sessions_dir = tau_config::settings::sessions_dir_of(state_dir);
    let mut store = tau_core::SessionStore::open(&sessions_dir).expect("session store");
    store
        .record_session_meta(session_id)
        .expect("seed canonical session manifest");
    store
        .append_session_event(
            session_id,
            None,
            Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                    .expect("test identifier must be valid"),

                session_id: test_session_id(session_id),
                agent_id: tau_proto::AgentId::parse(agent_id).expect("agent id"),
                ephemeral: false,
            }),
        )
        .expect("seed session membership");
    let mut agent_store =
        tau_core::AgentStore::open(state_dir.join("agents")).expect("agent store");
    if !agent_store.agent_is_known_for_routing(agent_id) {
        agent_store
            .append_agent_event_at(
                agent_id,
                None,
                tau_core::AgentEventParent::InheritHead,
                Event::AgentStarted(tau_proto::AgentStarted {
                    creator: Some(tau_proto::AgentCreator::default()),

                    parent_agent: None,
                    agent_id: tau_proto::AgentId::parse(agent_id).expect("agent id"),
                    role: "engineer".to_owned(),
                    display_name: None,
                    metadata: Vec::new(),
                    ephemeral: false,
                }),
                tau_proto::UnixMicros::new(1),
            )
            .expect("seed agent creation");
    }
}

fn context_text_count(prompt: &AgentPromptCreated, text: &str) -> usize {
    prompt
        .context
        .flatten()
        .iter()
        .filter(|item| text_part(item) == Some(text))
        .count()
}

fn restore_notice_context_text(prompt: &AgentPromptCreated) -> Option<String> {
    prompt
        .context
        .flatten()
        .iter()
        .filter_map(text_part)
        .find(|text| {
            crate::internal_envelope::body(text).is_some_and(is_restore_notice_prompt_text)
        })
        .map(str::to_owned)
}

fn restore_notice_context_count(prompt: &AgentPromptCreated) -> usize {
    prompt
        .context
        .flatten()
        .iter()
        .filter_map(text_part)
        .filter(|text| {
            crate::internal_envelope::body(text).is_some_and(is_restore_notice_prompt_text)
        })
        .count()
}

fn restore_notice_event_count(h: &Harness) -> usize {
    loaded_agent_events(h, "s1")
        .iter()
        .filter(|event| {
            matches!(
                event,
                Event::AgentPromptSubmitted(prompt)
                    if prompt.message_class.is_internal()
                        && is_restore_notice_prompt_text(&prompt.text)
            )
        })
        .count()
}

fn restored_background_notice(call_id: &str) -> String {
    format!(
        "{}: true\n\nBackground tool call `{call_id}` was interrupted due to session restart. Side effects may have occurred.",
        tau_proto::TAU_INTERNAL_HEADER_NAME
    )
}

fn persisted_background_terminal(
    harness: &Harness,
    cid: &AgentId,
    call_id: &str,
) -> tau_proto::ObservationId {
    let agent_id = harness.agent_runtime.agent_registry.agents[cid]
        .identity
        .agent_id
        .as_deref()
        .expect("durable agent id");
    harness
        .session_runtime
        .agent_store
        .agent_events(agent_id)
        .expect("agent events")
        .into_iter()
        .find_map(|record| match record.event {
            Event::ToolBackgroundResult(result) if result.call_id.as_str() == call_id => {
                Some(record.observation_id)
            }
            Event::ToolBackgroundError(error) if error.call_id.as_str() == call_id => {
                Some(record.observation_id)
            }
            _ => None,
        })
        .expect("persisted background terminal")
}

fn seed_background_placeholder(state_dir: &Path, call_id: &str, tool_name: &str) {
    seed_background_placeholder_for_agent(state_dir, "main", call_id, tool_name);
}

fn seed_background_placeholder_for_agent(
    state_dir: &Path,
    agent_id: &str,
    call_id: &str,
    tool_name: &str,
) {
    seed_agent_loaded(state_dir, "s1", agent_id);
    let parsed_agent_id = tau_proto::AgentId::parse(agent_id).expect("agent id");
    let mut agent_store =
        tau_core::AgentStore::open(state_dir.join("agents")).expect("agent store");
    agent_store
        .append_agent_event(
            agent_id,
            None,
            Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                inference_activation: false,
                agent_id: parsed_agent_id.clone(),
                text: format!("run {tool_name}"),
                trusted_internal_spans: Vec::new(),
                message_class: tau_proto::PromptMessageClass::User,
                internal_kind: None,
                originator: tau_proto::PromptOriginator::User,
                submission_source: Default::default(),
                display_name: None,
                ctx_id: None,
            }),
        )
        .expect("seed prior user message");
    agent_store
        .append_agent_event(
            agent_id,
            None,
            Event::ProviderResponseFinished(ProviderResponseFinished {
                automatic_compaction_decision: None,
                output_length_disposition: tau_proto::OutputLengthDisposition::None,
                estimated_api_cost_rates: None,
                estimated_api_cost_increment: None,

                agent_prompt_id: test_agent_prompt_id(format!("sp-{call_id}")),
                agent_id: parsed_agent_id,
                output_items: vec![ContextItem::ToolCall(ToolCallItem {
                    call_id: call_id.into(),
                    name: ToolName::new(tool_name),
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
                compaction_output_tokens: None,
                backend: None,
                provider_attempt: Default::default(),
                provider_response_id: None,
                ws_pool_delta: None,
            }),
        )
        .expect("seed background tool call");
    agent_store
        .append_agent_event(
            agent_id,
            None,
            Event::ProviderToolResult(ToolResult {
                presentation: Default::default(),
                call_id: call_id.into(),
                tool_name: ToolName::new(tool_name),
                tool_type: tau_proto::ToolType::Function,
                result: CborValue::Text(format!(
                    "{}: true\n\nTool call `{call_id}` is running in the background.",
                    tau_proto::TAU_INTERNAL_HEADER_NAME
                )),
                provider_content: Vec::new(),
                kind: tau_proto::ToolResultKind::BackgroundPlaceholder,
                originator: tau_proto::PromptOriginator::User,

                display: None,
            }),
        )
        .expect("seed background placeholder");
}

fn seed_background_result(state_dir: &Path, call_id: &str, tool_name: &str, output: &str) {
    let mut agent_store =
        tau_core::AgentStore::open(state_dir.join("agents")).expect("agent store");
    agent_store
        .append_agent_event(
            "main",
            None,
            Event::ToolBackgroundResult(tau_proto::ToolBackgroundResult {
                call_id: call_id.into(),
                tool_name: ToolName::new(tool_name),
                tool_type: tau_proto::ToolType::Function,
                result: CborValue::Text(output.to_owned()),
                originator: tau_proto::PromptOriginator::User,

                display: None,
            }),
        )
        .expect("seed background result");
}

fn seed_background_error(state_dir: &Path, call_id: &str, tool_name: &str, message: &str) {
    let mut agent_store =
        tau_core::AgentStore::open(state_dir.join("agents")).expect("agent store");
    agent_store
        .append_agent_event(
            "main",
            None,
            Event::ToolBackgroundError(tau_proto::ToolBackgroundError {
                call_id: call_id.into(),
                tool_name: ToolName::new(tool_name),
                tool_type: tau_proto::ToolType::Function,
                message: message.to_owned(),
                details: None,
                originator: tau_proto::PromptOriginator::User,

                display: None,
            }),
        )
        .expect("seed background error");
}

fn agent_event_count(h: &Harness, matches_event: impl Fn(&Event) -> bool) -> usize {
    h.agent_runtime
        .agent_registry
        .agent_routes
        .keys()
        .filter_map(|agent_id| h.session_runtime.agent_store.agent_events(agent_id).ok())
        .flatten()
        .filter(|entry| matches_event(&entry.event))
        .count()
}

fn background_error_count(h: &Harness, call_id: &str) -> usize {
    agent_event_count(
        h,
        |event| matches!(event, Event::ToolBackgroundError(error) if error.call_id.as_str() == call_id),
    )
}

fn background_result_count(h: &Harness, call_id: &str) -> usize {
    agent_event_count(
        h,
        |event| matches!(event, Event::ToolBackgroundResult(result) if result.call_id.as_str() == call_id),
    )
}

fn tool_result_count(h: &Harness, call_id: &str) -> usize {
    event_log_count(
        h,
        |event| matches!(event, Event::ToolResult(result) if result.call_id.as_str() == call_id),
    )
}

fn background_placeholder_count(h: &Harness, call_id: &str) -> usize {
    agent_event_count(h, |event| {
        matches!(
            event,
            Event::ProviderToolResult(result)
                if result.call_id.as_str() == call_id
                    && result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder
        )
    })
}

fn event_log_contains(h: &Harness, source: &str, matches_event: impl Fn(&Event) -> bool) -> bool {
    let mut seq = path_crate_event_log::EventLogSeq::new(0);
    while let Some(entry) = h.runtime_io.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if entry.source.as_deref() == Some(source) && matches_event(&entry.event) {
            return true;
        }
    }
    false
}

fn event_log_position(h: &Harness, matches_event: impl Fn(&Event) -> bool) -> Option<u64> {
    let mut seq = path_crate_event_log::EventLogSeq::new(0);
    while let Some(entry) = h.runtime_io.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if matches_event(&entry.event) {
            return Some(entry.seq.get());
        }
    }
    None
}

fn event_log_position_after(
    h: &Harness,
    after_seq: u64,
    matches_event: impl Fn(&Event) -> bool,
) -> Option<u64> {
    let mut seq = path_crate_event_log::EventLogSeq::new(after_seq + 1);
    while let Some(entry) = h.runtime_io.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if matches_event(&entry.event) {
            return Some(entry.seq.get());
        }
    }
    None
}

fn event_log_contains_any_source(h: &Harness, matches_event: impl Fn(&Event) -> bool) -> bool {
    let mut seq = path_crate_event_log::EventLogSeq::new(0);
    while let Some(entry) = h.runtime_io.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if matches_event(&entry.event) {
            return true;
        }
    }
    false
}

pub(super) fn event_log_count(h: &Harness, matches_event: impl Fn(&Event) -> bool) -> usize {
    let mut count = 0;
    let mut seq = path_crate_event_log::EventLogSeq::new(0);
    while let Some(entry) = h.runtime_io.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if matches_event(&entry.event) {
            count += 1;
        }
    }
    count
}

fn shared_test_tool_spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: ToolName::new(name),
        model_visible_name: None,
        description: None,
        parameters: None,
        tool_type: tau_proto::ToolType::Function,
        format: None,
        tags: Vec::new(),
        enabled_by_default: true,
        background_support: None,
        examples: Vec::new(),
    }
}

fn exclusive_test_tool_spec(name: &str) -> ToolSpec {
    ToolSpec {
        ..shared_test_tool_spec(name)
    }
}

fn scheduled_test_tool_spec(
    name: &str,
    background_support: tau_proto::BackgroundSupport,
) -> ToolSpec {
    ToolSpec {
        background_support: Some(background_support),
        ..shared_test_tool_spec(name)
    }
}

/// Builds a harness with one configured owner and an in-flight routed tool
/// call.
pub(super) fn setup_routed_test_tool_call(call_id: &str, tool_name: &str) -> (TempDir, Harness) {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let _ = connect_ready_configured_extension(
        &mut h,
        "conn-owner",
        "configured-owner",
        tau_proto::ClientKind::Tool,
    );
    let _ = connect_ready_configured_extension(
        &mut h,
        "conn-wrong",
        "configured-wrong",
        tau_proto::ClientKind::Tool,
    );
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-owner"),
        shared_test_tool_spec(tool_name),
    );

    let cid = ensure_test_user_agent(&mut h);
    let spid: AgentPromptId = test_agent_prompt_id(format!("sp-{call_id}"));
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), cid);

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: call_id.into(),
            name: ToolName::new(tool_name),
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
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("tool call routed");
    assert_eq!(
        h.tool_routing
            .tool_runtime
            .pending_tool_providers
            .get(call_id)
            .map(|provider_id| provider_id.as_str()),
        Some("conn-owner")
    );

    (td, h)
}

fn tool_invoke_call_ids(events: &Arc<Mutex<Vec<RoutedFrame>>>) -> Vec<String> {
    events
        .lock()
        .expect("sink mutex")
        .iter()
        .filter_map(|routed| match peel_inner_event(&routed.frame) {
            Some(Event::ToolStarted(invoke)) => Some(invoke.call_id.to_string()),
            _ => None,
        })
        .collect()
}

fn loop_guard_tool_error(call_id: &str, tool_name: &str, message: &str) -> tau_proto::ToolError {
    tau_proto::ToolError {
        presentation: Default::default(),
        call_id: call_id.into(),
        tool_name: ToolName::new(tool_name),
        tool_type: tau_proto::ToolType::Function,
        message: message.to_owned(),
        details: None,
        originator: tau_proto::PromptOriginator::User,
        display: None,
    }
}

/// Builds a final successful result fixture for dispatch and interception
/// tests.
pub(super) fn final_tool_result(call_id: &str, tool_name: &str, text: &str) -> ToolResult {
    ToolResult {
        presentation: Default::default(),
        call_id: call_id.into(),
        tool_name: ToolName::new(tool_name),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text(text.to_owned()),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }
}

fn wait_no_args_call(call_id: &str) -> AgentToolCall {
    AgentToolCall {
        call_ref: None,
        id: call_id.into(),
        name: ToolName::new("wait"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(Vec::new()),
    }
}

fn wait_input_call(call_id: &str) -> AgentToolCall {
    AgentToolCall {
        call_ref: None,
        id: call_id.into(),
        name: ToolName::new("wait"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("timeout_minutes".to_owned()),
            CborValue::Integer(1.into()),
        )]),
    }
}

fn provider_input_wait_response(
    prompt: &tau_proto::AgentPromptCreated,
    call_id: &str,
    timeout_minutes: u64,
) -> ProviderResponseFinished {
    provider_tool_response(
        prompt,
        call_id,
        "wait",
        CborValue::Map(vec![(
            CborValue::Text("timeout_minutes".to_owned()),
            CborValue::Integer(timeout_minutes.into()),
        )]),
    )
}

/// Build one provider ToolCalls response for lifecycle tests outside this
/// module.
pub(super) fn provider_tool_response(
    prompt: &tau_proto::AgentPromptCreated,
    call_id: &str,
    tool_name: &str,
    arguments: CborValue,
) -> ProviderResponseFinished {
    ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: prompt.agent_prompt_id.clone(),
        agent_id: prompt.agent_id.clone(),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: call_id.into(),
            name: ToolName::new(tool_name),
            tool_type: tau_proto::ToolType::Function,
            arguments,
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
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    }
}

/// Builds a terminal error fixture for dispatch and interception tests.
pub(super) fn tool_error(call_id: &str, tool_name: &str, message: &str) -> tau_proto::ToolError {
    tau_proto::ToolError {
        presentation: Default::default(),
        call_id: call_id.into(),
        tool_name: ToolName::new(tool_name),
        tool_type: tau_proto::ToolType::Function,
        message: message.to_owned(),
        details: None,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }
}

fn tool_progress(call_id: &str, tool_name: &str, message: &str) -> tau_proto::ToolProgress {
    tau_proto::ToolProgress {
        call_id: call_id.into(),
        tool_name: ToolName::new(tool_name),
        message: Some(message.to_owned()),
        progress: None,
        display: None,
    }
}

fn ext_query(query_id: &str) -> StartAgentRequest {
    StartAgentRequest {
        trusted_internal_spans: Vec::new(),
        parent_agent: None,
        query_id: query_id.to_owned(),
        instruction: format!("instruction {query_id}"),
        role: None,
        input_stats: tau_proto::ToolUseStats::default(),
        tool_call_id: None,
        task_name: None,
    }
}

fn provider_model_info(
    id: tau_proto::ModelId,
    context_window: u64,
) -> tau_proto::ProviderModelInfo {
    tau_proto::ProviderModelInfo {
        id,
        display_name: None,
        tags: Vec::new(),
        hosted_tool_capabilities: Vec::new(),
        supported_tool_types: vec![tau_proto::ToolType::Function],
        input_modalities: Vec::new(),
        tool_result_modalities: Vec::new(),
        supports_parallel_tool_calls: true,
        default_affinity: 0,
        context_window: tau_proto::TokenCount::new(context_window),
        max_input_tokens: None,
        max_output_tokens: None,
        efforts: tau_proto::ReasoningEffortCapability::mapped(vec![
            tau_proto::NativeReasoningEffort::None,
            tau_proto::NativeReasoningEffort::High,
        ]),
        verbosities: vec![tau_proto::Verbosity::Low, tau_proto::Verbosity::High],
        thinking_summaries: vec![
            tau_proto::ThinkingSummary::Off,
            tau_proto::ThinkingSummary::Auto,
        ],
        supports_compaction: false,
        supports_standalone_compaction: false,
        standalone_compaction_generation_negative: false,
        standalone_compaction_threshold: None,
        standalone_compaction_prefix_budget: Some(tau_proto::ByteCount::new(u64::MAX)),
        cache_policy: None,
        est_uncached_input_cost_1m_usd: Default::default(),
        est_cached_input_cost_1m_usd: Default::default(),
        est_cache_write_input_cost_1m_usd: Default::default(),
        est_output_cost_1m_usd: Default::default(),
        est_cache_storage_cost_1m_token_hour_usd: None,
    }
}

fn set_available_provider_models(
    h: &mut Harness,
    models: impl IntoIterator<Item = tau_proto::ProviderModelInfo>,
) {
    let models: Vec<_> = models.into_iter().collect();
    h.provider_runtime.available_models = models.iter().map(|info| info.id.clone()).collect();
    h.provider_runtime.model_info = models
        .into_iter()
        .map(|info| (info.id.clone(), info))
        .collect();
}

fn ext_query_cid(h: &Harness, query_id: &str) -> Option<AgentId> {
    h.agent_runtime
        .agent_registry
        .agents
        .iter()
        .find_map(|(cid, conv)| {
            matches!(
                &conv.identity.originator,
                tau_proto::PromptOriginator::Extension { query_id: id, .. } if id == query_id
            )
            .then_some(cid.clone())
        })
}

struct TestAgentStartBuiltin;

impl crate::InternalToolHandler for TestAgentStartBuiltin {
    fn tool_specs(&self) -> Vec<tau_proto::ToolSpec> {
        vec![tau_proto::ToolSpec {
            name: ToolName::new("agent_start"),
            model_visible_name: None,
            description: Some("test agent_start".to_owned()),
            tool_type: tau_proto::ToolType::Function,
            parameters: Some(serde_json::json!({"type":"object"})),
            format: None,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: Some(tau_proto::BackgroundSupport::Never),
            examples: Vec::new(),
        }]
    }

    fn handles(&self, internal_tool_name: &ToolName) -> bool {
        internal_tool_name.as_str() == "agent_start"
    }

    fn handle_event(
        &self,
        host: &mut crate::InternalToolHost<'_>,
        event: &Event,
    ) -> Result<(), HarnessError> {
        match event {
            Event::ToolStarted(started) => {
                let Some((_conversation_id, call, _visible_tool_name)) =
                    host.internal_started_call(started)
                else {
                    return Ok(());
                };
                let query_id = format!("test-agent-start-{}", call.id);
                host.enqueue_start_agent_request_without_draining(tau_proto::StartAgentRequest {
                    trusted_internal_spans: Vec::new(),
                    parent_agent: None,
                    query_id,
                    instruction: "test builtin agent_start".to_owned(),
                    role: None,
                    input_stats: tau_proto::ToolUseStats::default(),
                    tool_call_id: Some(call.id.clone()),
                    task_name: Some("test agent_start".to_owned()),
                })
                .map_err(HarnessError::Participant)?;
                host.background_tool_call(
                    &call.id,
                    CborValue::Text("test builtin agent_start running in background".to_owned()),
                );
                host.drain_start_agent_requests()
            }
            Event::ToolCancelRequest(request) => {
                let query_id = format!("test-agent-start-{}", request.target_call_id);
                let _ = host.cancel_start_agent_request(&query_id, &request.target_call_id, true);
                Ok(())
            }
            Event::StartAgentResult(result) => {
                let Some(call_id) = result.query_id.strip_prefix("test-agent-start-") else {
                    return Ok(());
                };
                host.finish_prebuilt_tool_error(tau_proto::ToolError {
                    presentation: Default::default(),
                    call_id: call_id.into(),
                    tool_name: ToolName::new("agent_start"),
                    tool_type: tau_proto::ToolType::Function,
                    message: result
                        .error
                        .clone()
                        .unwrap_or_else(|| "test agent_start failed".to_owned()),
                    details: None,
                    display: None,
                    originator: tau_proto::PromptOriginator::User,
                });
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

/// Pumps until ext-shell reports one terminal for `call_id`, then returns the
/// raw extension report after committing it through the production harness
/// path.
fn drive_harness_until_extension_tool_report(h: &mut Harness, call_id: &str) -> Event {
    let started = Instant::now();
    loop {
        if Duration::from_secs(3) <= started.elapsed() {
            panic!("timed out waiting for ext-shell report for {call_id}");
        }
        let Ok(event) = h.runtime_io.rx.recv_timeout(Duration::from_millis(100)) else {
            continue;
        };
        let event = h.expand_component_ingress_wake(event);
        match event {
            HarnessEvent::FromConnection {
                connection_id,
                message,
                ..
            } => {
                let terminal_report = match message.as_ref() {
                    HarnessInputMessage::Emit(emit) => match emit.event.as_ref() {
                        Event::ToolResultReported(result) if result.call_id.as_str() == call_id => {
                            Some(emit.event.as_ref().clone())
                        }
                        Event::ToolErrorReported(error) if error.call_id.as_str() == call_id => {
                            Some(emit.event.as_ref().clone())
                        }
                        _ => None,
                    },
                    _ => None,
                };
                h.handle_extension_message(&connection_id, *message)
                    .expect("handle");
                if let Some(report) = terminal_report {
                    return report;
                }
            }
            HarnessEvent::Disconnected { connection_id } => {
                h.handle_disconnect(&connection_id);
            }
            HarnessEvent::ReadFailed { connection_id, .. } => {
                h.handle_disconnect(&connection_id);
            }
            HarnessEvent::NewClient(_) => {}
            HarnessEvent::SupervisedWriterCleanupComplete { connection_id } => h
                .handle_supervised_writer_cleanup_complete_at(&connection_id, Instant::now())
                .expect("supervised cleanup"),
            HarnessEvent::ComponentIngressReady => unreachable!("wake expanded"),
            HarnessEvent::Command(command) => h.handle_harness_command(command).expect("handle"),
        }
    }
}

pub(super) fn context_overflow_response(
    prompt: &tau_proto::AgentPromptCreated,
) -> ProviderResponseFinished {
    ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: prompt.agent_prompt_id.clone(),
        agent_id: prompt.agent_id.clone(),
        output_items: Vec::new(),
        stop_reason: tau_proto::ProviderStopReason::Error,
        error: Some("bounded context rejection".to_owned()),
        failure_kind: Some(tau_proto::ProviderFailureKind::ContextWindowExceeded),
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        originator: prompt.originator.clone(),
        usage: None,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    }
}

fn seed_reactive_compaction_prefix(h: &mut Harness, cid: &AgentId) {
    let agent_id = durable_agent_id_for_conversation(h, cid);
    h.publish_for_agent(
        cid,
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            inference_activation: false,
            agent_id,
            text: "stable reactive prefix".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
    );
}

/// Build one successful standalone-compaction response with a single
/// replacement summary.
pub(super) fn standalone_compaction_success_response(
    prompt: &tau_proto::AgentPromptCreated,
    summary: &str,
) -> ProviderResponseFinished {
    ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,
        agent_prompt_id: prompt.agent_prompt_id.clone(),
        agent_id: prompt.agent_id.clone(),
        output_items: vec![ContextItem::Message(tau_proto::MessageItem {
            role: tau_proto::ContextRole::Assistant,
            content: vec![tau_proto::ContentPart::Text {
                text: summary.to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        originator: prompt.originator.clone(),
        usage: None,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    }
}

/// Expected provider-submission phase for an inference lifecycle assertion.
enum ExpectedProviderSubmission {
    /// The provider submission has not been reported.
    Pending,
    /// The provider submission has committed.
    Submitted,
}

/// Assert one agent's exact inference-dispatch lifecycle and return its
/// branch-owned checkpoint.
fn assert_inference_dispatch_lifecycle(
    events: &[Event],
    agent_id: &tau_proto::AgentId,
    expected_through: tau_proto::AgentHead,
    expected_cut: Option<tau_proto::AgentHead>,
    expected_submission: ExpectedProviderSubmission,
) -> tau_proto::AgentInferenceDispatchStarted {
    let checkpoints = events
        .iter()
        .filter_map(|event| match event {
            Event::AgentInferenceDispatchStarted(started) if &started.agent_id == agent_id => {
                Some(started)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(checkpoints.len(), 1);
    let checkpoint = checkpoints[0];
    assert_eq!(checkpoint.through, expected_through);
    assert_eq!(checkpoint.activation_cut, expected_cut);
    assert_eq!(checkpoint.model, Some("test/model".into()));
    assert_eq!(
        checkpoint.operation,
        Some(tau_proto::PromptOperation::Inference)
    );
    let sequence = events
        .iter()
        .filter_map(|event| match event {
            Event::AgentInferenceDispatchStarted(started)
                if started.agent_id == *agent_id
                    && started.agent_prompt_id == checkpoint.agent_prompt_id =>
            {
                Some("checkpoint")
            }
            Event::AgentPromptStarted(started)
                if started.agent_id == *agent_id
                    && started.agent_prompt_id == checkpoint.agent_prompt_id =>
            {
                Some("started")
            }
            Event::AgentPromptCreated(created)
                if created.agent_id == *agent_id
                    && created.agent_prompt_id == checkpoint.agent_prompt_id =>
            {
                Some("created")
            }
            Event::ProviderPromptSubmitted(submitted)
                if submitted.agent_prompt_id == checkpoint.agent_prompt_id =>
            {
                Some("submitted")
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let expected_sequence = match expected_submission {
        ExpectedProviderSubmission::Pending => vec!["checkpoint", "started", "created"],
        ExpectedProviderSubmission::Submitted => {
            vec!["checkpoint", "started", "created", "submitted"]
        }
    };
    assert_eq!(sequence, expected_sequence);
    checkpoint.clone()
}

/// Assert that live dispatch ownership exactly matches a committed inference
/// checkpoint.
fn assert_inference_dispatch_owner(
    agent: &Agent,
    checkpoint: &tau_proto::AgentInferenceDispatchStarted,
) {
    assert!(matches!(
        &agent.dispatch.activation_dispatch,
        crate::agent::ActivationDispatchState::DispatchUncertain {
            owner: crate::agent::InferenceCheckpointOwner::Inference,
            agent_prompt_id,
            through,
            model: Some(model),
            operation: Some(tau_proto::PromptOperation::Inference),
            activation_cut,
        } if agent_prompt_id == &checkpoint.agent_prompt_id
            && *through == checkpoint.through
            && model == &tau_proto::ModelId::from("test/model")
            && *activation_cut == checkpoint.activation_cut
    ));
}

/// Enables the shared fake model's remote-compaction capability for focused
/// lifecycle tests in sibling harness test modules.
pub(super) fn enable_remote_compaction_for_test_model(h: &mut Harness) {
    h.config.selected_model = Some("test/model".into());
    h.provider_runtime.model_info.insert(
        "test/model".into(),
        tau_proto::ProviderModelInfo {
            id: "test/model".into(),
            display_name: None,
            tags: Vec::new(),
            hosted_tool_capabilities: Vec::new(),
            supported_tool_types: vec![tau_proto::ToolType::Function],
            input_modalities: Vec::new(),
            tool_result_modalities: Vec::new(),
            supports_parallel_tool_calls: true,
            default_affinity: 0,
            context_window: tau_proto::TokenCount::new(1_000),
            max_input_tokens: None,
            max_output_tokens: None,
            efforts: tau_proto::ReasoningEffortCapability::mapped(vec![
                tau_proto::NativeReasoningEffort::Medium,
            ]),
            verbosities: vec![tau_proto::Verbosity::Medium],
            thinking_summaries: vec![tau_proto::ThinkingSummary::Auto],
            supports_compaction: true,
            supports_standalone_compaction: false,
            standalone_compaction_generation_negative: false,
            standalone_compaction_threshold: None,
            standalone_compaction_prefix_budget: Some(tau_proto::ByteCount::new(u64::MAX)),
            cache_policy: None,
            est_uncached_input_cost_1m_usd: Default::default(),
            est_cached_input_cost_1m_usd: Default::default(),
            est_cache_write_input_cost_1m_usd: Default::default(),
            est_output_cost_1m_usd: Default::default(),
            est_cache_storage_cost_1m_token_hour_usd: None,
        },
    );
}
fn strict_fake_compact_response(
    prompt: &tau_proto::AgentPromptCreated,
) -> Result<ProviderResponseFinished, String> {
    validate_closed_tool_timeline(&prompt.context).map_err(str::to_owned)?;
    Ok(provider_text_response(
        &prompt.agent_prompt_id,
        prompt.agent_id.clone(),
        "strict fake replacement",
    ))
}

fn seed_historical_open_prefix_failure(
    h: &mut Harness,
    cid: &AgentId,
    model: &str,
) -> (
    tau_proto::AgentHead,
    tau_proto::AgentHead,
    tau_proto::AgentHead,
) {
    let agent_id = h.agent_runtime.agent_registry.agents[cid]
        .identity
        .agent_id
        .clone()
        .expect("durable agent");
    h.publish_for_agent(
        cid,
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            inference_activation: false,
            agent_id: crate::parse_agent_id(&agent_id),
            text: "historical prefix".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
    );
    let prefix = tau_proto::AgentHead::Node(
        h.agent_runtime.agent_registry.agents[cid]
            .identity
            .head
            .expect("prefix"),
    );
    h.publish_for_agent(
        cid,
        Event::ProviderResponseFinished(ProviderResponseFinished {
            automatic_compaction_decision: None,
            output_length_disposition: tau_proto::OutputLengthDisposition::None,
            estimated_api_cost_rates: None,
            estimated_api_cost_increment: None,

            agent_prompt_id: test_agent_prompt_id("ap-historical-inference"),
            agent_id: crate::parse_agent_id(&agent_id),
            output_items: vec![ContextItem::ToolCall(ToolCallItem {
                call_id: "call-historical".into(),
                name: ToolName::new("historical_tool"),
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
            compaction_output_tokens: None,
            backend: None,
            provider_attempt: Default::default(),
            provider_response_id: None,
            ws_pool_delta: None,
        }),
    );
    let assistant = tau_proto::AgentHead::Node(
        h.agent_runtime.agent_registry.agents[cid]
            .identity
            .head
            .expect("assistant"),
    );
    h.publish_for_agent(
        cid,
        Event::ProviderToolResult(ToolResult {
            presentation: Default::default(),
            call_id: "call-historical".into(),
            tool_name: ToolName::new("historical_tool"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("historical output".to_owned()),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        }),
    );
    let results = tau_proto::AgentHead::Node(
        h.agent_runtime.agent_registry.agents[cid]
            .identity
            .head
            .expect("results"),
    );
    let transaction_id =
        tau_proto::CompactionTransactionId::parse("ct-historical-open-prefix").expect("id");
    h.publish_for_agent(
        cid,
        Event::AgentStandaloneCompactionStarted(tau_proto::AgentStandaloneCompactionStarted {
            compact_prompt_id: test_agent_prompt_id(format!(
                "ap-{agent_id}-historical-open-prefix"
            )),
            operation: tau_proto::PromptOperation::StandaloneCompaction,
            agent_id: crate::parse_agent_id(&agent_id),
            transaction_id: transaction_id.clone(),
            cut: assistant,
            resume_through: Some(results),
            model: model.into(),
            originator: tau_proto::PromptOriginator::User,
            supersedes: None,
            trigger: tau_proto::StandaloneCompactionTrigger::Manual,
        }),
    );
    h.publish_for_agent(
        cid,
        Event::AgentStandaloneCompactionFailed(tau_proto::AgentStandaloneCompactionFailed {
            agent_id: crate::parse_agent_id(&agent_id),
            transaction_id,
            cut: assistant,
            reason: tau_proto::StandaloneCompactionFailureReason::ProviderError,
            resume_through: Some(results),
            context_retreat: None,
            incomplete_response: None,
        }),
    );
    (prefix, assistant, results)
}

/// Test-only internal handler that exercises scheduler-driven compaction.
struct SchedulerCompactionTools;

impl crate::internal_tools::InternalToolHandler for SchedulerCompactionTools {
    fn tool_specs(&self) -> Vec<ToolSpec> {
        ["compact", "agent_compact"]
            .into_iter()
            .map(|name| ToolSpec {
                name: ToolName::new(name),
                model_visible_name: None,
                description: Some("test compaction tool".to_owned()),
                parameters: None,
                tool_type: tau_proto::ToolType::Function,
                format: None,
                tags: Vec::new(),
                enabled_by_default: name == "compact",
                background_support: Some(tau_proto::BackgroundSupport::Instant),
                examples: Vec::new(),
            })
            .collect()
    }

    fn handles(&self, internal_tool_name: &ToolName) -> bool {
        matches!(internal_tool_name.as_str(), "compact" | "agent_compact")
    }

    fn handle_event(
        &self,
        host: &mut crate::internal_tools::InternalToolHost<'_>,
        event: &Event,
    ) -> Result<(), HarnessError> {
        let Event::ToolStarted(started) = event else {
            return Ok(());
        };
        let Some((cid, call, visible_name)) = host.internal_started_call(started) else {
            return Ok(());
        };
        let target = if call.name.as_str() == "agent_compact" {
            let CborValue::Map(fields) = &call.arguments else {
                return Ok(());
            };
            fields.iter().find_map(|(key, value)| match (key, value) {
                (CborValue::Text(key), CborValue::Text(value)) if key == "agent_id" => {
                    tau_proto::AgentId::parse(value).ok()
                }
                _ => None,
            })
        } else {
            None
        };
        host.request_agent_tool_compaction(&cid, &call, visible_name, target.as_ref())
    }
}

/// Register [`SchedulerCompactionTools`] after constructing the generic
/// harness.
fn install_scheduler_compaction_tools(harness: &mut Harness) {
    let handler: std::sync::Arc<dyn crate::internal_tools::InternalToolHandler> =
        path_std_sync::Arc::new(SchedulerCompactionTools);
    {
        let mut host = path_crate_internal_tools::InternalToolHost::new(harness);
        for spec in handler.tool_specs() {
            host.register_internal_tool(spec, None);
        }
    }
    harness.tool_routing.internal_tool_handlers.push(handler);
}

/// Build a synchronous provider terminal that asks the test adapter to drive
/// the production scheduler and compaction-host path.
fn provider_compact_call(
    prompt: &AgentPromptCreated,
    call_id: &ToolCallId,
) -> ProviderResponseFinished {
    ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: prompt.agent_prompt_id.clone(),
        agent_id: prompt.agent_id.clone(),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: call_id.clone(),
            name: ToolName::new("compact"),
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
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    }
}

fn setup_manual_cross_compaction_test() -> (
    TempDir,
    Harness,
    AgentId,
    AgentId,
    AgentToolCall,
    tau_proto::AgentId,
) {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("harness");
    h.provider_runtime
        .model_info
        .get_mut(&"echo/model".into())
        .expect("echo model")
        .supports_standalone_compaction = true;
    let caller_cid = ensure_test_user_agent(&mut h);
    let (target_cid, target_id) = install_manual_compaction_target(&mut h, "unrelated-target");
    let call = register_manual_cross_compaction_call(&mut h, &caller_cid, "call-cross-compact");
    (td, h, caller_cid, target_cid, call, target_id)
}

/// Install one loaded idle target with the same initial local correlation
/// counters as other targets created by this helper.
fn install_manual_compaction_target(
    h: &mut Harness,
    target_agent_id: &str,
) -> (AgentId, tau_proto::AgentId) {
    let target_cid: AgentId = crate::parse_agent_id(target_agent_id);
    let mut target = Agent::new(
        target_cid.clone(),
        2,
        h.session_runtime.current_session_id.clone(),
        tau_proto::PromptOriginator::User,
        None,
        None,
    );
    target.identity.agent_id = Some(crate::parse_agent_id(target_agent_id));
    target.identity.role = Some(h.config.selected_role.clone());
    h.agent_runtime
        .agent_registry
        .agents
        .insert(target_cid.clone(), target);
    h.agent_runtime
        .agent_registry
        .agent_routes
        .insert(crate::parse_agent_id(target_agent_id), target_cid.clone());
    h.publish_for_agent(
        &target_cid,
        Event::AgentStarted(tau_proto::AgentStarted {
            creator: Some(tau_proto::AgentCreator::default()),

            agent_id: tau_proto::AgentId::parse(target_agent_id).expect("target id"),
            parent_agent: None,
            role: h.config.selected_role.clone(),
            display_name: None,
            metadata: Vec::new(),
            ephemeral: false,
        }),
    );
    h.ensure_loaded_agent_for_agent(&target_cid, &crate::parse_agent_id(target_agent_id));
    (
        target_cid,
        tau_proto::AgentId::parse(target_agent_id).expect("target id"),
    )
}

fn register_manual_cross_compaction_call(
    h: &mut Harness,
    caller_cid: &AgentId,
    call_id: &str,
) -> AgentToolCall {
    seed_assistant_tool_round(h, caller_cid, &[(call_id, "agent_compact")]);
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert(call_id.into(), caller_cid.clone());
    h.tool_routing.tool_runtime.pending_tools.insert(
        call_id.into(),
        PendingTool {
            name: ToolName::new("agent_compact"),
            internal_name: ToolName::new("agent_compact"),
            tool_type: tau_proto::ToolType::Function,
            allows_provider_image: false,
        },
    );
    h.tool_routing
        .tool_runtime
        .tool_turn
        .record_unqueued_in_flight(
            caller_cid.clone(),
            call_id.into(),
            ToolTurnCategories::default(),
        );
    h.prompt_coordination
        .prompt_runtime
        .record_tool_call_prompt(call_id.into(), test_agent_prompt_id("sp-seeded-tools"));
    AgentToolCall {
        call_ref: None,
        id: call_id.into(),
        name: ToolName::new("agent_compact"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(Vec::new()),
    }
}

fn durable_compaction_counts(
    h: &Harness,
    target_id: &tau_proto::AgentId,
) -> (usize, usize, usize, usize) {
    let events = h
        .session_runtime
        .agent_store
        .agent_events(target_id.as_str())
        .expect("target events");
    (
        events
            .iter()
            .filter(|record| matches!(record.event, Event::AgentManualCompactionRequested(_)))
            .count(),
        events
            .iter()
            .filter(|record| matches!(record.event, Event::AgentStandaloneCompactionStarted(_)))
            .count(),
        events
            .iter()
            .filter(|record| matches!(record.event, Event::AgentStandaloneCompactionFailed(_)))
            .count(),
        events
            .iter()
            .filter(|record| matches!(record.event, Event::AgentCompacted(_)))
            .count(),
    )
}

fn durable_background_outcome_counts(
    h: &Harness,
    caller_id: &tau_proto::AgentId,
    call_id: &str,
) -> (usize, usize) {
    let events = h
        .session_runtime
        .agent_store
        .agent_events(caller_id.as_str())
        .expect("caller events");
    (
        events
            .iter()
            .filter(|record| {
                matches!(
                    &record.event,
                    Event::ToolBackgroundResult(result) if result.call_id.as_str() == call_id
                )
            })
            .count(),
        events
            .iter()
            .filter(|record| {
                matches!(
                    &record.event,
                    Event::ToolBackgroundError(error) if error.call_id.as_str() == call_id
                )
            })
            .count(),
    )
}

fn assert_failed_manual_tool_recovery(cold_reopen: bool) {
    let (td, mut h, caller_cid, target_cid, first_call, target_id) =
        setup_manual_cross_compaction_test();
    let state = td.path().join("state");
    let caller_id = durable_agent_id_for_conversation(&h, &caller_cid);
    let (closed_prefix, _, owed_resume) =
        seed_historical_open_prefix_failure(&mut h, &target_cid, "echo/model");
    let generation = h
        .session_runtime
        .agent_store
        .agent(target_id.as_str())
        .expect("target tree")
        .ordinary_inference_generation();

    h.request_agent_tool_compaction(
        &caller_cid,
        &first_call,
        ToolName::new("agent_compact"),
        Some(&target_id),
    );
    let first_started = h
        .session_runtime
        .agent_store
        .agent_events(target_id.as_str())
        .expect("target events")
        .into_iter()
        .filter_map(|record| match record.event {
            Event::AgentStandaloneCompactionStarted(started)
                if matches!(
                    started.trigger,
                    tau_proto::StandaloneCompactionTrigger::ManualAgentTool { .. }
                ) =>
            {
                Some(started)
            }
            _ => None,
        })
        .next()
        .expect("first manual transaction");
    let first_prompt = h
        .read_agent_prompt_created(
            &h.session_runtime.current_session_id,
            &first_started.compact_prompt_id,
        )
        .expect("first compact prompt");
    h.handle_provider_response_finished(context_overflow_response(&first_prompt))
        .expect("fail first manual transaction");

    assert_eq!(durable_compaction_counts(&h, &target_id), (1, 2, 2, 0));
    assert_eq!(
        durable_background_outcome_counts(&h, &caller_id, first_call.id.as_str()),
        (0, 1)
    );
    assert_eq!(
        h.session_runtime
            .agent_store
            .agent(target_id.as_str())
            .expect("target tree")
            .ordinary_inference_generation(),
        generation,
        "standalone failure must not advance the ordinary generation"
    );
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&target_cid]
            .dispatch
            .activation_dispatch,
        crate::agent::ActivationDispatchState::None
    ));
    assert_eq!(
        h.session_runtime
            .agent_store
            .agent_events(target_id.as_str())
            .expect("target events")
            .iter()
            .filter(|record| matches!(
                record.event,
                Event::AgentPromptStarted(ref prompt)
                    if prompt.operation == tau_proto::PromptOperation::StandaloneCompaction
            ))
            .count(),
        2,
        "the terminal failure must not retry provider work"
    );

    let before_reopen = durable_compaction_counts(&h, &target_id);
    let (mut h, caller_cid, target_cid) = if cold_reopen {
        h.shutdown().expect("shutdown before cold reopen");
        drop(h);
        assert!(
            !tau_core::session_is_locked(&tau_config::settings::sessions_dir_of(&state), "s1")
                .expect("session lock probe"),
            "joined shutdown must release the session lock"
        );
        let mut resumed =
            echo_harness_with_start_reason("s1", &state, tau_proto::SessionStartReason::Resume)
                .expect("cold reopen");
        resumed
            .provider_runtime
            .model_info
            .get_mut(&"echo/model".into())
            .expect("echo model")
            .supports_standalone_compaction = true;
        let resumed_caller = resumed
            .agent_runtime
            .agent_registry
            .agent_routes
            .get(caller_id.as_str())
            .cloned()
            .expect("restored caller");
        let resumed_target = resumed
            .agent_runtime
            .agent_registry
            .agent_routes
            .get(target_id.as_str())
            .cloned()
            .expect("restored target");
        assert_eq!(
            durable_compaction_counts(&resumed, &target_id),
            before_reopen,
            "cold replay must reconstruct the same durable lifecycle"
        );
        assert_eq!(
            durable_background_outcome_counts(&resumed, &caller_id, first_call.id.as_str()),
            (0, 1),
            "cold repair must not duplicate the first background error"
        );
        assert!(
            !event_log_events(&resumed)
                .iter()
                .any(|event| matches!(event, Event::AgentPromptCreated(_))),
            "cold replay must not automatically retry the failed transaction"
        );
        (resumed, resumed_caller, resumed_target)
    } else {
        (h, caller_cid, target_cid)
    };

    let second_call =
        register_manual_cross_compaction_call(&mut h, &caller_cid, "call-cross-compact-recovery");
    h.request_agent_tool_compaction(
        &caller_cid,
        &second_call,
        ToolName::new("agent_compact"),
        Some(&target_id),
    );
    assert_eq!(
        h.session_runtime
            .agent_store
            .agent(target_id.as_str())
            .expect("target tree")
            .ordinary_inference_generation(),
        generation,
        "explicit recovery must remain in the failed request's generation"
    );
    let starts = h
        .session_runtime
        .agent_store
        .agent_events(target_id.as_str())
        .expect("target events")
        .into_iter()
        .filter_map(|record| match record.event {
            Event::AgentStandaloneCompactionStarted(started) => Some(started),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(starts.len(), 3);
    let successor = starts.last().expect("successor");
    assert_eq!(
        successor.supersedes.as_ref(),
        Some(&first_started.transaction_id)
    );
    assert_eq!(successor.cut, closed_prefix);
    let tree = h
        .session_runtime
        .agent_store
        .agent(target_id.as_str())
        .expect("target tree");
    assert!(tree.contains_head_ancestry(successor.cut, owed_resume));
    assert!(tree.contains_head_ancestry(
        owed_resume,
        successor.resume_through.expect("successor owed resume")
    ));
    assert!(tree.contains_head_ancestry(
        first_started.resume_through.expect("first owed resume"),
        successor.resume_through.expect("successor owed resume")
    ));
    assert_eq!(durable_compaction_counts(&h, &target_id), (2, 3, 2, 0));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolError(error)
            if error.call_id == second_call.id && error.message == "not_needed"
    )));

    let successor_prompt = h
        .read_agent_prompt_created(
            &h.session_runtime.current_session_id,
            &successor.compact_prompt_id,
        )
        .expect("successor compact prompt");
    h.handle_provider_response_finished(provider_text_response(
        &successor_prompt.agent_prompt_id,
        successor_prompt.agent_id,
        "recovered summary",
    ))
    .expect("complete explicit recovery");
    assert_eq!(durable_compaction_counts(&h, &target_id), (2, 3, 2, 1));
    assert_eq!(
        durable_background_outcome_counts(&h, &caller_id, first_call.id.as_str()),
        (0, 1)
    );
    assert_eq!(
        durable_background_outcome_counts(&h, &caller_id, second_call.id.as_str()),
        (1, 0)
    );
    let successor_outcomes = h
        .session_runtime
        .agent_store
        .agent_events(target_id.as_str())
        .expect("target events")
        .iter()
        .filter(|record| match &record.event {
            Event::AgentCompacted(compacted) => {
                compacted.transaction_id.as_ref() == Some(&successor.transaction_id)
            }
            Event::AgentStandaloneCompactionFailed(failed) => {
                failed.transaction_id == successor.transaction_id
            }
            _ => false,
        })
        .count();
    assert_eq!(successor_outcomes, 1);
    assert!(!matches!(
        h.agent_runtime.agent_registry.agents[&target_cid]
            .dispatch
            .activation_dispatch,
        crate::agent::ActivationDispatchState::None
    ));
    h.shutdown().expect("shutdown");
}

fn assert_manual_cross_compaction_error(h: &Harness, call: &AgentToolCall, expected: &str) {
    assert!(event_log_contains_any_source(h, |event| matches!(
        event,
        Event::ToolError(error)
            if error.call_id == call.id && error.message == expected
    )));
    assert!(!event_log_contains_any_source(h, |event| matches!(
        event,
        Event::AgentManualCompactionRequested(requested)
            if requested.tool_source().is_some_and(|source| {
                source.initiating_tool_call_id == call.id
            })
    )));
}

fn instant_background_test_tool_spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: ToolName::new(name),
        model_visible_name: None,
        description: None,
        parameters: None,
        tool_type: tau_proto::ToolType::Function,
        format: None,
        tags: Vec::new(),
        enabled_by_default: true,
        background_support: Some(tau_proto::BackgroundSupport::Instant),
        examples: Vec::new(),
    }
}

fn active_prompt_for(h: &Harness, cid: &AgentId) -> AgentPromptId {
    h.agent_runtime
        .agent_registry
        .agents
        .get(cid)
        .expect("conversation exists")
        .dispatch
        .in_flight_prompt
        .clone()
        .expect("active prompt")
}

fn start_background_tool_and_finish_placeholder_turn(
    h: &mut Harness,
    cid: &AgentId,
    call_id: &str,
    tool_name: &str,
) {
    let agent_id = h
        .ensure_agent_id_for_agent(cid)
        .unwrap_or_else(|| crate::parse_agent_id("main"));
    h.publish_for_agent(
        cid,
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            inference_activation: false,
            agent_id: crate::parse_agent_id(&agent_id),
            text: format!("run {tool_name}"),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
    );
    let spid: AgentPromptId = test_agent_prompt_id(format!("sp-{call_id}"));
    seed_agent_thinking(h, cid, spid.as_str());
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), cid.clone());
    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid,
        agent_id: crate::parse_agent_id(&agent_id),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: call_id.into(),
            name: ToolName::new(tool_name),
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
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("start background tool");
    assert!(
        h.tool_routing
            .tool_runtime
            .tool_turn
            .is_backgrounded(&ToolCallId::from(call_id))
    );

    let placeholder_followup = active_prompt_for(h, cid);
    h.handle_provider_response_finished(provider_text_response(
        &placeholder_followup,
        durable_agent_id_for_conversation(h, cid),
        "placeholder acknowledged",
    ))
    .expect("finish placeholder followup");
    assert!(matches!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .expect("conversation exists")
            .turn
            .turn_state,
        AgentTurnState::Idle
    ));
}

struct ReentrantDelegateCompletionPrompt {
    target_agent_id: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

impl crate::InternalToolHandler for ReentrantDelegateCompletionPrompt {
    fn tool_specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    fn handles(&self, _internal_tool_name: &ToolName) -> bool {
        false
    }

    fn handle_event(
        &self,
        host: &mut crate::InternalToolHost<'_>,
        event: &Event,
    ) -> Result<(), HarnessError> {
        let Event::StartAgentResult(result) = event else {
            return Ok(());
        };
        if result.query_id != "q-reentrant" {
            return Ok(());
        }
        let agent_id = self
            .target_agent_id
            .lock()
            .expect("target agent id")
            .clone()
            .expect("target agent configured");
        host.dispatch_test_background_completion(
            &agent_id,
            background_completion_prompt(&ToolCallId::from("canceled-shell")),
        )
    }
}

fn start_agent_request_error(
    frames: &Arc<Mutex<Vec<RoutedFrame>>>,
    query_id: &str,
) -> Option<String> {
    frames
        .lock()
        .expect("frames")
        .iter()
        .find_map(|routed| match peel_inner_event(&routed.frame) {
            Some(Event::StartAgentResult(result)) if result.query_id == query_id => {
                result.error.clone()
            }
            _ => None,
        })
}

fn drain_watches_updated(
    frames: &Arc<Mutex<Vec<RoutedFrame>>>,
) -> Vec<tau_proto::AgentWatchesUpdated> {
    let mut frames = frames.lock().expect("frames");
    let mut out = Vec::new();
    frames.retain(|routed| match peel_inner_event(&routed.frame) {
        Some(Event::AgentWatchesUpdated(snapshot)) => {
            out.push(snapshot.clone());
            false
        }
        _ => true,
    });
    out
}

fn drain_stats_updated(frames: &Arc<Mutex<Vec<RoutedFrame>>>) -> Vec<tau_proto::AgentStatsUpdated> {
    let mut frames = frames.lock().expect("frames");
    let mut out = Vec::new();
    frames.retain(|routed| match peel_inner_event(&routed.frame) {
        Some(Event::AgentStatsUpdated(stats)) => {
            out.push(stats.clone());
            false
        }
        _ => true,
    });
    out
}

fn configure_delegate_error_roles(h: &mut Harness) {
    let available_model: tau_proto::ModelId = "test/available".into();
    set_available_provider_models(h, [provider_model_info(available_model.clone(), 128_000)]);
    h.config.available_roles = path_std_collections::HashMap::from([
        (
            "beta".to_owned(),
            tau_config::settings::AgentRole {
                model: Some(available_model.clone()),
                ..Default::default()
            },
        ),
        (
            "alpha".to_owned(),
            tau_config::settings::AgentRole {
                model: Some(available_model),
                ..Default::default()
            },
        ),
        (
            "offline".to_owned(),
            tau_config::settings::AgentRole {
                model: Some("test/offline".into()),
                ..Default::default()
            },
        ),
    ]);
}

/// Test-only internal status handler that rejects every admitted call.
struct RejectingStatusTool;

impl crate::InternalToolHandler for RejectingStatusTool {
    fn tool_specs(&self) -> Vec<tau_proto::ToolSpec> {
        vec![shared_test_tool_spec("status")]
    }

    fn handles(&self, internal_tool_name: &ToolName) -> bool {
        internal_tool_name.as_str() == "status"
    }

    fn handle_event(
        &self,
        host: &mut crate::InternalToolHost<'_>,
        event: &Event,
    ) -> Result<(), HarnessError> {
        let Event::ToolStarted(started) = event else {
            return Ok(());
        };
        let Some(owner) = host.agent_owned_internal_started_call(started) else {
            return Ok(());
        };
        host.finish_tool_with_error(
            owner.conversation_id(),
            owner.call().id.clone(),
            owner.visible_tool_name().clone(),
            owner.call().tool_type,
            "invalid status state".to_owned(),
            None,
        );
        Ok(())
    }
}

/// Test-only internal tool that records the harness-owned self snapshot.
struct RecordingSelfInfoTool(
    /// Most recent authoritative snapshot observed through the host seam.
    path_std_sync::Mutex<Option<path_crate_internal_tools::InternalSelfInfo>>,
);

/// Test handler that attempts to claim the reserved intrinsic name.
struct ReservedSelfInfoClaim;

impl crate::InternalToolHandler for ReservedSelfInfoClaim {
    fn tool_specs(&self) -> Vec<tau_proto::ToolSpec> {
        vec![
            shared_test_tool_spec("self_info"),
            shared_test_tool_spec("must_not_register"),
        ]
    }

    fn handles(&self, internal_tool_name: &ToolName) -> bool {
        internal_tool_name.as_str() == "self_info"
    }
}

impl crate::InternalToolHandler for RecordingSelfInfoTool {
    fn tool_specs(&self) -> Vec<tau_proto::ToolSpec> {
        vec![shared_test_tool_spec("record_self_info")]
    }

    fn handles(&self, internal_tool_name: &ToolName) -> bool {
        internal_tool_name.as_str() == "record_self_info"
    }

    fn handle_event(
        &self,
        host: &mut crate::InternalToolHost<'_>,
        event: &Event,
    ) -> Result<(), HarnessError> {
        let Event::ToolStarted(started) = event else {
            return Ok(());
        };
        let Some(owner) = host.agent_owned_internal_started_call(started) else {
            return Ok(());
        };
        let info = host.self_info(&owner).expect("self metadata");
        *self.0.lock().expect("self-info recorder") = Some(info);
        host.finish_tool_with_cbor_result(
            owner.conversation_id(),
            owner.call().id.clone(),
            owner.visible_tool_name().clone(),
            owner.call().tool_type,
            CborValue::Text("recorded".to_owned()),
            None,
        );
        Ok(())
    }
}

/// Builds one canonical production `message` tool call for focused routing
/// tests.
fn message_tool_call(id: &str, recipient_id: &str, message: &str) -> AgentToolCall {
    AgentToolCall {
        call_ref: None,
        id: id.into(),
        name: ToolName::new(path_crate_harness::subagents_tool::MESSAGE_TOOL_NAME),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![
            (
                CborValue::Text("recipient_id".to_owned()),
                CborValue::Text(recipient_id.to_owned()),
            ),
            (
                CborValue::Text("message".to_owned()),
                CborValue::Text(message.to_owned()),
            ),
        ]),
    }
}

fn session_agent_message_sent_events(h: &Harness) -> Vec<tau_proto::AgentMessageSent> {
    event_log_events(h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentMessageSent(message) => Some(message),
            _ => None,
        })
        .collect()
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

fn durable_agent_message_sent_events(h: &Harness) -> Vec<tau_proto::AgentMessageSent> {
    loaded_agent_events(h, "s1")
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentMessageSent(message) => Some(message),
            _ => None,
        })
        .collect()
}

fn durable_agent_message_received_events(h: &Harness) -> Vec<tau_proto::AgentMessageReceived> {
    loaded_agent_events(h, "s1")
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentMessageReceived(message) => Some(message),
            _ => None,
        })
        .collect()
}

fn cache_miss_diagnostic_for_test(prompt_id: &str) -> tau_proto::ProviderCacheMissDiagnostic {
    tau_proto::ProviderCacheMissDiagnostic {
        agent_prompt_id: test_agent_prompt_id(prompt_id),
        model: "provider/model".into(),
        originator: tau_proto::PromptOriginator::User,
        tool_choice: tau_proto::ToolChoice::default(),
        ws_pool_delta: None,
        input_tokens: 1,
        cached_tokens: 0,
        previous_input_tokens: 1,
        cacheable_input_tokens: 1,
        corrected_cache_efficiency: 0.0,
    }
}

fn seed_render_prompt_role(h: &mut Harness) {
    h.config.available_roles = path_std_collections::HashMap::from([(
        "debug-role".to_owned(),
        tau_config::settings::AgentRole {
            prompt_fragments: vec![tau_config::settings::RolePromptFragment {
                name: "debug.instructions".to_owned(),
                priority: tau_proto::PromptPriority::new(100),
                text: tau_proto::PromptContent::new("DEBUG ROLE PROMPT"),
            }],
            ..Default::default()
        },
    )]);
}

fn request_rendered_prompt(
    h: &mut Harness,
    role: &str,
    enable_agents_md: bool,
) -> tau_proto::RenderedPromptResult {
    let frames = connect_test_client(h, "render-prompt-test", tau_proto::ClientKind::Ui);
    h.send_rendered_prompt_result(
        &crate::test_connection_id("render-prompt-test"),
        tau_proto::GetRenderedPrompt {
            request_id: "request-1".to_owned(),
            role: Some(role.to_owned()),
            enable_agents_md,
        },
    );
    let frames = frames.lock().expect("frames lock");
    frames
        .iter()
        .find_map(|frame| match &frame.frame {
            HarnessOutputMessage::RenderedPromptResult(result) => Some((**result).clone()),
            _ => None,
        })
        .expect("rendered prompt result")
}

/// Builds a validated shell command id used by this test module.
fn test_shell_command_id(value: impl AsRef<str>) -> tau_proto::ShellCommandId {
    tau_proto::ShellCommandId::parse(value.as_ref())
        .expect("test identifier must satisfy its grammar")
}

mod agent_messaging;
mod agent_metadata;
mod agent_runtime;
mod cancellation_and_background;
mod compaction;
mod compaction_agent_scope;
mod compaction_failure_recovery;
mod compaction_generation_negative;
mod compaction_provider_watch;
mod compaction_reactive_rolling;
mod compaction_runtime_state;
mod compaction_standalone_rejections;
mod compaction_strict;
mod compaction_threshold;
mod configuration;
mod extension_routing;
mod internal_tool_dispatch;
mod loop_guard;
mod prompt_lifecycle;
mod provider_responses;
mod rendered_previews;
mod runtime_scheduling;
mod session_lifecycle;
mod status_reporting;
mod tool_execution;
mod ui_admission;

/// Rewrites one test agent journal to an exact complete-record crash prefix.
fn rewrite_agent_records(
    state: &std::path::Path,
    agent_id: &tau_proto::AgentId,
    records: &[tau_core::PersistedAgentEvent],
) {
    use std::io::Write as _;

    let path = state
        .join("agents")
        .join(agent_id.as_str())
        .join("events.cbor");
    let mut journal = path_std_fs::File::create(path).expect("rewrite crash prefix");
    for record in records {
        let mut encoded = Vec::new();
        ciborium::into_writer(record, &mut encoded).expect("encode record");
        journal
            .write_all(&(encoded.len() as u64).to_le_bytes())
            .expect("write record length");
        journal.write_all(&encoded).expect("write record");
    }
    journal.sync_all().expect("sync crash prefix");
}

/// Counts the decision terminal and its matching finish/start suffix.
fn automatic_policy_recovery_counts(
    records: &[tau_core::PersistedAgentEvent],
    transaction_id: &tau_proto::CompactionTransactionId,
) -> (usize, usize, usize, usize) {
    let terminals = records
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::ProviderResponseFinished(_) | Event::AgentPromptTerminated(_)
            )
        })
        .count();
    let finishes = records
        .iter()
        .filter(|record| matches!(&record.event, Event::AgentOuterTurnFinished(_)))
        .count();
    let starts = records
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::AgentStandaloneCompactionStarted(started)
                    if &started.transaction_id == transaction_id
            )
        })
        .count();
    let failures = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::AgentStandaloneCompactionFailed(failed)
                if &failed.transaction_id == transaction_id =>
            {
                Some(failed)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        failures.iter().all(|failed| {
            matches!(
                failed.reason,
                tau_proto::StandaloneCompactionFailureReason::Interrupted
                    | tau_proto::StandaloneCompactionFailureReason::PrefixTooLarge
            )
        }),
        "automatic recovery closed with an unexpected failure: {failures:?}"
    );
    (terminals, finishes, starts, failures.len())
}

/// Projects only lifecycle records whose repetition would violate quiescent
/// automatic-policy recovery; discovery snapshots are intentionally excluded.
fn automatic_policy_recovery_events(
    records: &[tau_core::PersistedAgentEvent],
) -> Vec<&tau_proto::Event> {
    records
        .iter()
        .filter_map(|record| {
            matches!(
                record.event,
                Event::ProviderResponseFinished(_)
                    | Event::AgentPromptTerminated(_)
                    | Event::AgentOuterTurnFinished(_)
                    | Event::AgentStandaloneCompactionStarted(_)
                    | Event::AgentStandaloneCompactionFailed(_)
            )
            .then_some(&record.event)
        })
        .collect()
}
