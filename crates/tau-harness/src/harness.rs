//! [`Harness`]: the central event loop. Owns the bus, registry, session
//! store, and the live extensions; routes every event between the agent,
//! tools, and clients.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::io;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rand::SeedableRng as _;
use rand::rngs::StdRng;
use tau_core::{
    ActionRegistry, AgentStore, Connection, ConnectionMetadata, ConnectionOrigin,
    DefaultSubscriptionPolicy, EventBus, NodeId, PolicyStore, RouteError, SessionStore,
    ToolRegistry, ToolRouteError, ToolRouteTarget, validate_tool_arguments,
};
use tau_proto::{
    ActionError, ActionInvocationId, ActionInvoke, ActionResult, ActionSchemaPublished, AgentId,
    AgentPromptCreated, AgentPromptId, AgentPromptQueued, AgentPromptRecalled,
    AgentPromptTerminated, AgentPromptTerminationReason, BackgroundSupport, CborValue, ClientKind,
    ConnectionId, ContentPart, ContextItem, ContextRole, Disconnect, Event, EventSelector,
    ExtensionName, HarnessAgentContextUsageChanged, HarnessContextUsageChanged,
    HarnessInputMessage, HarnessOutputMessage, HarnessRoleSelected, Hello, MessageItem, ModelId,
    PROTOCOL_VERSION, PromptFragment, PromptOriginator, ProviderModelInfo,
    ProviderResponseFinished, ProviderStopReason, ProviderTokenUsage, SecretValue, SessionId,
    ToolBackgroundError, ToolBackgroundResult, ToolCallId, ToolCallItem, ToolCancelled,
    ToolDefinition, ToolError, ToolName, ToolRegister, ToolRejected, ToolRequest, ToolResult,
    ToolResultKind, ToolType, UiCancelPrompt,
};

use crate::agent::{Agent, AgentTurnState, PendingCancel, PendingPrompt};
use crate::daemon::InteractionOutcome;
use crate::debug_log::DebugEventLog;
use crate::dedup::{
    DEFAULT_THRESHOLD_BYTES, build_pointer_error_message, build_pointer_value,
    encode_error_for_hash, encode_for_hash, hash_truncated,
};
use crate::dirs::policy_store_path_from;
use crate::discovery::{DiscoveredAgentsFile, DiscoveredSkill, DiscoveredSkillSource};
use crate::error::HarnessError;
use crate::event::{
    ChannelSink, HarnessCommand, HarnessEvent, WriterCommand, WriterShutdown, spawn_reader_thread,
    spawn_writer_thread,
};
use crate::event_log::EventLog;
#[cfg(any(test, feature = "echo-agent"))]
use crate::extension::spawn_in_process;
use crate::extension::{
    ExtensionConnectCommand, ExtensionEntry, ExtensionState, extension_stderr_log_path,
    spawn_supervised,
};
use crate::format::{format_tool_progress, render_entry_preview};
use crate::harness::agent_context::AgentContextStore;
use crate::harness::current_session::CurrentSessionState;
use crate::harness::extension_data::{
    ExtensionDataError, run_extension_data_append_file, run_extension_data_create_file,
    run_extension_data_delete_file, run_extension_data_list_files, run_extension_data_read_file,
    run_extension_data_rename_file, run_extension_data_write_file,
};
#[cfg(test)]
use crate::harness::extension_data::{
    append_extension_data_file, atomic_replace_extension_data_file, checked_extension_data_path,
    create_extension_data_file, delete_extension_data_file, list_extension_data_entries,
    rename_extension_data_file, sanitize_extension_data_path,
};
use crate::harness::extensions::{
    ExtensionActivationStage, ExtensionRuntimeState, StagedExtensionPublish,
};
use crate::harness::interception::{
    ConversationHeadSync, DeferredPublish, InterceptorRegistry, PendingIntercept,
};
use crate::harness::pending_notices::{PendingPromptNoticeState, PendingToolAvailabilityNotice};
use crate::harness::subagents_tool::SubagentToolState;
use crate::internal_tools::InternalToolHandlers;
use crate::model::{
    LoadedRoles, MissingDefaultRole, baseline_params_for_selection, context_percent_used,
    context_window_for_model, efforts_for_model, fallback_role, load_roles, model_for_role,
    role_infos, select_model_for_role, selected_params_for_role, thinking_summaries_for_model,
    verbosities_for_model,
};
use crate::prompt::{
    BUILT_IN_SYSTEM_TEMPLATE_NAME, RolePromptTemplateContext, ToolPromptFragment,
    assemble_prompt_context_from, build_system_prompt_with_tool_template_context,
    built_in_system_prompt_templates, render_agents_context_message,
    render_effective_prompt_message,
};
use crate::secrets::{load_secret_sources, resolve_extension_secrets};
use crate::settings::{Config, ExtensionStartupDiagnostic, load_harness_settings_or_warn};
use crate::tool_turn::{ForegroundAction, PendingToolInvocation, ToolTurnMachine};

const RENDERED_PROMPT_PREVIEW_AGENT_ID: &str = "dev-preview-agent";
use crate::turn::{PromptSubmission, TurnState};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(2);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(120);
const BUILT_IN_SKILLS_SOURCE_ID: &str = "harness:built-in-skills";
const SELF_KNOWLEDGE_VERSION_TOKEN: &str = "__TAU_SELF_KNOWLEDGE_VERSION__";
const SELF_KNOWLEDGE_HASH_TOKEN: &str = "__TAU_SELF_KNOWLEDGE_HASH__";
const SELF_KNOWLEDGE_BUILD_DATE_TOKEN: &str = "__TAU_SELF_KNOWLEDGE_BUILD_DATE__";
const SELF_KNOWLEDGE_CONFIG_SKILL_NAME: &str = "tau-self-knowledge-config";
const SELF_KNOWLEDGE_PIM_SKILL_NAME: &str = "tau-self-knowledge-ext-pim";
const SELF_KNOWLEDGE_HARNESS_CONFIG: &str =
    include_str!("../../tau-config/config/built-in.harness.yaml");
const SELF_KNOWLEDGE_UI_CONFIG: &str = include_str!("../../tau-config/config/built-in.cli.yaml");
const SELF_KNOWLEDGE_PIM_CONFIG: &str =
    include_str!("../../tau-ext-pim/config/self-knowledge.harness.yaml");

fn session_dir_status_from_reason(
    reason: tau_proto::SessionStartReason,
) -> tau_proto::SessionDirStatus {
    match reason {
        tau_proto::SessionStartReason::Initial | tau_proto::SessionStartReason::New => {
            tau_proto::SessionDirStatus::New
        }
        tau_proto::SessionStartReason::Resume => tau_proto::SessionDirStatus::Resumed,
    }
}

pub(crate) fn background_completion_prompt(call_id: &ToolCallId) -> String {
    format!(
        "{} Tool call `{call_id}` is complete.",
        crate::INTERNAL_MARKER
    )
}

const RESTORE_NOTICE_BODY_PREFIX: &str = "Previous session was interrupted and restored.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AgentState {
    Active,
    ActiveDelegated,
    Suspended,
}

fn agent_runtime_state_for_turn(state: &AgentTurnState) -> tau_proto::AgentRuntimeState {
    match state {
        AgentTurnState::Idle => tau_proto::AgentRuntimeState::Idle,
        AgentTurnState::AgentThinking { .. } | AgentTurnState::ToolsRunning { .. } => {
            tau_proto::AgentRuntimeState::Running
        }
    }
}

/// Text for the one-shot model-visible notice folded into the first user turn
/// after a cold session resume.
pub(crate) fn restore_notice_prompt(
    last_recorded_at: Option<tau_proto::UnixMicros>,
    now: tau_proto::UnixMicros,
) -> String {
    restore_notice_prompt_for_elapsed_inner(restore_notice_elapsed(last_recorded_at, now))
}

/// Test helper that formats the restore notice for a fixed elapsed duration.
#[cfg(test)]
pub(crate) fn restore_notice_prompt_for_elapsed(elapsed: Option<Duration>) -> String {
    restore_notice_prompt_for_elapsed_inner(elapsed)
}

fn restore_notice_prompt_for_elapsed_inner(elapsed: Option<Duration>) -> String {
    let timing = elapsed.map_or_else(
        || "The state of the world might have changed since the last session.".to_owned(),
        |elapsed| {
            format!(
                "{} since the last recorded session event, and the state of the world might have changed.",
                format_restore_notice_elapsed(elapsed)
            )
        },
    );
    format!(
        "{} {RESTORE_NOTICE_BODY_PREFIX} {timing}",
        crate::INTERNAL_MARKER
    )
}

fn restore_notice_elapsed(
    last_recorded_at: Option<tau_proto::UnixMicros>,
    now: tau_proto::UnixMicros,
) -> Option<Duration> {
    let last = last_recorded_at?;
    if last.get() == 0 || now.get() < last.get() {
        return None;
    }
    Some(Duration::from_micros(now.get() - last.get()))
}

fn format_restore_notice_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        return "Less than 1 minute has passed".to_owned();
    }

    let minutes = seconds / 60;
    if minutes < 60 {
        return format_elapsed_count(minutes, "minute");
    }

    let hours = minutes / 60;
    if hours < 24 {
        return format_elapsed_count(hours, "hour");
    }

    format_elapsed_count(hours / 24, "day")
}

fn format_elapsed_count(count: u64, unit: &str) -> String {
    let suffix = if count == 1 { "" } else { "s" };
    let verb = if count == 1 { "has" } else { "have" };
    format!("{count} {unit}{suffix} {verb} passed")
}

/// Returns true when `text` is the hidden one-shot restore notice.
pub(crate) fn is_restore_notice_prompt_text(text: &str) -> bool {
    text.strip_prefix(crate::INTERNAL_MARKER)
        .and_then(|text| text.strip_prefix(" "))
        .is_some_and(|text| text.starts_with(RESTORE_NOTICE_BODY_PREFIX))
}

fn event_is_internal_prompt_text(event: &Event, text: &str) -> bool {
    match event {
        Event::AgentPromptSubmitted(prompt) => {
            prompt.message_class.is_internal() && prompt.text == text
        }
        Event::AgentPromptSteered(steered) => {
            steered.message_class.is_internal() && steered.text == text
        }
        Event::AgentUserMessageInjected(injected) => {
            injected.message_class.is_internal() && injected.text == text
        }
        _ => false,
    }
}

fn event_is_internal_restore_notice(event: &Event) -> bool {
    match event {
        Event::AgentPromptSubmitted(prompt) => {
            prompt.message_class.is_internal() && is_restore_notice_prompt_text(&prompt.text)
        }
        Event::AgentPromptSteered(steered) => {
            steered.message_class.is_internal() && is_restore_notice_prompt_text(&steered.text)
        }
        Event::AgentUserMessageInjected(injected) => {
            injected.message_class.is_internal() && is_restore_notice_prompt_text(&injected.text)
        }
        _ => false,
    }
}

/// Estimate how many prompt/input tokens a compacted replay window will occupy
/// when replayed on the next turn.
///
/// Tau does not carry a tokenizer in the harness, and providers do not always
/// report usage for compaction items. For UI status we use the same coarse
/// convention used by many provider dashboards: roughly four UTF-8 bytes per
/// token, measured over the provider-owned items that prompt assembly will
/// replay after compaction. This is not a billing counter.
fn estimate_compacted_input_tokens(replay_window: &[ContextItem]) -> Option<u64> {
    const APPROX_BYTES_PER_TOKEN: u64 = 4;

    let bytes: u64 = replay_window
        .iter()
        .map(approx_context_item_provider_bytes)
        .sum();
    (0 < bytes).then_some(bytes.div_ceil(APPROX_BYTES_PER_TOKEN).max(1))
}

fn approx_context_item_provider_bytes(item: &ContextItem) -> u64 {
    match item {
        ContextItem::Message(message) => {
            let content_bytes: u64 = message
                .content
                .iter()
                .map(|part| match part {
                    ContentPart::Text { text } => text.len() as u64,
                })
                .sum();
            content_bytes + 16
        }
        ContextItem::ToolCall(call) => {
            call.call_id.as_str().len() as u64
                + call.name.as_str().len() as u64
                + approx_cbor_json_bytes(&call.arguments)
                + 16
        }
        ContextItem::ToolResult(result) => {
            let status_bytes = match &result.status {
                tau_proto::ToolResultStatus::Success => 0,
                tau_proto::ToolResultStatus::Error { message }
                | tau_proto::ToolResultStatus::Cancelled { reason: message } => {
                    message.len() as u64
                }
            };
            result.call_id.as_str().len() as u64
                + status_bytes
                + result.output.render().len() as u64
                + 16
        }
        ContextItem::ReasoningText(reasoning) => reasoning.text.len() as u64 + 16,
        ContextItem::Reasoning(item)
        | ContextItem::Compaction(item)
        | ContextItem::UnknownProviderItem(item) => approx_cbor_json_bytes(&item.0),
        ContextItem::CompactionTrigger => 16,
    }
}

fn latest_compaction_replay_window(items: &[ContextItem]) -> Option<&[ContextItem]> {
    items
        .iter()
        .rposition(|item| matches!(item, ContextItem::Compaction(_)))
        .map(|index| &items[index..])
}

fn approx_cbor_json_bytes(value: &CborValue) -> u64 {
    match value {
        CborValue::Null => 4,
        CborValue::Bool(value) => {
            if *value {
                4
            } else {
                5
            }
        }
        CborValue::Integer(value) => {
            let value: i128 = (*value).into();
            value.to_string().len() as u64
        }
        CborValue::Float(value) => value.to_string().len() as u64,
        CborValue::Bytes(bytes) => (bytes.len() as u64).div_ceil(3) * 4,
        CborValue::Text(text) => text.len() as u64,
        CborValue::Array(values) => {
            2 + values.iter().map(approx_cbor_json_bytes).sum::<u64>()
                + values.len().saturating_sub(1) as u64
        }
        CborValue::Map(entries) => {
            2 + entries
                .iter()
                .map(|(key, value)| approx_cbor_json_bytes(key) + approx_cbor_json_bytes(value) + 3)
                .sum::<u64>()
                + entries.len().saturating_sub(1) as u64
        }
        CborValue::Tag(_, value) => approx_cbor_json_bytes(value),
        _ => 0,
    }
}

fn restored_tool_call_error_message(call_id: &ToolCallId) -> String {
    format!(
        "{}: true\n\nTool call `{call_id}` was interrupted due to session restart. Side effects may have occurred.",
        tau_proto::TAU_INTERNAL_HEADER_NAME
    )
}

fn restored_background_tool_call_error_message(call_id: &ToolCallId) -> String {
    format!(
        "{}: true\n\nBackground tool call `{call_id}` was interrupted due to session restart. Side effects may have occurred.",
        tau_proto::TAU_INTERNAL_HEADER_NAME
    )
}

fn extension_disconnected_tool_call_error_message(call_id: &ToolCallId) -> String {
    format!(
        "{}: true\n\nTool call `{call_id}` was interrupted because extension disconnected. Side effects may have occurred.",
        tau_proto::TAU_INTERNAL_HEADER_NAME
    )
}

fn extension_disconnected_background_tool_call_error_message(call_id: &ToolCallId) -> String {
    format!(
        "{}: true\n\nBackground tool call `{call_id}` was interrupted because extension disconnected. Side effects may have occurred.",
        tau_proto::TAU_INTERNAL_HEADER_NAME
    )
}

/// Model-visible internal tool error for calls whose provider is no longer
/// live.
pub(crate) fn unavailable_tool_error_message(tool_name: &ToolName) -> String {
    format!(
        "{}: true\n\nTool `{tool_name}` is not available.",
        tau_proto::TAU_INTERNAL_HEADER_NAME
    )
}

/// Hidden prompt text used to tell the model a tool left the live registry.
pub(crate) fn tool_unavailable_notice_prompt(tool_name: &ToolName) -> String {
    format!(
        "{} Tool `{tool_name}` is temporarily no longer available.",
        crate::INTERNAL_MARKER
    )
}

/// Hidden prompt text used to tell the model a previously missing tool
/// returned.
pub(crate) fn tool_available_again_notice_prompt(tool_name: &ToolName) -> String {
    format!(
        "{} Tool `{tool_name}` is available again.",
        crate::INTERNAL_MARKER
    )
}

fn remove_pending_internal_prompt_text(prompts: &mut VecDeque<PendingPrompt>, text: &str) -> bool {
    let before = prompts.len();
    prompts.retain(|prompt| !(prompt.is_internal() && prompt.text == text));
    prompts.len() != before
}

fn load_system_prompt_templates(config_dir: Option<&Path>) -> HashMap<String, String> {
    let mut templates = built_in_system_prompt_templates();
    let Some(config_dir) = config_dir else {
        return templates;
    };
    let prompts_dir = config_dir.join("prompts");
    let Ok(entries) = std::fs::read_dir(prompts_dir) else {
        return templates;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("hbs") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                templates.insert(name.to_owned(), content);
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), error = %error, "failed to read prompt template");
            }
        }
    }
    templates
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PromptFragmentSource {
    RoleConfig {
        role_name: String,
    },
    Extension {
        connection_id: tau_proto::ConnectionId,
    },
    Tool {
        connection_id: tau_proto::ConnectionId,
    },
}

impl PromptFragmentSource {
    fn sort_key(&self) -> (&str, u8) {
        match self {
            // Role-config fragments have no extension connection id. Keep them
            // deterministic without pretending they came from a magic string
            // connection.
            Self::RoleConfig { role_name } => (role_name.as_str(), 0),
            Self::Extension { connection_id } => (connection_id.as_str(), 1),
            Self::Tool { connection_id } => (connection_id.as_str(), 2),
        }
    }
}

#[derive(Clone, Debug)]
struct SourcedPromptFragment {
    source: PromptFragmentSource,
    fragment: PromptFragment,
}

fn sort_sourced_prompt_fragments(fragments: &mut [SourcedPromptFragment]) {
    fragments.sort_by(|a, b| {
        a.fragment
            .priority
            .cmp(&b.fragment.priority)
            .then_with(|| a.source.sort_key().cmp(&b.source.sort_key()))
            .then_with(|| a.fragment.name.cmp(&b.fragment.name))
    });
}

fn sorted_prompt_fragments(
    fragments: impl IntoIterator<Item = SourcedPromptFragment>,
) -> Vec<PromptFragment> {
    let mut fragments = fragments.into_iter().collect::<Vec<_>>();
    sort_sourced_prompt_fragments(&mut fragments);
    fragments
        .into_iter()
        .map(|sourced| sourced.fragment)
        .collect()
}

#[derive(Clone, Debug)]
struct SourcedToolPromptFragment {
    source: PromptFragmentSource,
    tool_name: tau_proto::ToolName,
    fragment: PromptFragment,
}

fn sorted_tool_prompt_fragments(
    fragments: impl IntoIterator<Item = SourcedToolPromptFragment>,
) -> Vec<ToolPromptFragment> {
    let mut fragments = fragments.into_iter().collect::<Vec<_>>();
    fragments.sort_by(|a, b| {
        a.fragment
            .priority
            .cmp(&b.fragment.priority)
            .then_with(|| a.source.sort_key().cmp(&b.source.sort_key()))
            .then_with(|| a.fragment.name.cmp(&b.fragment.name))
    });
    fragments
        .into_iter()
        .map(|sourced| ToolPromptFragment {
            tool_name: sourced.tool_name,
            fragment: sourced.fragment,
        })
        .collect()
}

#[derive(Clone, Debug)]
pub struct AgentToolCall {
    /// Provider-supplied tool call id.
    pub id: ToolCallId,
    /// Internal tool name selected by routing.
    pub name: ToolName,
    /// Protocol tool type.
    pub tool_type: tau_proto::ToolType,
    /// CBOR arguments supplied by the model/provider.
    pub arguments: CborValue,
}

#[derive(Clone, Debug)]
pub(crate) struct PendingTool {
    pub(crate) name: ToolName,
    pub(crate) internal_name: ToolName,
    pub(crate) tool_type: ToolType,
}

fn tags_match_any(
    tags: &[tau_proto::ToolTag],
    patterns: &[tau_config::settings::ToolTagPattern],
) -> bool {
    tags.iter()
        .any(|tag| patterns.iter().any(|pattern| pattern.matches(tag)))
}

const DEFAULT_AGENT_ID_TEMPLATE: &str = "{{random_alphanumeric 6}}";
const AGENT_ID_TEMPLATE_COLLISION_ATTEMPTS: usize = 10;

fn normalize_display_name(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
fn deterministic_agent_id_rng() -> StdRng {
    StdRng::seed_from_u64(0)
}

fn random_alphanumeric(len: usize, rng: &mut StdRng) -> String {
    use rand::Rng as _;

    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    (0..len)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect()
}

struct RandomAlphanumericHelper<'a> {
    collision_extra_len: usize,
    rng: Mutex<&'a mut StdRng>,
}

impl handlebars::HelperDef for RandomAlphanumericHelper<'_> {
    fn call_inner<'reg: 'rc, 'rc>(
        &self,
        h: &handlebars::Helper<'rc>,
        _: &'reg handlebars::Handlebars<'reg>,
        _: &'rc handlebars::Context,
        _: &mut handlebars::RenderContext<'reg, 'rc>,
    ) -> Result<handlebars::ScopedJson<'rc>, handlebars::RenderError> {
        let requested = h
            .param(0)
            .and_then(|param| param.value().as_u64())
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(6);
        let mut rng = self.rng.lock().expect("agent id rng lock poisoned");
        Ok(handlebars::ScopedJson::Derived(serde_json::Value::String(
            random_alphanumeric(requested.saturating_add(self.collision_extra_len), &mut rng),
        )))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AgentIdTemplateKind {
    Configured,
    Default,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum AgentIdMintWarning {
    RenderFailed { error: String },
    InvalidRendered { candidate: String, error: String },
    CollisionsExceeded { attempts: usize },
}

fn handlebars_for_agent_template<'a>(
    collision_extra_len: usize,
    rng: &'a mut StdRng,
) -> handlebars::Handlebars<'a> {
    let mut handlebars = handlebars::Handlebars::new();
    handlebars.set_strict_mode(true);
    handlebars.register_escape_fn(handlebars::no_escape);
    handlebars.register_helper(
        "random_alphanumeric",
        Box::new(RandomAlphanumericHelper {
            collision_extra_len,
            rng: Mutex::new(rng),
        }),
    );
    handlebars
}

fn base_agent_template_context(
    role: &str,
    role_group: &str,
) -> serde_json::Map<String, serde_json::Value> {
    let mut context = serde_json::Map::new();
    context.insert(
        "role".to_owned(),
        serde_json::Value::String(role.to_owned()),
    );
    context.insert(
        "role_group".to_owned(),
        serde_json::Value::String(role_group.to_owned()),
    );
    context.insert(
        "roleGroup".to_owned(),
        serde_json::Value::String(role_group.to_owned()),
    );
    context
}

fn render_agent_template(
    template: &str,
    role: &str,
    role_group: &str,
    agent_id: &str,
    task_name: Option<&str>,
    collision_extra_len: usize,
    rng: &mut StdRng,
) -> Result<String, handlebars::RenderError> {
    let handlebars = handlebars_for_agent_template(collision_extra_len, rng);
    let mut context = base_agent_template_context(role, role_group);
    context.insert(
        "agent_id".to_owned(),
        serde_json::Value::String(agent_id.to_owned()),
    );
    context.insert(
        "agentId".to_owned(),
        serde_json::Value::String(agent_id.to_owned()),
    );
    context.insert(
        "task_name".to_owned(),
        serde_json::Value::String(task_name.unwrap_or("").to_owned()),
    );
    context.insert(
        "taskName".to_owned(),
        serde_json::Value::String(task_name.unwrap_or("").to_owned()),
    );
    context.insert(
        "task_name_present".to_owned(),
        serde_json::Value::Bool(task_name.is_some()),
    );
    context.insert(
        "taskNamePresent".to_owned(),
        serde_json::Value::Bool(task_name.is_some()),
    );
    handlebars.render_template(template, &serde_json::Value::Object(context))
}

fn render_agent_id_template(
    template: &str,
    role: &str,
    role_group: &str,
    collision_extra_len: usize,
    rng: &mut StdRng,
) -> Result<String, handlebars::RenderError> {
    let handlebars = handlebars_for_agent_template(collision_extra_len, rng);
    handlebars.render_template(
        template,
        &serde_json::Value::Object(base_agent_template_context(role, role_group)),
    )
}

fn mint_available_agent_id_for_role_with(
    role: &str,
    role_group: &str,
    template: &str,
    mut is_taken: impl FnMut(&str) -> bool,
    rng: &mut StdRng,
    mut warn: impl FnMut(AgentIdTemplateKind, AgentIdMintWarning),
) -> String {
    let mut use_default = false;
    loop {
        let active_template = if use_default {
            DEFAULT_AGENT_ID_TEMPLATE
        } else {
            template
        };
        let kind = if use_default {
            AgentIdTemplateKind::Default
        } else {
            AgentIdTemplateKind::Configured
        };
        let max_attempts = if use_default {
            tau_proto::AGENT_ID_MAX_LEN
        } else {
            AGENT_ID_TEMPLATE_COLLISION_ATTEMPTS
        };
        let mut exhausted_attempts = true;
        for attempt in 0..max_attempts {
            let rendered =
                match render_agent_id_template(active_template, role, role_group, attempt, rng) {
                    Ok(rendered) => rendered,
                    Err(error) => {
                        warn(
                            kind,
                            AgentIdMintWarning::RenderFailed {
                                error: error.to_string(),
                            },
                        );
                        exhausted_attempts = false;
                        break;
                    }
                };
            let agent_id = match rendered.parse::<AgentId>() {
                Ok(agent_id) => agent_id,
                Err(error) => {
                    warn(
                        kind,
                        AgentIdMintWarning::InvalidRendered {
                            candidate: rendered,
                            error: error.to_string(),
                        },
                    );
                    exhausted_attempts = false;
                    break;
                }
            };
            if !is_taken(agent_id.as_str()) {
                return agent_id.into_string();
            }
        }
        if use_default {
            panic!("unable to mint unique agent id with default template");
        }
        if exhausted_attempts {
            warn(
                AgentIdTemplateKind::Configured,
                AgentIdMintWarning::CollisionsExceeded {
                    attempts: AGENT_ID_TEMPLATE_COLLISION_ATTEMPTS,
                },
            );
        }
        use_default = true;
    }
}

#[cfg(test)]
fn mint_agent_id_for_role(role: &str) -> String {
    let mut rng = deterministic_agent_id_rng();
    mint_available_agent_id_for_role_with(
        role,
        role,
        DEFAULT_AGENT_ID_TEMPLATE,
        |_| false,
        &mut rng,
        |_, _| {},
    )
}

fn built_in_discovered_skills() -> HashMap<tau_proto::SkillName, DiscoveredSkill> {
    let modified = built_in_skill_modified_time();
    tau_skills::built_in_skills()
        .into_iter()
        .map(|skill| {
            let content = render_built_in_self_knowledge_content(&skill.name, skill.content);
            (
                tau_proto::SkillName::from(skill.name),
                DiscoveredSkill {
                    source_id: BUILT_IN_SKILLS_SOURCE_ID.into(),
                    description: skill.description,
                    source: DiscoveredSkillSource::BuiltIn { content },
                    add_to_prompt: skill.add_to_prompt,
                    user_invocable: skill.user_invocable,
                    disable_model_invocation: skill.disable_model_invocation,
                    modified,
                },
            )
        })
        .collect()
}

fn normalize_skill_invocation_policy(skill: &mut tau_proto::ExtSkillAvailable) {
    if skill.disable_model_invocation {
        skill.user_invocable = true;
    }
}

fn built_in_skill_modified_time() -> Option<SystemTime> {
    crate::version::build_last_modified()
        .as_deref()
        .and_then(parse_build_last_modified)
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|path| skill_file_modified_time(&path))
        })
}

fn parse_build_last_modified(value: &str) -> Option<SystemTime> {
    let bytes = value.as_bytes();
    if bytes.len() != "YYYY-MM-DD HH:MM".len() {
        return None;
    }
    let year = parse_ascii_i64(value.get(0..4)?)?;
    let month = parse_ascii_i64(value.get(5..7)?)?;
    let day = parse_ascii_i64(value.get(8..10)?)?;
    let hour = parse_ascii_u64(value.get(11..13)?)?;
    let minute = parse_ascii_u64(value.get(14..16)?)?;
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b' '
        || bytes[13] != b':'
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || 24 <= hour
        || 60 <= minute
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    if days < 0 {
        return None;
    }
    let seconds = (days as u64)
        .saturating_mul(24 * 60 * 60)
        .saturating_add(hour.saturating_mul(60 * 60))
        .saturating_add(minute.saturating_mul(60));
    Some(UNIX_EPOCH + Duration::from_secs(seconds))
}

fn parse_ascii_i64(value: &str) -> Option<i64> {
    value.parse().ok()
}

fn parse_ascii_u64(value: &str) -> Option<u64> {
    value.parse().ok()
}

// Howard Hinnant's civil-calendar conversion: returns days since Unix epoch
// for a proleptic Gregorian date without pulling in a time dependency here.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if 0 <= year { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = month + if 2 < month { -3 } else { 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn skill_file_modified_time(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

fn compare_skill_modified(a: Option<SystemTime>, b: Option<SystemTime>) -> std::cmp::Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => std::cmp::Ordering::Greater,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

fn selected_skill_candidate(candidates: &[DiscoveredSkill]) -> Option<&DiscoveredSkill> {
    let mut selected = candidates.first()?;
    for candidate in &candidates[1..] {
        if compare_skill_modified(selected.modified, candidate.modified).is_lt() {
            selected = candidate;
        }
    }
    Some(selected)
}

fn render_built_in_self_knowledge_content(
    skill_name: &str,
    content: std::borrow::Cow<'static, str>,
) -> std::borrow::Cow<'static, str> {
    match skill_name {
        SELF_KNOWLEDGE_CONFIG_SKILL_NAME => render_self_knowledge_config_content(),
        SELF_KNOWLEDGE_PIM_SKILL_NAME => render_self_knowledge_pim_content(),
        _ => render_self_knowledge_content(content),
    }
}

fn render_self_knowledge_config_content() -> std::borrow::Cow<'static, str> {
    std::borrow::Cow::Owned(format!(
        include_str!("../../tau-skills/self-knowledge/tau-self-knowledge-config.md"),
        XDG_RUNTIME_DIR = "{XDG_RUNTIME_DIR}",
        harness_config = SELF_KNOWLEDGE_HARNESS_CONFIG,
        ui_config = SELF_KNOWLEDGE_UI_CONFIG,
    ))
}

fn render_self_knowledge_pim_content() -> std::borrow::Cow<'static, str> {
    std::borrow::Cow::Owned(format!(
        include_str!("../../tau-skills/self-knowledge/tau-self-knowledge-ext-pim.md"),
        pim_config = SELF_KNOWLEDGE_PIM_CONFIG,
    ))
}

fn render_self_knowledge_content(
    content: std::borrow::Cow<'static, str>,
) -> std::borrow::Cow<'static, str> {
    let last_modified = crate::version::build_last_modified().unwrap_or_else(|| "unknown".into());
    std::borrow::Cow::Owned(
        content
            .replace(SELF_KNOWLEDGE_VERSION_TOKEN, env!("CARGO_PKG_VERSION"))
            .replace(SELF_KNOWLEDGE_HASH_TOKEN, &crate::version::build_revision())
            .replace(SELF_KNOWLEDGE_BUILD_DATE_TOKEN, &last_modified),
    )
}

pub(crate) fn assistant_text_from_output_items(output_items: &[ContextItem]) -> Option<String> {
    let text = output_items
        .iter()
        .filter_map(|item| match item {
            ContextItem::Message(MessageItem {
                role: ContextRole::Assistant,
                content,
                ..
            }) => Some(
                content
                    .iter()
                    .map(|part| match part {
                        ContentPart::Text { text } => text.as_str(),
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect::<String>();
    (!text.is_empty()).then_some(text)
}

pub(crate) fn tool_calls_from_output_items(output_items: &[ContextItem]) -> Vec<AgentToolCall> {
    output_items
        .iter()
        .filter_map(|item| match item {
            ContextItem::ToolCall(call) => Some(AgentToolCall {
                id: call.call_id.clone(),
                name: call.name.clone(),
                tool_type: call.tool_type,
                arguments: call.arguments.clone(),
            }),
            _ => None,
        })
        .collect()
}

fn unique_synthetic_tool_call_id(
    reserved_tool_call_ids: &mut HashSet<ToolCallId>,
    prompt_id: &AgentPromptId,
    index: usize,
) -> ToolCallId {
    let mut suffix = index + 1;
    loop {
        let candidate: ToolCallId = format!("invalid_tool_call_{}_{}", prompt_id, suffix).into();
        if reserved_tool_call_ids.insert(candidate.clone()) {
            return candidate;
        }
        suffix += 1;
    }
}

fn response_requests_tool_calls(response: &ProviderResponseFinished) -> bool {
    if response.stop_reason.requests_tool_calls() {
        return true;
    }
    if response.stop_reason != ProviderStopReason::EndTurn {
        return false;
    }
    response
        .output_items
        .iter()
        .any(|item| matches!(item, ContextItem::ToolCall(_)))
}
fn validate_protocol_version(hello: &Hello) -> Result<(), HarnessError> {
    if hello.protocol_version == PROTOCOL_VERSION {
        return Ok(());
    }
    Err(HarnessError::Participant(format!(
        "unsupported protocol version from {}: got {}, expected {}",
        hello.client_name, hello.protocol_version, PROTOCOL_VERSION
    )))
}

#[cfg(test)]
mod agent_context_tests;
#[cfg(test)]
mod compaction_metadata_tests;
#[cfg(test)]
mod delegate_display_tests;
#[cfg(test)]
mod semantic_event_router_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tool_policy_tests;

mod agent_context;
mod current_session;
mod dispatch;
mod extension_data;
mod extensions;
mod interception;
mod pending_notices;
mod replay;
mod semantic_event_router;
mod subagents_tool;
mod user_skill_invocation;

/// Connection ID used for harness-owned tools and their side-query
/// [`PromptOriginator`] name (e.g. `skill`, `agent_start`, and `wait`).
pub(crate) const HARNESS_CONNECTION_ID: &str = "__harness__";

#[derive(Debug)]
struct PendingStartAgentRequest {
    source_id: String,
    extension_name: String,
    query: tau_proto::StartAgentRequest,
    role: String,
    cid: AgentId,
    parent_cid: Option<AgentId>,
    agent_id: String,
    pending_agent_messages: VecDeque<PendingPrompt>,
}

#[derive(Clone, Debug)]
struct PendingActionInvocation {
    provider_connection_id: tau_proto::ConnectionId,
    requester_client_id: tau_proto::ConnectionId,
    action_id: String,
}

/// Message recipient state used to report precise tool errors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AgentMessageRecipientStatus {
    /// The recipient is known and can receive messages now.
    Live,
    /// The recipient id was known earlier but its agent has stopped.
    Stopped,
    /// The recipient id has never been observed by this harness.
    Unknown,
}

/// Initial UI transport owned by the harness process during startup.
pub(crate) enum InitialClient {
    Stdio,
}

/// Output path used before an initial UI has been accepted by the normal bus.
pub(crate) enum InitialClientStartupErrorOutput {
    #[cfg(test)]
    Stream(UnixStream),
    Stdout,
}

/// Central harness event loop and runtime state.
///
/// `Harness` owns the event bus, live connections, durable session and agent
/// stores, provider/tool routing state, and the currently bound session. Most
/// fields remain crate-visible so focused harness submodules and regression
/// tests can share the state while the implementation is gradually split into
/// smaller owners.
pub struct Harness {
    /// Sender side of the harness's central event channel. Cloned into
    /// each per-connection reader thread so they can feed
    /// `HarnessEvent`s back into the main loop.
    pub(crate) tx: Sender<HarnessEvent>,
    /// Receiver side of the central event channel. The main loop
    /// blocks on this and dispatches one `HarnessEvent` at a time.
    pub(crate) rx: Receiver<HarnessEvent>,
    /// Routes protocol events between connections (agent ↔ extensions
    /// ↔ socket clients). Owns connection state and per-connection
    /// outgoing queues.
    pub(crate) bus: EventBus,
    /// Maps tool name → providing connection. Used to resolve
    /// `ToolRequest` into either a broadcast `ToolStarted`
    /// or a broadcast `ToolRejected`.
    pub(crate) registry: ToolRegistry,
    /// Maps extension-provided UI actions to their owning extension connection.
    pub(crate) action_registry: ActionRegistry,
    /// Injected handlers for tools implemented inside the harness process.
    pub(crate) internal_tool_handlers: InternalToolHandlers,
    /// Runtime state root for this harness. Extension-specific persistent
    /// directories are allocated below this path and sent in Configure.
    pub(crate) state_dir: PathBuf,
    /// Append-only on-disk session membership store. Owns the folded
    /// loaded-agent set for each session id, derived from the durable
    /// membership journal at `<state_dir>/sessions/<session_id>/events.cbor`.
    pub(crate) store: SessionStore,
    /// Append-only global per-agent transcript store under `<state>/agents`.
    pub(crate) agent_store: AgentStore,
    /// The single session this harness owns. UserMessages with a
    /// different `session_id` are rejected. Pi-style: one harness =
    /// one active session at a time. Switching sessions tears the
    /// harness down and respawns extensions; that's a future
    /// `switch_session` operation, not silent multi-session.
    pub(crate) current_session_id: SessionId,
    /// Reason associated with the current session binding. Late UI subscribers
    /// receive a replayed `SessionStarted` snapshot with this reason.
    pub(crate) current_session_start_reason: tau_proto::SessionStartReason,
    /// Random stream for agent-id template helpers. Production harnesses seed
    /// it from OS entropy; tests can replace it with a deterministic stream
    /// to stabilize generated agent ids. Advanced on each agent creation so
    /// one harness does not mint the same random candidate repeatedly.
    agent_id_rng: StdRng,
    /// `call_id` → owning agent for every tool call currently
    /// in flight. Used to attribute incoming `ToolResult` / `ToolError`
    /// / `ToolProgress` events back to the originating conversation.
    pub(crate) tool_agents: std::collections::HashMap<ToolCallId, AgentId>,
    /// `call_id` → pending tool metadata for in-flight calls. Used to
    /// enrich terminal runtime events before they are folded into
    /// durable transcript facts.
    pub(crate) pending_tools: std::collections::HashMap<ToolCallId, PendingTool>,
    /// Tool call ids that were known to this harness and reached a terminal
    /// state. Used to distinguish completed calls from typos in user-facing
    /// cancellation errors.
    pub(crate) completed_tool_calls: std::collections::HashSet<ToolCallId>,
    /// `call_id` → connection id of the extension currently servicing
    /// the call. Needed to route cancellation requests back to the
    /// right provider.
    pub(crate) pending_tool_providers:
        std::collections::HashMap<ToolCallId, tau_proto::ConnectionId>,
    /// `invocation_id` → action provider/requester pair for UI-directed
    /// action result routing and source validation.
    pending_action_invocations: HashMap<ActionInvocationId, PendingActionInvocation>,
    /// Runtime event sequencer. Replay for reconnecting clients is rebuilt from
    /// semantic state instead of retained event payloads.
    pub(crate) event_log: std::sync::Arc<EventLog>,
    /// Writer channels for socket clients, keyed by connection ID.
    /// Used to start follower threads for log-based replay + delivery.
    pub(crate) client_writers:
        std::collections::HashMap<tau_proto::ConnectionId, Sender<WriterCommand>>,
    /// A UI sent `/detach` while the harness was still in startup gating.
    /// The main event loop consumes this to preserve detach semantics after
    /// startup completes.
    startup_detach_requested: bool,
    /// Buffered human-readable lifecycle messages (extension init,
    /// model changes, etc.) surfaced to the UI as part of the next
    /// `InteractionOutcome`.
    pub(crate) lifecycle_messages: Vec<String>,
    /// Mandatory harness diagnostics that must be replayed to late UI clients.
    ///
    /// Extension config errors commonly happen during daemon startup, before
    /// the terminal UI has subscribed. Keep these messages as explicit
    /// current harness state instead of relying on the append-only event
    /// log: a config parse failure must never be visible only in stderr or
    /// historical debug logs.
    pub(crate) replayable_harness_notices: Vec<tau_proto::HarnessNotice>,
    /// Extension process lifecycle and pre-`Ready` activation state.
    pub(crate) extensions: ExtensionRuntimeState,
    /// Maps agent_prompt_id → owning agent for in-flight
    /// prompts. The conversation knows its `session_id`, so older
    /// `prompt_sessions[spid]` lookups become two hops:
    /// `prompt_agents[spid]` → `agents[cid].session_id`.
    pub(crate) prompt_agents: std::collections::HashMap<AgentPromptId, AgentId>,
    /// All in-flight agents keyed by durable `AgentId`. User agents and side
    /// agents use the same identity; there is no default/main alias.
    pub(crate) agents: std::collections::HashMap<AgentId, Agent>,
    /// Agent id to conversation routing for addressable agents in the current
    /// session. Suspended agents remain here so `/agent resume` and follow-up
    /// prompts can continue their conversation.
    pub(crate) agent_routes: HashMap<String, AgentId>,
    /// Agent ids already loaded, or with a must-pass membership publish queued,
    /// for the current session. This closes the race where interception parks
    /// `session.agent_loaded` before the durable session store can fold it.
    pub(crate) session_loaded_agents: HashSet<AgentId>,
    /// Harness-owned lifecycle state for current-session agents.
    pub(crate) agent_states: HashMap<String, AgentState>,
    /// Agent ids that were once known but can no longer receive messages.
    pub(crate) stopped_agent_ids: HashSet<String>,
    /// Global harness state. Currently only tracks per-session init
    /// (waiting on extensions to announce skills + AGENTS.md). Agent
    /// turn state is per-agent; multiple agents may have
    /// in-flight prompts simultaneously and the agent extension
    /// serializes its own consumption of `AgentPromptCreated`.
    pub(crate) turn_state: TurnState,
    /// Append-only event debug log.
    pub(crate) debug_log: Option<DebugEventLog>,
    /// Event emission interceptors, exact name first and prefix fallback.
    pub(crate) interceptors: InterceptorRegistry,
    /// Currently in-flight interception. While `Some(_)`, no new
    /// publishes commit — they queue onto `deferred_publishes` until
    /// the awaited [`InterceptReply`] arrives (or the awaited
    /// connection disconnects, treated as `Pass(None)`).
    pub(crate) pending_intercept: Option<PendingIntercept>,
    /// Publishes that arrived while `pending_intercept` was active.
    /// Drained in FIFO order once the pending intercept resolves.
    pub(crate) deferred_publishes: VecDeque<DeferredPublish>,
    /// Conversations whose just-published `AgentPromptSubmitted` (or
    /// equivalent user-message event) has not yet committed because
    /// it is parked in the interception chain. Each entry triggers
    /// a `send_prompt_to_agent_for` call once the next
    /// user-message-bearing event commits — that's when the
    /// `AgentTree` reflects the prompt and the assembled message
    /// list will actually contain it. Without this, the agent
    /// receives a stale message list (the "Ready" loop bug). Owned by
    /// the defer/dispatch helpers in `harness::interception`.
    pub(crate) pending_user_prompt_dispatches: VecDeque<AgentId>,
    /// Conversations whose next agent prompt is ready except that an
    /// unrelated publish is still parked in the interception chain.
    /// These do not wait for another user-message fold; they drain
    /// once interception and deferred publishes are idle. Owned by the
    /// defer/dispatch helpers in `harness::interception`.
    pub(crate) pending_publish_idle_dispatches: VecDeque<AgentId>,
    /// All available models.
    pub(crate) available_models: Vec<ModelId>,
    /// Model snapshots published by provider extensions, keyed by sender
    /// connection.
    pub(crate) provider_models_by_extension: HashMap<String, Vec<ProviderModelInfo>>,
    /// Flattened provider model metadata keyed by model id. Rebuilt from
    /// [`Self::provider_models_by_extension`] whenever a provider snapshot
    /// changes.
    pub(crate) provider_model_info: HashMap<ModelId, ProviderModelInfo>,
    /// Provider extension connection for each model id. This is kept alongside
    /// [`Self::provider_model_info`] so prompt routing can address the provider
    /// that most recently published the selected model.
    pub(crate) provider_model_routes: HashMap<ModelId, tau_proto::ConnectionId>,
    /// Provider connection that received each in-flight prompt request.
    /// Incoming provider execution events must match this owner before the
    /// harness will publish streaming updates or accept the final response.
    pub(crate) pending_provider_prompts: HashMap<AgentPromptId, tau_proto::ConnectionId>,
    /// Available agent roles.
    pub(crate) available_roles: std::collections::HashMap<String, tau_config::settings::AgentRole>,
    /// Ordered role navigation groups for the currently available roles.
    pub(crate) available_role_groups: Vec<tau_proto::HarnessRoleGroup>,
    /// Reusable prompt templates from the effective startup harness settings.
    pub(crate) custom_prompts: Vec<tau_proto::HarnessCustomPrompt>,
    /// Handlebars template used to mint new durable agent identifiers.
    pub(crate) agent_id_template: String,
    /// Optional Handlebars template used to name newly created agents.
    pub(crate) agent_display_name_template: Option<String>,
    /// Role overrides changed at runtime for this process.
    pub(crate) role_overrides: std::collections::HashMap<String, tau_config::settings::AgentRole>,
    /// Harness-owned declarative tool tag policy applied before role overrides.
    pub(crate) tool_policy: tau_config::settings::ToolPolicy,
    /// Currently selected role. The resolved model is derived from this role
    /// and provider model availability.
    pub(crate) selected_role: String,
    /// Model currently resolved from [`Self::selected_role`] and provider
    /// availability. `None` means the role has no provider-published model yet.
    pub(crate) selected_model: Option<ModelId>,
    /// State that belongs to exactly the currently bound session.
    /// Keep session-scoped counters here instead of as top-level
    /// harness fields, so `/session new` resets them with one assignment.
    pub(crate) current_session_state: CurrentSessionState,
    /// Provider/model for each prompt sent to the provider, used to
    /// attribute the corresponding finished response even if the user
    /// switches models while it is in flight.
    pub(crate) prompt_models: std::collections::HashMap<AgentPromptId, ModelId>,
    /// Effective tool specs advertised for each in-flight prompt. Tool-call
    /// validation uses this snapshot so mid-turn role/model switches cannot
    /// change which tools the provider was allowed to call.
    pub(crate) prompt_tool_specs:
        std::collections::HashMap<AgentPromptId, Vec<tau_proto::ToolSpec>>,
    /// Prompt snapshot owner for each provider-emitted tool call id.
    pub(crate) prompt_tool_call_prompts: std::collections::HashMap<ToolCallId, AgentPromptId>,
    /// Selected skill winners, keyed by name.
    pub(crate) discovered_skills: std::collections::HashMap<tau_proto::SkillName, DiscoveredSkill>,
    /// All discovered skill candidates, keyed by name, so removing the winner
    /// can restore the next-best candidate.
    pub(crate) discovered_skill_candidates:
        std::collections::HashMap<tau_proto::SkillName, Vec<DiscoveredSkill>>,
    /// AGENTS.md files discovered by extensions, in delivery order.
    pub(crate) discovered_agents_files: Vec<DiscoveredAgentsFile>,
    /// Session-scoped JSON context contributions published by extensions.
    pub(crate) agent_context: AgentContextStore,
    /// Extensions that explicitly registered as per-agent prompt-context
    /// providers.
    pub(crate) agent_context_providers: HashSet<tau_proto::ConnectionId>,
    /// Per-agent context providers still expected to acknowledge the latest
    /// `session.agent_loaded` before that agent's first prompt can dispatch.
    pub(crate) pending_agent_context_ready:
        HashMap<tau_proto::AgentId, HashSet<tau_proto::ConnectionId>>,
    /// Extension-level prompt fragments keyed by source connection and name.
    pub(crate) extension_prompt_fragments:
        BTreeMap<tau_proto::ConnectionId, BTreeMap<String, PromptFragment>>,
    /// Loaded system prompt templates keyed by template name.
    pub(crate) system_prompt_templates: HashMap<String, String>,
    /// Sessions whose AGENTS/skill discovery has completed.
    pub(crate) initialized_sessions: std::collections::HashSet<SessionId>,
    /// Model-visible notices waiting to be folded into the next real user
    /// prompt.
    pub(crate) pending_notices: PendingPromptNoticeState,
    /// Pure scheduler state for queued and in-flight tool invocations.
    pub(crate) tool_turn: ToolTurnMachine,
    /// Backgrounded calls whose real completion should not enqueue an internal
    /// model-visible steering prompt. The real result/error event is still
    /// published normally.
    pub(crate) suppressed_background_completion_prompts: HashSet<ToolCallId>,
    /// Owning agents for background calls that have delivered their real
    /// completion. Kept so suppression can remove and later restore queued
    /// completion prompts across repeated wait/interrupt cycles.
    pub(crate) background_completion_targets: HashMap<ToolCallId, AgentId>,
    /// Prompt ids canceled by `/cancel`. Late agent events for these
    /// prompts are ignored and never folded into session state.
    pub(crate) canceled_prompts: std::collections::HashSet<AgentPromptId>,
    /// Extension-started side agents waiting for dispatch after their
    /// requested role, initial prompt, and queued messages have been resolved.
    pending_start_agent_requests: VecDeque<PendingStartAgentRequest>,
    /// State for harness-owned delegate/wait tools.
    pub(crate) subagents: SubagentToolState,
    /// Directory layout (config + state) the harness reads and writes.
    pub(crate) dirs: tau_config::settings::TauDirs,
}

#[cfg(any(test, feature = "echo-agent"))]
pub(crate) type ProviderRunner = fn(UnixStream, UnixStream) -> Result<(), String>;

/// One in-process tool extension to spawn alongside the echo provider during
/// tests.
#[cfg(any(test, feature = "echo-agent"))]
pub(crate) struct InProcessTool {
    pub(crate) name: &'static str,
    pub(crate) runner: fn(UnixStream, UnixStream) -> Result<(), String>,
}

/// A small echo provider used only by tests and echo-provider helpers.
#[cfg(any(test, feature = "echo-agent"))]
pub(crate) fn run_echo_provider<R, W>(
    reader: R,
    writer: W,
) -> Result<(), Box<dyn std::error::Error>>
where
    R: std::io::Read,
    W: std::io::Write,
{
    use std::io::{BufReader, BufWriter};

    use tau_proto::{
        ContentPart, ContextItem, ContextRole, Effort, EventName, HarnessInputMessage,
        HarnessOutputMessage, Hello, MessageItem, PROTOCOL_VERSION, PeerInputReader,
        PeerOutputWriter, ProviderModelInfo, ProviderModelsUpdated, ProviderPromptSubmitted, Ready,
        Subscribe, ThinkingSummary, ToolCallItem, ToolName, Verbosity,
    };

    fn materialize_prompt(prompt: &tau_proto::AgentPromptCreated) -> tau_proto::AgentPromptCreated {
        let mut materialized = prompt.clone();
        materialized.tools_ref = None;
        materialized
    }

    let mut reader = PeerInputReader::new(BufReader::new(reader));
    let mut writer = PeerOutputWriter::new(BufWriter::new(writer));

    writer.write_message(&HarnessInputMessage::Hello(Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "tau-echo-provider".into(),
        client_kind: ClientKind::Provider,
    }))?;
    // Live-only test provider: prompt and cancel events are work requests.
    // Replaying past ones would rerun or cancel completed turns.
    writer.write_message(&HarnessInputMessage::Subscribe(Subscribe {
        selectors: vec![
            EventSelector::Exact(EventName::AGENT_PROMPT_CREATED),
            EventSelector::Exact(EventName::UI_CANCEL_PROMPT),
        ],
    }))?;
    writer.write_message(&HarnessInputMessage::emit(Event::ProviderModelsUpdated(
        ProviderModelsUpdated {
            models: vec![ProviderModelInfo {
                id: "echo/model".into(),
                display_name: Some("Echo".to_owned()),
                tags: Vec::new(),
                default_affinity: 0,
                context_window: 128_000,
                efforts: vec![Effort::Off],
                verbosities: vec![Verbosity::Low],
                thinking_summaries: vec![ThinkingSummary::Off],
                supports_compaction: true,
            }],
        },
    )))?;
    writer.write_message(&HarnessInputMessage::Ready(Ready {
        message: Some("echo provider ready".to_owned()),
    }))?;
    writer.flush()?;

    let mut next_call = 1_u64;

    loop {
        let Some(message) = reader.read_message()? else {
            return Ok(());
        };
        let event = match message {
            HarnessOutputMessage::Deliver(delivery) => Some(delivery.into_event()),
            HarnessOutputMessage::Disconnect(_) => return Ok(()),
            _ => None,
        };
        if let Some(Event::AgentPromptCreated(prompt)) = event {
            let spid = prompt.agent_prompt_id.clone();
            let prompt = materialize_prompt(&prompt);
            let context_items = prompt.context.flatten();
            writer.write_message(&HarnessInputMessage::emit(Event::ProviderPromptSubmitted(
                ProviderPromptSubmitted {
                    agent_prompt_id: spid.clone(),
                    originator: prompt.originator.clone(),
                },
            )))?;

            let is_tool_result = context_items
                .last()
                .is_some_and(|item| matches!(item, ContextItem::ToolResult(_)));
            if is_tool_result {
                let text = context_items
                    .last()
                    .and_then(|item| match item {
                        ContextItem::ToolResult(result) => Some(result.output.render()),
                        _ => None,
                    })
                    .unwrap_or_default();
                writer.write_message(&HarnessInputMessage::emit(
                    Event::ProviderResponseFinished(ProviderResponseFinished {
                        agent_prompt_id: spid,
                        agent_id: prompt.agent_id.clone(),
                        output_items: vec![ContextItem::Message(MessageItem {
                            role: ContextRole::Assistant,
                            content: vec![ContentPart::Text { text }],
                            phase: None,
                        })],
                        stop_reason: ProviderStopReason::EndTurn,
                        error: None,
                        originator: prompt.originator.clone(),
                        usage: None,
                        compaction_original_input_tokens: None,
                        compaction_compacted_input_tokens: None,
                        backend: None,
                        provider_response_id: None,
                        ws_pool_delta: None,
                    }),
                ))?;
            } else {
                let user_text = context_items
                    .iter()
                    .rev()
                    .find_map(|item| match item {
                        ContextItem::Message(message) if message.role == ContextRole::User => {
                            message.content.first().map(|part| match part {
                                ContentPart::Text { text } => text.clone(),
                            })
                        }
                        _ => None,
                    })
                    .unwrap_or_default();

                let call_id = format!("call-{next_call}");
                next_call += 1;

                let tool_call = if let Some(path) = user_text.strip_prefix("read ") {
                    ToolCallItem {
                        call_id: call_id.into(),
                        name: ToolName::new("read"),
                        tool_type: tau_proto::ToolType::Function,
                        arguments: CborValue::Map(vec![(
                            CborValue::Text("path".to_owned()),
                            CborValue::Text(path.trim().to_owned()),
                        )]),
                    }
                } else if let Some(cmd) = user_text.strip_prefix("shell ") {
                    ToolCallItem {
                        call_id: call_id.into(),
                        name: ToolName::new("shell"),
                        tool_type: tau_proto::ToolType::Function,
                        arguments: CborValue::Map(vec![(
                            CborValue::Text("command".to_owned()),
                            CborValue::Text(cmd.trim().to_owned()),
                        )]),
                    }
                } else {
                    ToolCallItem {
                        call_id: call_id.into(),
                        name: ToolName::new("echo"),
                        tool_type: tau_proto::ToolType::Function,
                        arguments: CborValue::Text(user_text),
                    }
                };

                writer.write_message(&HarnessInputMessage::emit(
                    Event::ProviderResponseFinished(ProviderResponseFinished {
                        agent_prompt_id: spid,
                        agent_id: prompt.agent_id.clone(),
                        output_items: vec![ContextItem::ToolCall(tool_call)],
                        stop_reason: ProviderStopReason::ToolCalls,
                        error: None,
                        originator: prompt.originator.clone(),
                        usage: None,
                        compaction_original_input_tokens: None,
                        compaction_compacted_input_tokens: None,
                        backend: None,
                        provider_response_id: None,
                        ws_pool_delta: None,
                    }),
                ))?;
            }
            writer.flush()?;
        }
    }
}

/// Returns a closure that mints monotonic `ExtensionInstanceId`s starting
/// at zero. Used during harness construction so each extension entry gets
/// a distinct id without a manually managed counter that's easy to leave
/// dangling when extensions are added or removed.
fn instance_id_factory() -> impl FnMut() -> tau_proto::ExtensionInstanceId {
    let mut counter: u64 = 0;
    move || {
        let iid = tau_proto::ExtensionInstanceId::new(counter);
        counter += 1;
        iid
    }
}

enum BackgroundCompletionPromptMode {
    QueueAndAdvance,
    QueueOnly,
}

struct HarnessBaseParts {
    /// Sender side of the harness event channel.
    tx: Sender<HarnessEvent>,
    /// Receiver side of the harness event channel.
    rx: Receiver<HarnessEvent>,
    /// Event bus configured with the desired subscription policy.
    bus: EventBus,
    /// Runtime state directory for this harness.
    state_dir: PathBuf,
    /// Session membership store, with the eager session already loaded.
    store: SessionStore,
    /// Per-agent transcript store.
    agent_store: AgentStore,
    /// Session id the harness is initially bound to.
    current_session_id: SessionId,
    /// Reason associated with the initial session binding.
    current_session_start_reason: tau_proto::SessionStartReason,
    /// Roles available after applying harness settings.
    available_roles: HashMap<String, tau_config::settings::AgentRole>,
    /// Role groups available for navigation and UI display.
    available_role_groups: Vec<tau_proto::HarnessRoleGroup>,
    /// Reusable prompt templates loaded from effective harness settings.
    custom_prompts: Vec<tau_proto::HarnessCustomPrompt>,
    /// Runtime role overrides loaded from settings.
    role_overrides: HashMap<String, tau_config::settings::AgentRole>,
    /// Harness-owned declarative tool tag policy.
    tool_policy: tau_config::settings::ToolPolicy,
    /// Initially selected role name.
    selected_role: String,
    /// Initially selected model, if any provider metadata can resolve one.
    selected_model: Option<ModelId>,
    /// Template used to mint new agent ids.
    agent_id_template: String,
    /// Template used to display newly created agents.
    agent_display_name_template: Option<String>,
    /// Loaded system prompt templates keyed by template name.
    system_prompt_templates: HashMap<String, String>,
    /// Resolved Tau config/state directories.
    dirs: tau_config::settings::TauDirs,
}

impl Harness {
    /// Enables the test-only echo tool explicitly for every configured role.
    #[cfg(any(test, feature = "echo-agent"))]
    pub(crate) fn enable_echo_tool_for_tests(&mut self) {
        let echo = tau_proto::ToolName::new("echo");
        for role in self.available_roles.values_mut() {
            if !role.enable_tools.iter().any(|tool| tool == &echo) {
                role.enable_tools.push(echo.clone());
            }
        }
    }

    fn from_base_parts(parts: HarnessBaseParts) -> Self {
        let discovered_skills = built_in_discovered_skills();
        let discovered_skill_candidates = discovered_skills
            .iter()
            .map(|(name, skill)| (name.clone(), vec![skill.clone()]))
            .collect();
        Self {
            tx: parts.tx,
            rx: parts.rx,
            bus: parts.bus,
            registry: ToolRegistry::new(),
            action_registry: ActionRegistry::new(),
            internal_tool_handlers: Vec::new(),
            state_dir: parts.state_dir,
            store: parts.store,
            agent_store: parts.agent_store,
            current_session_id: parts.current_session_id,
            current_session_start_reason: parts.current_session_start_reason,
            agent_id_rng: StdRng::from_entropy(),
            tool_agents: HashMap::new(),
            pending_tools: HashMap::new(),
            completed_tool_calls: HashSet::new(),
            pending_tool_providers: HashMap::new(),
            pending_action_invocations: HashMap::new(),
            event_log: EventLog::new(),
            client_writers: HashMap::new(),
            startup_detach_requested: false,
            lifecycle_messages: Vec::new(),
            replayable_harness_notices: Vec::new(),
            extensions: ExtensionRuntimeState::default(),
            prompt_agents: HashMap::new(),
            agents: HashMap::new(),
            agent_routes: HashMap::new(),
            session_loaded_agents: HashSet::new(),
            agent_states: HashMap::new(),
            stopped_agent_ids: HashSet::new(),
            turn_state: TurnState::Idle,
            debug_log: None,
            interceptors: InterceptorRegistry::default(),
            pending_intercept: None,
            deferred_publishes: VecDeque::new(),
            pending_user_prompt_dispatches: VecDeque::new(),
            pending_publish_idle_dispatches: VecDeque::new(),
            available_models: Vec::new(),
            provider_models_by_extension: HashMap::new(),
            provider_model_info: HashMap::new(),
            provider_model_routes: HashMap::new(),
            pending_provider_prompts: HashMap::new(),
            available_roles: parts.available_roles,
            available_role_groups: parts.available_role_groups,
            custom_prompts: parts.custom_prompts,
            role_overrides: parts.role_overrides,
            tool_policy: parts.tool_policy,
            agent_id_template: parts.agent_id_template,
            agent_display_name_template: parts.agent_display_name_template,
            selected_role: parts.selected_role,
            selected_model: parts.selected_model,
            current_session_state: CurrentSessionState::default(),
            prompt_models: HashMap::new(),
            prompt_tool_specs: HashMap::new(),
            prompt_tool_call_prompts: HashMap::new(),
            discovered_skills,
            discovered_skill_candidates,
            discovered_agents_files: Vec::new(),
            agent_context: AgentContextStore::default(),
            agent_context_providers: HashSet::new(),
            pending_agent_context_ready: HashMap::new(),
            extension_prompt_fragments: BTreeMap::new(),
            system_prompt_templates: parts.system_prompt_templates,
            initialized_sessions: HashSet::new(),
            pending_notices: PendingPromptNoticeState::default(),
            tool_turn: ToolTurnMachine::default(),
            suppressed_background_completion_prompts: HashSet::new(),
            background_completion_targets: HashMap::new(),
            canceled_prompts: HashSet::new(),
            pending_start_agent_requests: VecDeque::new(),
            subagents: SubagentToolState::default(),
            dirs: parts.dirs,
        }
    }

    #[cfg(any(test, feature = "echo-agent"))]
    pub(crate) fn new_with_provider(
        state_dir: impl Into<PathBuf>,
        dirs: tau_config::settings::TauDirs,
        provider_runner: ProviderRunner,
        tools: Vec<InProcessTool>,
        eager_session_id: &str,
        eager_session_start_reason: tau_proto::SessionStartReason,
    ) -> Result<Self, HarnessError> {
        let state_dir = state_dir.into();
        let sessions_dir = tau_config::settings::sessions_dir_of(&state_dir);
        let (tx, rx) = mpsc::channel();
        let bus =
            EventBus::with_subscription_policy(Box::new(DefaultSubscriptionPolicy::with_store(
                PolicyStore::open(policy_store_path_from(&state_dir))?,
            )));
        // Lazy: only the eager session's tree is needed up front
        // (loaded below via `store.load_session`); other sessions
        // load on first access. Avoids a startup walk over every
        // historical session dir.
        let agents_dir = state_dir.join("agents");
        let store = SessionStore::open_lazy(&sessions_dir)?;
        let agent_store = AgentStore::open_lazy(&agents_dir)?;

        let own_pid = std::process::id();
        let mut next_iid = instance_id_factory();

        let mut extension_connects = Vec::new();
        // Provider
        let provider_spawn =
            spawn_in_process("provider", ClientKind::Provider, provider_runner, &tx)?;
        let provider_conn_id = provider_spawn.connection_id.clone();
        extension_connects.push(ExtensionConnectCommand {
            entry: ExtensionEntry {
                name: "provider".to_owned(),
                instance_id: next_iid(),
                connection_id: provider_conn_id,
                kind: ClientKind::Provider,
                require: true,
                respawn_allowed: true,
                pid: Some(own_pid),
                in_process_thread: Some(provider_spawn.thread),
                supervised_config: None,
                secrets: BTreeMap::new(),
                restart_attempt: 0,
                state: ExtensionState::Spawning,
            },
            origin: ConnectionOrigin::Supervised,
            writer_tx: provider_spawn.writer_tx,
            initialized_ack: provider_spawn.initialized_ack,
            replaces: None,
        });

        // Caller-supplied in-process tools.
        for tool in tools {
            let tool_spawn = spawn_in_process(tool.name, ClientKind::Tool, tool.runner, &tx)?;
            let conn_id = tool_spawn.connection_id.clone();
            extension_connects.push(ExtensionConnectCommand {
                entry: ExtensionEntry {
                    name: tool.name.to_owned(),
                    instance_id: next_iid(),
                    connection_id: conn_id,
                    kind: ClientKind::Tool,
                    require: true,
                    respawn_allowed: true,
                    pid: Some(own_pid),
                    in_process_thread: Some(tool_spawn.thread),
                    supervised_config: None,
                    secrets: BTreeMap::new(),
                    restart_attempt: 0,
                    state: ExtensionState::Spawning,
                },
                origin: ConnectionOrigin::Supervised,
                writer_tx: tool_spawn.writer_tx,
                initialized_ack: tool_spawn.initialized_ack,
                replaces: None,
            });
        }

        let (harness_settings, harness_settings_error) = load_harness_settings_or_warn(&dirs);
        let system_prompt_templates = load_system_prompt_templates(dirs.config_dir.as_deref());
        let LoadedRoles {
            roles: available_roles,
            role_overrides,
            selected_role,
            role_groups: available_role_groups,
            missing_default_role,
        } = load_roles(&harness_settings);
        let custom_prompts = harness_settings
            .custom_prompts
            .iter()
            .map(|prompt| tau_proto::HarnessCustomPrompt {
                id: prompt.id.clone(),
                text: prompt.text.clone(),
            })
            .collect();
        if available_roles.is_empty() {
            return Err(HarnessError::Participant(
                "no roles are enabled; enable at least one role in harness.yaml or with --enable-role <role>".to_owned(),
            ));
        }
        let selected_model =
            select_model_for_role(&HashMap::new(), &available_roles, &selected_role);
        crate::session_cleanup::spawn_session_cleanup(
            sessions_dir.clone(),
            harness_settings.session_retention(),
        );
        let mut store = store;
        let _ = store.load_session(eager_session_id)?;
        let mut harness = Self::from_base_parts(HarnessBaseParts {
            tx,
            rx,
            bus,
            state_dir: state_dir.clone(),
            store,
            agent_store,
            current_session_id: eager_session_id.into(),
            current_session_start_reason: eager_session_start_reason,
            available_roles,
            available_role_groups,
            custom_prompts,
            role_overrides,
            tool_policy: harness_settings.tool_policy.clone(),
            selected_role,
            selected_model,
            agent_id_template: harness_settings.agent_id_template.clone(),
            agent_display_name_template: harness_settings.agent_display_name_template.clone(),
            system_prompt_templates,
            dirs,
        });

        // Debug log lives next to the eager-init session's events file
        // so the session dir stays self-contained: `events.cbor` +
        // `events.jsonl` + `meta.json` + `lock`.
        let _ = harness.enable_debug_log(&sessions_dir.join(eager_session_id))?;
        // Record metadata so `-r` can find this session even before it has
        // membership entries. Also acquires the flock on
        // `<sessions_dir>/<eager_session_id>/lock`.
        harness.store.record_session_meta(eager_session_id)?;

        if matches!(
            eager_session_start_reason,
            tau_proto::SessionStartReason::Resume
        ) {
            harness.rehydrate_agents_from_session();
        }
        harness.publish_current_session_dir();

        for command in extension_connects {
            harness.queue_extension_connect(command)?;
        }
        harness.wait_for_extensions_ready()?;
        #[cfg(test)]
        harness.register_harness_tools();
        harness.publish_delegate_roles_context();
        harness.check_config_exists();
        harness.emit_startup_settings_errors(harness_settings_error);
        harness.emit_missing_default_role(missing_default_role);

        // Eager session init for the default session. INTENTIONAL —
        // do NOT "simplify" this to lazy-on-first-prompt.
        //
        // Reasons this is a design choice, not dead weight:
        //
        // 1. **Pre-warm AGENTS.md and skill discovery.** The default session is the
        //    fallback when a caller (embedded or socket) doesn't specify one, and even
        //    when callers pick their own `chat-<ts>` id they still benefit: ext-shell
        //    has already walked the user agent roots + the cwd ancestor chain once, so
        //    the second init is cache-warm.
        //
        // 2. **Surface discovery before the first prompt.** The CLI prints "loaded: …"
        //    as events arrive; doing this at startup gives the user visible
        //    confirmation that their AGENTS.md was found — before they type anything —
        //    instead of bundling that feedback into the first agent response.
        //
        // 3. **Fail loudly at startup, not mid-first-turn.** If a provider hangs or the
        //    discovery logic panics, the process hits `StartupTimeout` here rather than
        //    appearing to accept the first prompt and then silently stalling.
        //
        // Every past agent that touched this code has "noticed" that
        // the CLI uses `chat-<ts>` session ids and concluded the eager
        // init is wasted work. It isn't. Please resist the urge.
        harness.start_session_init(eager_session_id.into(), eager_session_start_reason);
        harness.wait_for_session_init()?;
        Ok(harness)
    }

    /// Creates a harness from configuration, spawning real child processes.
    pub(crate) fn from_config(
        config: &Config,
        state_dir: impl Into<PathBuf>,
        dirs: tau_config::settings::TauDirs,
        eager_session_id: &str,
        eager_session_start_reason: tau_proto::SessionStartReason,
    ) -> Result<Self, HarnessError> {
        let mut initial_client_error_stream = None;
        Self::from_config_with_initial_client(
            config,
            state_dir,
            dirs,
            eager_session_id,
            eager_session_start_reason,
            None,
            &mut initial_client_error_stream,
        )
        .map(|(harness, _)| harness)
    }

    pub(crate) fn from_config_with_initial_client(
        config: &Config,
        state_dir: impl Into<PathBuf>,
        dirs: tau_config::settings::TauDirs,
        eager_session_id: &str,
        eager_session_start_reason: tau_proto::SessionStartReason,
        initial_client: Option<InitialClient>,
        initial_client_error_stream: &mut Option<InitialClientStartupErrorOutput>,
    ) -> Result<(Self, Option<ConnectionId>), HarnessError> {
        let startup_started_at = Instant::now();
        tracing::debug!(target: "tau_harness::startup", eager_session_id, "constructing harness from config");
        let state_dir = state_dir.into();
        let sessions_dir = tau_config::settings::sessions_dir_of(&state_dir);
        let (tx, rx) = mpsc::channel();
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "opening policy store");
        let policy_store = PolicyStore::open(policy_store_path_from(&state_dir))?;
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "policy store opened");
        let bus = EventBus::with_subscription_policy(Box::new(
            DefaultSubscriptionPolicy::with_store(policy_store),
        ));
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "opening session store");
        let agents_dir = state_dir.join("agents");
        let store = SessionStore::open_lazy(&sessions_dir)?;
        let agent_store = AgentStore::open_lazy(&agents_dir)?;
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "session store opened");

        let secret_sources =
            load_secret_sources().map_err(|error| HarnessError::Participant(error.to_string()))?;
        let extension_secrets = resolve_extension_secrets(config, &state_dir, &secret_sources)
            .map_err(|error| HarnessError::Participant(error.to_string()))?;

        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "loading harness settings");
        let (harness_settings, harness_settings_error) = load_harness_settings_or_warn(&dirs);
        let system_prompt_templates = load_system_prompt_templates(dirs.config_dir.as_deref());
        let LoadedRoles {
            roles: available_roles,
            role_overrides,
            selected_role,
            role_groups: available_role_groups,
            missing_default_role,
        } = load_roles(&harness_settings);
        let custom_prompts = harness_settings
            .custom_prompts
            .iter()
            .map(|prompt| tau_proto::HarnessCustomPrompt {
                id: prompt.id.clone(),
                text: prompt.text.clone(),
            })
            .collect();
        if available_roles.is_empty() {
            return Err(HarnessError::Participant(
                "no roles are enabled; enable at least one role in harness.yaml or with --enable-role <role>".to_owned(),
            ));
        }
        let selected_model =
            select_model_for_role(&HashMap::new(), &available_roles, &selected_role);
        tracing::debug!(target: "tau_harness::startup", selected_model = ?selected_model, elapsed_ms = startup_started_at.elapsed().as_millis(), "harness settings loaded");
        crate::session_cleanup::spawn_session_cleanup(
            sessions_dir.clone(),
            harness_settings.session_retention(),
        );
        let mut store = store;
        let _ = store.load_session(eager_session_id)?;
        let mut harness = Self::from_base_parts(HarnessBaseParts {
            tx,
            rx,
            bus,
            state_dir: state_dir.clone(),
            store,
            agent_store,
            current_session_id: eager_session_id.into(),
            current_session_start_reason: eager_session_start_reason,
            available_roles,
            available_role_groups,
            custom_prompts,
            role_overrides,
            tool_policy: harness_settings.tool_policy.clone(),
            selected_role,
            selected_model,
            agent_id_template: harness_settings.agent_id_template.clone(),
            agent_display_name_template: harness_settings.agent_display_name_template.clone(),
            system_prompt_templates,
            dirs,
        });

        let _ = harness.enable_debug_log(&sessions_dir.join(eager_session_id))?;
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "debug event log enabled");
        // Record metadata so `-r` can find this session even before it has
        // membership entries. Also acquires the flock on
        // `<sessions_dir>/<eager_session_id>/lock`.
        harness.store.record_session_meta(eager_session_id)?;
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "session metadata recorded");

        if matches!(
            eager_session_start_reason,
            tau_proto::SessionStartReason::Resume
        ) {
            harness.rehydrate_agents_from_session();
        }
        let initial_client_id = if let Some(initial_client) = initial_client {
            let client_id = match initial_client {
                InitialClient::Stdio => harness.accept_stdio_client()?,
            };
            *initial_client_error_stream = None;
            if let Err(error) = harness.wait_for_initial_ui_subscribe() {
                harness.send_startup_disconnect_to_initial_client(Some(&client_id), &error);
                return Err(error);
            }
            Some(client_id)
        } else {
            None
        };
        harness.publish_current_session_dir();
        harness.emit_extension_startup_diagnostics(&config.extension_startup_diagnostics);
        harness.emit_extension_startup_diagnostics(&extension_secrets.diagnostics);

        if let Err(error) = harness.spawn_configured_extensions(
            config,
            &sessions_dir,
            eager_session_id,
            &extension_secrets.secrets,
            &extension_secrets.skipped_extensions,
            startup_started_at,
        ) {
            harness.send_startup_disconnect_to_initial_client(initial_client_id.as_ref(), &error);
            return Err(error);
        }
        if let Err(error) = harness.wait_for_extensions_ready() {
            harness.send_startup_disconnect_to_initial_client(initial_client_id.as_ref(), &error);
            return Err(error);
        }
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "extensions ready");
        #[cfg(test)]
        harness.register_harness_tools();
        harness.publish_delegate_roles_context();
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "harness tools registered");
        harness.check_config_exists();
        harness.emit_startup_settings_errors(harness_settings_error);
        harness.emit_missing_default_role(missing_default_role);
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "config checks complete");

        harness.start_session_init(eager_session_id.into(), eager_session_start_reason);
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "session init started");
        if let Err(error) = harness.wait_for_session_init() {
            harness.send_startup_disconnect_to_initial_client(initial_client_id.as_ref(), &error);
            return Err(error);
        }
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "session init complete");
        Ok((harness, initial_client_id))
    }

    fn spawn_configured_extensions(
        &mut self,
        config: &Config,
        sessions_dir: &Path,
        eager_session_id: &str,
        extension_secrets: &BTreeMap<String, BTreeMap<String, SecretValue>>,
        skipped_extensions: &BTreeSet<String>,
        startup_started_at: Instant,
    ) -> Result<(), HarnessError> {
        let mut extension_connects = Vec::new();
        let mut next_iid = instance_id_factory();
        for ext_config in config.extensions.values() {
            if skipped_extensions.contains(&ext_config.name) {
                continue;
            }
            tracing::info!(
                target: "tau_harness::startup",
                extension = %ext_config.name,
                command = %ext_config.command,
                args = ?ext_config.args,
                elapsed_ms = startup_started_at.elapsed().as_millis(),
                "spawning extension",
            );
            let kind = match ext_config.role.as_deref() {
                Some("provider") => ClientKind::Provider,
                _ => ClientKind::Tool,
            };

            let log_path =
                extension_stderr_log_path(sessions_dir, eager_session_id, &ext_config.name)
                    .map_err(|error| HarnessError::Participant(error.to_string()))?;
            let spawned = match spawn_supervised(ext_config, kind.clone(), Some(log_path), &self.tx)
            {
                Ok(spawned) => spawned,
                Err(error) if !ext_config.require => {
                    tracing::warn!(
                        target: "tau_harness::startup",
                        extension = %ext_config.name,
                        error = %error,
                        "optional extension did not initialize during spawn"
                    );
                    self.emit_optional_extension_skipped(&format!(
                        "optional extension {} did not initialize",
                        ext_config.name
                    ));
                    continue;
                }
                Err(error) => return Err(error),
            };
            let conn_id = spawned.connection_id.clone();
            tracing::info!(
                target: "tau_harness::startup",
                extension = %ext_config.name,
                pid = spawned.child_pid,
                elapsed_ms = startup_started_at.elapsed().as_millis(),
                "extension spawned",
            );

            extension_connects.push(ExtensionConnectCommand {
                entry: ExtensionEntry {
                    name: ext_config.name.clone(),
                    instance_id: next_iid(),
                    connection_id: conn_id,
                    kind: kind.clone(),
                    require: ext_config.require,
                    respawn_allowed: true,
                    pid: Some(spawned.child_pid),
                    in_process_thread: None,
                    supervised_config: Some(ext_config.clone()),
                    secrets: extension_secrets
                        .get(&ext_config.name)
                        .cloned()
                        .unwrap_or_default(),
                    restart_attempt: 0,
                    state: ExtensionState::Spawning,
                },
                origin: ConnectionOrigin::Supervised,
                writer_tx: spawned.writer_tx,
                initialized_ack: spawned.initialized_ack,
                replaces: None,
            });
        }
        for command in extension_connects {
            self.queue_extension_connect(command)?;
        }
        Ok(())
    }

    fn wait_for_initial_ui_subscribe(&mut self) -> Result<(), HarnessError> {
        let started_at = Instant::now();
        loop {
            let remaining = STARTUP_TIMEOUT
                .checked_sub(started_at.elapsed())
                .unwrap_or(Duration::ZERO);
            let harness_evt = self
                .rx
                .recv_timeout(remaining)
                .map_err(|_| HarnessError::StartupTimeout)?;
            self.log_event(&harness_evt);
            match harness_evt {
                HarnessEvent::FromConnection {
                    connection_id,
                    message,
                } => {
                    if self.handle_startup_from_connection(&connection_id, *message)? {
                        return Ok(());
                    }
                }
                HarnessEvent::Disconnected { connection_id } => {
                    self.handle_startup_disconnect(&connection_id)?;
                }
                HarnessEvent::NewClient(stream) => {
                    self.accept_client(stream)?;
                }
                HarnessEvent::Command(command) => self.handle_harness_command(command)?,
            }
        }
    }

    fn handle_startup_from_connection(
        &mut self,
        connection_id: &str,
        message: HarnessInputMessage,
    ) -> Result<bool, HarnessError> {
        let origin = self.bus.connection(connection_id).map(|m| m.origin.clone());
        match origin {
            Some(ConnectionOrigin::Socket) => {
                let detach_requested = matches!(
                    &message,
                    HarnessInputMessage::Emit(emit)
                        if matches!(emit.event.as_ref(), Event::UiDetachRequest(_))
                );
                let subscribed = matches!(&message, HarnessInputMessage::Subscribe(_));
                if detach_requested {
                    self.startup_detach_requested = true;
                }
                let keep = self.handle_client_message(connection_id, message)?;
                if !keep {
                    self.handle_disconnect(connection_id);
                    if self.startup_detach_requested {
                        return Ok(false);
                    }
                    return Err(HarnessError::Participant(
                        "initial UI disconnected during startup handshake".to_owned(),
                    ));
                }
                Ok(subscribed)
            }
            Some(_) => {
                self.handle_extension_message(connection_id, message)?;
                Ok(false)
            }
            None => Ok(false),
        }
    }

    fn handle_startup_disconnect(&mut self, connection_id: &str) -> Result<(), HarnessError> {
        let name = self
            .bus
            .connection(connection_id)
            .map(|m| m.name.clone())
            .unwrap_or_else(|| connection_id.to_string());
        let was_socket = self
            .bus
            .connection(connection_id)
            .is_some_and(|m| m.origin == ConnectionOrigin::Socket);
        let was_provider = self.is_provider_extension(connection_id);
        let optional_pre_ready_extension = self
            .extensions
            .entries
            .get(connection_id)
            .is_some_and(|entry| !entry.require && entry.state != ExtensionState::Ready);
        if optional_pre_ready_extension {
            tracing::warn!(
                target: "tau_harness::startup",
                extension = %name,
                "optional extension did not initialize: disconnected before becoming ready"
            );
            self.disable_optional_extension(
                connection_id,
                &format!("optional extension {name} did not initialize"),
            );
            return Ok(());
        }
        self.handle_disconnect(connection_id);
        if was_socket {
            if self.startup_detach_requested {
                return Ok(());
            }
            return Err(HarnessError::Participant(format!(
                "{name} disconnected during startup"
            )));
        }
        if was_provider {
            return Err(provider_disconnected_error());
        }
        Ok(())
    }

    fn log_event(&mut self, harness_event: &HarnessEvent) {
        if let Some(log) = &mut self.debug_log {
            log.log_harness_event(harness_event);
        }
    }

    fn queue_extension_connect(
        &mut self,
        command: ExtensionConnectCommand,
    ) -> Result<(), HarnessError> {
        self.extensions.pending_connects += 1;
        if self
            .tx
            .send(HarnessEvent::Command(HarnessCommand::ConnectExtension(
                Box::new(command),
            )))
            .is_ok()
        {
            return Ok(());
        }
        self.extensions.pending_connects -= 1;
        Err(HarnessError::Participant(
            "harness command channel closed".to_owned(),
        ))
    }

    fn handle_harness_command(&mut self, command: HarnessCommand) -> Result<(), HarnessError> {
        match command {
            HarnessCommand::ConnectExtension(command) => self.connect_extension(*command),
        }
        Ok(())
    }

    fn connect_extension(&mut self, command: ExtensionConnectCommand) {
        let ExtensionConnectCommand {
            entry,
            origin,
            writer_tx,
            initialized_ack,
            replaces,
        } = command;
        let connection_id = entry.connection_id.clone();
        let name = entry.name.clone();
        let kind = entry.kind.clone();

        let connected_id = self.bus.connect(Connection::new(
            ConnectionMetadata {
                id: connection_id.clone(),
                name: name.clone(),
                kind,
                origin,
            },
            Box::new(ChannelSink { tx: writer_tx }),
        ));
        debug_assert_eq!(connected_id, connection_id);

        if let Some(replaced) = replaces {
            self.extensions.entries.remove(&replaced);
            self.extensions.activation_staging.remove(&replaced);
            if let Some(slot) = self.extensions.order.iter_mut().find(|id| **id == replaced) {
                *slot = connection_id.clone();
            } else if !self.extensions.order.iter().any(|id| id == &connection_id) {
                self.extensions.order.push(connection_id.clone());
            }
        } else if !self.extensions.order.iter().any(|id| id == &connection_id) {
            self.extensions.order.push(connection_id.clone());
        }
        self.extensions
            .activation_staging
            .insert(connection_id.clone(), ExtensionActivationStage::default());
        self.extensions.entries.insert(connection_id, entry);
        if 0 < self.extensions.pending_connects {
            self.extensions.pending_connects -= 1;
        }
        self.emit_extension_starting(&name);
        let _ = initialized_ack.send(());
    }

    /// Agent id that owns a given in-flight prompt, if any.
    fn agent_id_for_prompt(&self, spid: &AgentPromptId) -> Option<AgentId> {
        self.prompt_agents.get(spid).cloned()
    }

    /// If the agent's dedup map's "built for" cursor doesn't
    /// match its current `head`, rebuild it from the assembled branch.
    /// O(branch_len) on rebuild; O(1) on the steady-state hot path
    /// where the linear-extension hook in [`Self::commit_event`] keeps
    /// `built_for` in sync after every fold.
    ///
    /// `None` is returned only if the conversation no longer exists
    /// (the caller raced its own teardown), and the caller treats that
    /// as "skip dedup, just publish".
    fn ensure_dedup_built_for_branch(&mut self, cid: &AgentId) -> Option<()> {
        let head = self.agents.get(cid)?.head;
        let needs = self
            .agents
            .get(cid)
            .map(|c| c.result_dedup.needs_rebuild(head))
            .unwrap_or(false);
        if !needs {
            return Some(());
        }
        // Walk the branch under an immutable borrow of the store, then
        // hand the snapshot to the conversation under a mut borrow —
        // the branch iterator borrows the tree, so we materialize it
        // into an owned Vec first to release the tree borrow.
        let agent_id = self.agents.get(cid)?.agent_id.clone();
        let branch: Vec<tau_core::AgentEntry> = agent_id
            .as_deref()
            .and_then(|agent_id| self.agent_store.agent(agent_id))
            .map(|t| t.branch_from(head).into_iter().cloned().collect())
            .unwrap_or_default();
        let conv = self.agents.get_mut(cid)?;
        conv.result_dedup
            .rebuild_from_branch(branch.iter(), head, DEFAULT_THRESHOLD_BYTES);
        Some(())
    }

    /// Replace `result.result` with a pointer if a previous tool
    /// result on this agent's branch has the same content.
    /// Mutates `result` in place; the caller publishes the (possibly
    /// modified) value, which is what gets folded into the tree and
    /// what the LLM sees on the next turn.
    fn dedup_tool_result(&mut self, cid: &AgentId, result: &mut tau_proto::ToolResult) {
        if self.ensure_dedup_built_for_branch(cid).is_none() {
            return;
        }
        let bytes = encode_for_hash(&result.result);
        if bytes.len() < DEFAULT_THRESHOLD_BYTES {
            return;
        }
        let hash = hash_truncated(&bytes);
        let Some(conv) = self.agents.get_mut(cid) else {
            return;
        };
        if let Some(original_call_id) = conv.result_dedup.lookup(&hash).cloned() {
            // Belt-and-suspenders: refuse to point a call at itself.
            // This can't happen in practice — `tool_agents`
            // already drops the call_id between intake and now — but
            // a future change to the tracking map could let a tool
            // result re-enter this path twice, and self-pointing is a
            // worse failure mode than just skipping the dedup.
            if original_call_id == result.call_id {
                return;
            }
            tracing::debug!(
                target: "tau_harness",
                cid = %cid,
                tool = %result.tool_name,
                call_id = %result.call_id,
                points_to = %original_call_id,
                bytes = bytes.len(),
                "deduping tool result against earlier identical output"
            );
            result.result = build_pointer_value(&original_call_id, &result.tool_name);
        } else {
            conv.result_dedup.insert(hash, result.call_id.clone());
        }
    }

    /// Companion to [`Self::dedup_tool_result`] for `ToolError`s.
    /// Same semantics — collapses repeated identical errors (same
    /// message, same `details`) into a pointer back to the first
    /// occurrence on this branch.
    fn dedup_tool_error(&mut self, cid: &AgentId, error: &mut tau_proto::ToolError) {
        if self.ensure_dedup_built_for_branch(cid).is_none() {
            return;
        }
        let bytes = encode_error_for_hash(&error.message, error.details.as_ref());
        if bytes.len() < DEFAULT_THRESHOLD_BYTES {
            return;
        }
        let hash = hash_truncated(&bytes);
        let Some(conv) = self.agents.get_mut(cid) else {
            return;
        };
        if let Some(original_call_id) = conv.result_dedup.lookup(&hash).cloned() {
            if original_call_id == error.call_id {
                return;
            }
            tracing::debug!(
                target: "tau_harness",
                cid = %cid,
                tool = %error.tool_name,
                call_id = %error.call_id,
                points_to = %original_call_id,
                bytes = bytes.len(),
                "deduping tool error against earlier identical output"
            );
            error.message = build_pointer_error_message(&original_call_id, &error.tool_name);
            error.details = None;
        } else {
            conv.result_dedup.insert(hash, error.call_id.clone());
        }
    }

    /// Publishes an event for a specific conversation. The fold uses
    /// the agent's `head` as the explicit parent — no more
    /// `UiNavigateTree` head-bouncing — and the post-commit hook in
    /// [`Harness::commit_event`] keeps `c.head` in sync with the
    /// freshly-folded node.
    ///
    /// This helper is what makes branching prompts work: a user
    /// conversation can keep advancing while a side agent from an
    /// extension grows its own branch off some earlier node;
    /// each side publish brackets its own navigate-then-append.
    pub(crate) fn publish_for_agent(&mut self, cid: &AgentId, event: Event) {
        self.publish_for_agent_from(cid, None, event);
    }

    fn publish_terminal_tool_result(
        &mut self,
        cid: Option<&AgentId>,
        source: Option<&str>,
        result: ToolResult,
    ) {
        match cid {
            Some(cid) => {
                self.publish_for_agent_from(cid, source, Event::ToolResult(result.clone()));
                self.publish_for_agent_from(cid, source, Event::ProviderToolResult(result.clone()));
            }
            None => {
                self.publish_event(source, Event::ToolResult(result.clone()));
                self.publish_event(source, Event::ProviderToolResult(result.clone()));
            }
        }
        self.record_wait_tool_result(result);
    }

    fn publish_terminal_tool_error(
        &mut self,
        cid: Option<&AgentId>,
        source: Option<&str>,
        error: ToolError,
    ) {
        match cid {
            Some(cid) => {
                self.publish_for_agent_from(cid, source, Event::ToolError(error.clone()));
                self.publish_for_agent_from(cid, source, Event::ProviderToolError(error.clone()));
            }
            None => {
                self.publish_event(source, Event::ToolError(error.clone()));
                self.publish_event(source, Event::ProviderToolError(error.clone()));
            }
        }
        self.record_wait_tool_error(error);
    }

    fn publish_terminal_background_error(
        &mut self,
        cid: &AgentId,
        source: Option<&str>,
        error: ToolBackgroundError,
    ) {
        self.publish_for_agent_from(cid, source, Event::ToolBackgroundError(error.clone()));
        self.record_wait_background_error(error);
    }

    /// Like [`publish_for_agent`] but lets the caller record an
    /// originating connection on the persisted record (for `tool.result`
    /// / `tool.error` arriving from extensions). The snap-to-`cid`-head
    /// step is what keeps cross-conversation tool activity from folding
    /// onto the wrong tree branch — without it, a sibling side conv that
    /// just navigated `tree.head` would steal the parent of the next
    /// tree-folding event.
    fn publish_for_agent_from(&mut self, cid: &AgentId, source: Option<&str>, event: Event) {
        // Stamp the publish with `cid`. The fold reads the
        // agent's `head` as the explicit parent node in
        // `commit_event`, so cross-conversation publishes no longer
        // need a `UiNavigateTree` round-trip to bounce the global
        // write cursor. After the commit, the post-commit hook
        // also syncs `c.head` automatically — the trailing
        // read-tree-and-update idiom is gone entirely.
        //
        // Re-stamp tool events with the owning agent's
        // originator so subscribers can tell main-agent tool
        // activity from sub-agent tool activity without having to
        // map `call_id` back to a conversation themselves. Construction
        // sites can leave `originator` as the default — this is the
        // single point of truth.
        let event = if let Some(originator) = self.agents.get(cid).map(|c| c.originator.clone()) {
            stamp_tool_event_originator(event, originator)
        } else {
            event
        };
        self.publish_event_for_agent(cid, source, event);
    }

    /// Publishes an event to both the event bus and the event log.
    /// Convenience wrapper that uses the event's default transience
    /// and never marks the publish as `must_pass`.
    pub(crate) fn publish_event(&mut self, source: Option<&str>, event: Event) {
        let transient = event.defaults_to_transient();
        self.enqueue_publish(source, event, transient, false, None);
    }

    /// Like [`Harness::publish_event`] but tags the publish with the
    /// originating agent. After the event commits, the
    /// harness syncs that agent's cached `head` to the
    /// freshly-folded `tree.head()` — so callers don't need to read
    /// the tree themselves (which would race the interception chain
    /// when a publish parks).
    fn publish_event_for_agent(&mut self, cid: &AgentId, source: Option<&str>, event: Event) {
        if !self.agents.contains_key(cid) {
            // The conversation was torn down between when the
            // caller looked it up and now (e.g. side conv that
            // raced its own teardown with a late tool result).
            // Fall back to a plain publish so the event still
            // reaches the bus / log; we just can't stamp a parent
            // for it.
            tracing::warn!(
                target: "tau_harness",
                event = %event.name(),
                cid = %cid,
                "publish_event_for_agent called with unknown cid; \
                 publishing without parent stamp",
            );
            self.publish_event(source, event);
            return;
        }
        let transient = event.defaults_to_transient();
        let agent_id = self.agent_id_for_event(&event).or_else(|| {
            self.agents
                .get(cid)
                .and_then(|conv| conv.agent_id.as_ref())
                .cloned()
                .map(crate::parse_agent_id)
        });
        let sync = Some(ConversationHeadSync {
            cid: cid.clone(),
            agent_id,
        });
        self.enqueue_publish(source, event, transient, false, sync);
    }

    fn note_agent_prompt_created(&mut self, prompt: &AgentPromptCreated) {
        if let Some(cid) = self.prompt_agents.get(&prompt.agent_prompt_id).cloned()
            && let Some(conv) = self.agents.get_mut(&cid)
        {
            conv.last_prompt_id = Some(prompt.agent_prompt_id.clone());
        }
    }

    fn provider_route_for_prompt_request(&self, event: &Event) -> Option<tau_proto::ConnectionId> {
        let model = match event {
            Event::AgentPromptCreated(prompt) => Some(&prompt.model),
            _ => None,
        }?;
        self.provider_model_routes.get(model).cloned()
    }

    fn track_provider_prompt_request(
        &mut self,
        event: &Event,
        provider_connection_id: tau_proto::ConnectionId,
    ) {
        let Some(agent_prompt_id) = (match event {
            Event::AgentPromptCreated(prompt) => Some(&prompt.agent_prompt_id),
            _ => None,
        }) else {
            return;
        };
        self.pending_provider_prompts
            .insert(agent_prompt_id.clone(), provider_connection_id);
    }

    fn recover_failed_provider_prompt_route(
        &mut self,
        event: &Event,
        provider_connection_id: &tau_proto::ConnectionId,
        reason: &str,
    ) {
        let Event::AgentPromptCreated(prompt) = event else {
            return;
        };
        let agent_prompt_id = prompt.agent_prompt_id.clone();
        let cid = self.prompt_agents.get(&agent_prompt_id).cloned();
        if let Some(cid) = cid.as_ref() {
            let prompt_context = self.agents.get(cid).map(|conv| {
                (
                    conv.session_id.clone(),
                    conv.originator.clone(),
                    conv.agent_id.clone(),
                )
            });
            if let Some((session_id, originator, Some(_))) = prompt_context {
                self.publish_prompt_terminated(
                    session_id,
                    agent_prompt_id.clone(),
                    AgentPromptTerminationReason::Canceled,
                    originator,
                );
            }
        }

        self.prompt_agents.remove(agent_prompt_id.as_str());
        self.pending_provider_prompts.remove(&agent_prompt_id);
        self.clear_prompt_tool_snapshot(&agent_prompt_id);
        if let Some(model) = self.prompt_models.remove(&agent_prompt_id) {
            self.current_session_state.token_usage.total.requests = self
                .current_session_state
                .token_usage
                .total
                .requests
                .saturating_sub(1);
            if let Some(counts) = self
                .current_session_state
                .token_usage
                .by_model
                .get_mut(&model)
            {
                counts.requests = counts.requests.saturating_sub(1);
            }
        }
        if let Some(cid) = cid {
            if let Some(conv) = self.agents.get_mut(&cid) {
                if conv.in_flight_prompt.as_ref() == Some(&agent_prompt_id) {
                    conv.in_flight_prompt = None;
                }
                if conv.last_prompt_id.as_ref() == Some(&agent_prompt_id) {
                    conv.last_prompt_id = None;
                }
            }
            self.set_agent_turn_state(&cid, AgentTurnState::Idle);
        }
        self.emit_harness_failure(&format!(
            "provider prompt route failed for `{agent_prompt_id}` via `{provider_connection_id}`: {reason}"
        ));
        self.try_advance_queue();
    }

    /// Final commit: persist (when applicable), append to the event
    /// log, and broadcast on the bus. Does not consult interception
    /// state — the caller is responsible for getting here only when
    /// the chain has resolved. Triggers any post-commit reactions
    /// (currently: deferred agent dispatches that were waiting on
    /// this user-message-bearing event to land in the tree, plus
    /// per-publish conversation `head` syncs).
    pub(crate) fn commit_event(
        &mut self,
        source: Option<&str>,
        event: Event,
        transient: bool,
        sync_head_for: Option<ConversationHeadSync>,
    ) {
        // When this publish was stamped with a conversation, fold
        // the event onto that agent's branch directly. This
        // skips the `UiNavigateTree` head-bouncing dance that
        // `publish_for_agent_from` used to do — the explicit
        // parent in `apply_event_at` does the same job without
        // touching the global cursor.
        let parent_for_fold = if sync_head_for
            .as_ref()
            .is_some_and(|s| self.agents.get(&s.cid).is_some_and(|c| c.head.is_none()))
        {
            tau_core::AgentEventParent::Root
        } else {
            sync_head_for
                .as_ref()
                .and_then(|s| self.agents.get(&s.cid).and_then(|c| c.head))
                .map(tau_core::AgentEventParent::Under)
                .unwrap_or(tau_core::AgentEventParent::InheritHead)
        };
        // Stamp once and share with every downstream observer: the durable
        // record on disk, the debug JSONL line, and the wire delivery.
        // Sampling the clock separately would let timing analyses
        // disagree with what live subscribers saw.
        let source_id = source.map(tau_proto::ConnectionId::from);
        let (seq, recorded_at) = self.event_log.append();
        #[cfg(test)]
        self.event_log
            .record_for_test(seq, recorded_at, source_id.clone(), event.clone());
        #[cfg(not(test))]
        let _ = seq;
        // Mirror every committed event into the JSONL debug log as a
        // `published` line. The inbound `from_connection` lines carry
        // the raw frame the agent sent us, but for events that the
        // harness enriches (notably `ProviderResponseFinished`, where
        // `token_usage` is built here from session-wide state the
        // agent never sees), the enriched payload only exists on the
        // outbound copy. Offline cache/cost analysis tools that read
        // `events.jsonl` would otherwise see zeros where the running
        // session totals belong.
        if let Some(log) = &mut self.debug_log {
            log.log_published_event(source_id.as_ref(), &event, recorded_at);
        }
        let folded_node_id = match self.persist_semantic_event(
            source,
            &event,
            transient,
            parent_for_fold,
            sync_head_for.as_ref(),
            recorded_at,
        ) {
            Ok(folded_node_id) => folded_node_id,
            Err(error) => {
                tracing::warn!(
                    target: "tau_harness",
                    event = %event.name(),
                    %error,
                    "dropping event rejected by session store"
                );
                self.emit_harness_failure(&format!(
                    "event {} rejected by session store: {error}",
                    event.name()
                ));
                return;
            }
        };
        if let Event::AgentPromptCreated(prompt) = &event {
            self.note_agent_prompt_created(prompt);
        }
        if let Some(sync) = sync_head_for
            && let Some(c) = self.agents.get_mut(&sync.cid)
        {
            match (&event, folded_node_id) {
                (Event::AgentHeadMoved(moved), _) => {
                    c.head = Some(moved.node_id);
                }
                (_, Some(node_id)) => {
                    // Only advance the agent's own branch cursor when
                    // the event produced a tree node. `tree.head()` is the
                    // *global* write cursor and may sit on a sibling
                    // agent's last fold; syncing to it after a
                    // non-folding event (e.g. `ProviderResponseFinished` with
                    // only tool calls) would graft this agent's next
                    // tool request onto the wrong branch and produce orphan
                    // ToolUse blocks downstream.
                    c.head = Some(node_id);
                    // Keep the dedup map's "built for" cursor in lockstep with
                    // the just-folded linear extension. The dedup-decision
                    // path already inserted any new (hash, call_id) entry
                    // before the publish, so the map's contents already match
                    // what a fresh rebuild from this new head would produce.
                    // Bumping the cursor here lets the next tool result skip
                    // the rebuild entirely (the steady-state hot path).
                    //
                    // We pass *every* fold through this hook, including ones
                    // that didn't touch the dedup map (a user message from
                    // session re-init or a message projection).
                    // [`ResultDedupMap::note_head_advanced_to`] guards
                    // against the dangerous case — `built_for == None` plus a
                    // non-dedup-eligible fold — by skipping the bump, so the
                    // rebuild still triggers on the next dedup intake. Don't
                    // gate this call on the event variant: that would re-couple
                    // `commit_event` to per-tool semantics that the dedup
                    // module deliberately owns.
                    c.result_dedup.note_head_advanced_to(node_id);
                }
                _ => {}
            }
        }
        // Wrap in a harness-owned delivery so subscribers get the runtime
        // event-log sequence and can ack after processing.
        let log_frame = HarnessOutputMessage::deliver_live(recorded_at, event.clone());
        if let Some(provider_connection_id) = self.provider_route_for_prompt_request(&event) {
            // Provider-owned prompt execution is point-to-point: observers still
            // see the durable prompt fact, but execution clients do not all race
            // to consume it. The owning provider gets the exact same delivery
            // payload via a directed route so ACK and replay semantics match
            // the subscribed-provider path.
            let execution_kinds = [ClientKind::Provider];
            let _ =
                self.bus
                    .publish_from_excluding_kinds(source, log_frame.clone(), &execution_kinds);
            match self
                .bus
                .send_to(provider_connection_id.as_str(), source, log_frame)
            {
                Ok(report) if !report.delivered_to.is_empty() => {
                    self.track_provider_prompt_request(&event, provider_connection_id);
                }
                Ok(report) => {
                    tracing::warn!(
                        target: "tau_harness",
                        event = %event.name(),
                        provider_connection_id = %provider_connection_id,
                        ?report,
                        "provider prompt route did not deliver"
                    );
                    self.recover_failed_provider_prompt_route(
                        &event,
                        &provider_connection_id,
                        "no provider connection accepted the prompt",
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        target: "tau_harness",
                        event = %event.name(),
                        provider_connection_id = %provider_connection_id,
                        %error,
                        "provider prompt route failed"
                    );
                    self.recover_failed_provider_prompt_route(
                        &event,
                        &provider_connection_id,
                        &error.to_string(),
                    );
                }
            }
        } else {
            let _ = self.bus.publish_from(source, log_frame);
        }
        if let Err(error) = self.dispatch_internal_tool_event(&event) {
            self.emit_harness_failure(&format!("internal tool event handler failed: {error}"));
        }
        self.react_to_committed_event(&event);
    }

    /// Post-commit reactions. Drains the deferred-agent-dispatch
    /// queue when a user-message-bearing event commits, so the
    /// agent prompt assembled in `send_prompt_to_agent_for` sees
    /// the just-folded user message. The `c.head` sync that this
    /// dispatch depends on is handled inside `commit_event` for any
    /// publish stamped via `publish_event_for_agent`.
    fn react_to_committed_event(&mut self, event: &Event) {
        if let Event::AgentMessageReceived(message) = event {
            self.deliver_agent_message(message);
        }
        let folds_user_message = matches!(
            event,
            Event::AgentPromptSubmitted(_)
                | Event::AgentUserMessageInjected(_)
                | Event::AgentPromptSteered(_)
        );
        if !folds_user_message {
            return;
        }
        let Some(cid) = self.pending_user_prompt_dispatches.pop_front() else {
            return;
        };
        if !self.agents.contains_key(&cid) {
            // Agent was torn down while the prompt was in
            // limbo (e.g. side query that timed out).
            return;
        }
        self.dispatch_prompt_after_publish_idle(&cid);
    }

    /// Classify whether a non-user message recipient can receive a hidden
    /// prompt.
    pub(crate) fn agent_message_recipient_status(
        &self,
        recipient_id: &str,
    ) -> AgentMessageRecipientStatus {
        if self.agent_routes.contains_key(recipient_id)
            || self
                .pending_start_agent_requests
                .iter()
                .any(|pending| pending.agent_id == recipient_id)
        {
            AgentMessageRecipientStatus::Live
        } else if self.stopped_agent_ids.contains(recipient_id) {
            AgentMessageRecipientStatus::Stopped
        } else {
            AgentMessageRecipientStatus::Unknown
        }
    }

    fn deliver_agent_message(&mut self, message: &tau_proto::AgentMessageReceived) {
        let escaped_message = escape_agent_message_for_prompt(&message.message);
        let text = match message.kind {
            tau_proto::AgentMessageKind::Message => format!(
                "[tau-internal]: You have received a message from {}\n\n<message>\n{}\n</message>",
                message.sender_id, escaped_message
            ),
            tau_proto::AgentMessageKind::WatchResponse => format!(
                "[tau-internal]: Agent {} finished its turn\n\n<response>\n{}\n</response>",
                message.sender_id, escaped_message
            ),
        };
        if let Some(cid) = self
            .agent_routes
            .get(message.recipient_id.as_str())
            .cloned()
        {
            if let Some(conv) = self.agents.get_mut(&cid) {
                conv.pending_prompts
                    .push_back(PendingPrompt::agent_message_received(text));
            }
            self.interrupt_active_waits_for(&cid);
            self.preempt_queued_tool_calls_for_message_received(&cid);
            self.try_advance_queue();
            return;
        }
        if let Some(pending) = self
            .pending_start_agent_requests
            .iter_mut()
            .find(|pending| pending.agent_id == message.recipient_id.as_str())
        {
            pending
                .pending_agent_messages
                .push_back(PendingPrompt::agent_message_received(text));
        }
    }

    fn preempt_queued_tool_calls_for_message_received(&mut self, cid: &AgentId) {
        let Some(remaining_calls) = self
            .agents
            .get(cid)
            .and_then(|conv| match &conv.turn_state {
                AgentTurnState::ToolsRunning { remaining_calls } => Some(remaining_calls.clone()),
                _ => None,
            })
        else {
            return;
        };
        if self.tool_turn.any_in_flight_for(cid)
            || self.tool_turn.backgrounded_calls_for(cid).is_empty()
        {
            return;
        }

        let remaining: std::collections::HashSet<ToolCallId> =
            remaining_calls.iter().cloned().collect();
        let cancelled = self.tool_turn.cancel_queued_for(cid, &remaining);
        if cancelled.len() != remaining_calls.len() {
            return;
        }
        let cancelled_call_ids: std::collections::HashSet<ToolCallId> = cancelled
            .iter()
            .map(|(call_id, _, _)| call_id.clone())
            .collect();
        self.record_wait_tool_cancelled(&cancelled_call_ids);
        for (call_id, tool_name, tool_type) in cancelled {
            self.publish_for_agent(
                cid,
                Event::ToolCancelled(ToolCancelled {
                    call_id: call_id.clone(),
                    tool_name,
                    tool_type,
                }),
            );
            self.clear_tool_call_tracking(call_id.as_str());
        }
        self.set_agent_turn_state(cid, AgentTurnState::Idle);
    }

    /// Persists `event` to its durable semantic log and folds it into the
    /// corresponding in-memory view. Session membership facts go to the session
    /// log; agent transcript facts go to the owning agent log. Returns the id
    /// of the just-folded agent transcript node, when one was produced.
    fn persist_semantic_event(
        &mut self,
        source: Option<&str>,
        event: &Event,
        transient: bool,
        parent: tau_core::AgentEventParent,
        sync_head_for: Option<&ConversationHeadSync>,
        recorded_at: tau_proto::UnixMicros,
    ) -> Result<Option<tau_proto::NodeId>, HarnessError> {
        if !semantic_event_router::should_persist_event(event, transient) {
            return Ok(None);
        }
        let source = source.map(tau_proto::ConnectionId::from);
        if let Some(session_id) = semantic_event_router::session_membership_id_for_event(event) {
            self.store.append_session_event_at(
                session_id.as_str(),
                source,
                event.clone(),
                recorded_at,
            )?;
            return Ok(None);
        }
        let Some(agent_id) = self
            .agent_id_for_event(event)
            .or_else(|| self.agent_scoped_agent_id_for_event(event, sync_head_for))
        else {
            return Ok(None);
        };
        Ok(self
            .agent_store
            .append_agent_event_at(
                agent_id.as_str(),
                source,
                parent,
                event.clone(),
                recorded_at,
            )?
            .folded_node_id)
    }

    fn agent_scoped_agent_id_for_event(
        &self,
        event: &Event,
        sync_head_for: Option<&ConversationHeadSync>,
    ) -> Option<tau_proto::AgentId> {
        if !matches!(
            event,
            Event::ProviderToolResult(_)
                | Event::ProviderToolError(_)
                | Event::ToolError(_)
                | Event::ToolCancelled(_)
                | Event::ToolBackgroundResult(_)
                | Event::ToolBackgroundError(_)
        ) {
            return None;
        }
        let sync = sync_head_for?;
        sync.agent_id.clone().or_else(|| {
            self.agents
                .get(&sync.cid)?
                .agent_id
                .as_ref()
                .cloned()
                .map(crate::parse_agent_id)
        })
    }

    pub(crate) fn agent_display_name_for_cid(&self, cid: &AgentId) -> Option<String> {
        self.agents.get(cid).and_then(|conv| {
            normalize_display_name(conv.display_name.as_deref())
                .or_else(|| conv.agent_id.as_ref().cloned())
        })
    }

    fn agent_id_for_event(&self, event: &Event) -> Option<tau_proto::AgentId> {
        match event {
            Event::AgentStarted(started) => Some(started.agent_id.clone()),
            Event::AgentDisplayNameSet(name) => Some(name.agent_id.clone()),
            Event::AgentMetadataSet(set) => Some(set.agent_id.clone()),
            Event::AgentMetadataUnset(unset) => Some(unset.agent_id.clone()),
            Event::AgentPromptSubmitted(prompt) => Some(prompt.agent_id.clone()),
            Event::AgentPromptSteered(prompt) => Some(prompt.agent_id.clone()),
            Event::AgentCompactionTriggered(triggered) => Some(triggered.agent_id.clone()),
            Event::AgentUserMessageInjected(injected) => Some(injected.agent_id.clone()),
            Event::AgentMessageSent(message) => Some(message.sender_id.clone()),
            Event::AgentMessageReceived(message) => Some(message.recipient_id.clone()),
            Event::AgentHeadMoved(moved) => Some(moved.agent_id.clone()),
            Event::ProviderResponseFinished(finished) => Some(finished.agent_id.clone()),
            Event::ProviderToolResult(result) => self
                .tool_agents
                .get(&result.call_id)
                .and_then(|cid| self.agents.get(cid))
                .and_then(|conv| conv.agent_id.as_ref())
                .cloned()
                .map(crate::parse_agent_id),
            Event::ProviderToolError(error) | Event::ToolError(error) => self
                .tool_agents
                .get(&error.call_id)
                .and_then(|cid| self.agents.get(cid))
                .and_then(|conv| conv.agent_id.as_ref())
                .cloned()
                .map(crate::parse_agent_id),
            Event::ToolBackgroundResult(result) => self
                .tool_agents
                .get(&result.call_id)
                .and_then(|cid| self.agents.get(cid))
                .and_then(|conv| conv.agent_id.as_ref())
                .cloned()
                .map(crate::parse_agent_id),
            Event::ToolBackgroundError(error) => self
                .tool_agents
                .get(&error.call_id)
                .and_then(|cid| self.agents.get(cid))
                .and_then(|conv| conv.agent_id.as_ref())
                .cloned()
                .map(crate::parse_agent_id),
            _ => None,
        }
    }

    fn enable_debug_log(&mut self, dir: &Path) -> Result<PathBuf, HarnessError> {
        let log = DebugEventLog::open(dir)?;
        let path = log.path().to_path_buf();
        self.debug_log = Some(log);
        Ok(path)
    }

    // -----------------------------------------------------------------------
    // Startup
    // -----------------------------------------------------------------------

    /// Drives the event loop until the in-flight session initialization
    /// completes (turn state returns to `Idle`). Called at harness
    /// startup after the eager `start_session_init` for the default
    /// session — see that call site for the design rationale.
    fn wait_for_session_init(&mut self) -> Result<(), HarnessError> {
        if self.turn_state.is_idle() {
            return Ok(());
        }
        let started_at = Instant::now();
        while !self.turn_state.is_idle() {
            let remaining = STARTUP_TIMEOUT
                .checked_sub(started_at.elapsed())
                .unwrap_or(Duration::ZERO);
            let harness_evt = self
                .rx
                .recv_timeout(remaining)
                .map_err(|_| HarnessError::StartupTimeout)?;
            self.log_event(&harness_evt);
            match harness_evt {
                HarnessEvent::FromConnection {
                    connection_id,
                    message,
                } => {
                    let _ = self.handle_startup_from_connection(&connection_id, *message)?;
                }
                HarnessEvent::Disconnected { connection_id } => {
                    self.handle_startup_disconnect(&connection_id)?;
                }
                HarnessEvent::NewClient(_) => {}
                HarnessEvent::Command(command) => self.handle_harness_command(command)?,
            }
        }
        Ok(())
    }

    /// Drives the event loop until every configured extension reaches
    /// `ExtensionState::Ready`. Replaces the old `wait_for_startup(n)`:
    /// state transitions are tracked per-extension so the same predicate
    /// can also gate runtime dispatch in `dispatch_blocked_for`.
    fn wait_for_extensions_ready(&mut self) -> Result<(), HarnessError> {
        if self.extensions.pending_connects == 0 && self.extensions_all_ready() {
            return Ok(());
        }
        let started_at = Instant::now();
        while self.extensions.pending_connects != 0 || !self.extensions_all_ready() {
            let remaining = STARTUP_TIMEOUT
                .checked_sub(started_at.elapsed())
                .unwrap_or(Duration::ZERO);
            let harness_evt = match self.rx.recv_timeout(remaining) {
                Ok(event) => event,
                Err(_) => return self.handle_extensions_startup_timeout(),
            };
            self.log_event(&harness_evt);
            match harness_evt {
                HarnessEvent::FromConnection {
                    connection_id,
                    message,
                } => {
                    let _ = self.handle_startup_from_connection(&connection_id, *message)?;
                }
                HarnessEvent::Disconnected { connection_id } => {
                    self.handle_startup_disconnect(&connection_id)?;
                }
                HarnessEvent::NewClient(_) => {}
                HarnessEvent::Command(command) => self.handle_harness_command(command)?,
            }
        }
        Ok(())
    }

    fn handle_extensions_startup_timeout(&mut self) -> Result<(), HarnessError> {
        let blockers: Vec<_> = self
            .extensions
            .entries
            .iter()
            .filter(|(_, entry)| {
                !matches!(
                    entry.state,
                    ExtensionState::Ready | ExtensionState::Disconnected
                )
            })
            .map(|(connection_id, entry)| {
                (connection_id.to_string(), entry.name.clone(), entry.require)
            })
            .collect();
        let required_blockers: Vec<_> = blockers
            .iter()
            .filter_map(|(_, name, require)| require.then_some(name.as_str()))
            .collect();
        if !required_blockers.is_empty() {
            self.emit_info_important(&format!(
                "startup timed out waiting for required extension(s): {}",
                required_blockers.join(", ")
            ));
            return Err(HarnessError::StartupTimeout);
        }

        for (connection_id, name, require) in blockers {
            if require {
                continue;
            }
            tracing::warn!(
                target: "tau_harness::startup",
                extension = %name,
                "optional extension did not initialize: timed out before becoming ready"
            );
            self.disable_optional_extension(
                connection_id.as_str(),
                &format!("optional extension {name} did not initialize"),
            );
        }

        if self.extensions.pending_connects == 0 && self.extensions_all_ready() {
            Ok(())
        } else {
            Err(HarnessError::StartupTimeout)
        }
    }

    // -----------------------------------------------------------------------
    // Main event loop (daemon mode)
    // -----------------------------------------------------------------------

    pub(crate) fn run_event_loop(
        &mut self,
        max_clients: Option<usize>,
        mut exit_on_disconnect: bool,
    ) -> Result<(), HarnessError> {
        let mut served_clients = 0_usize;
        if self.startup_detach_requested {
            exit_on_disconnect = false;
        }
        let mut ever_attached = !self.client_writers.is_empty();
        loop {
            if max_clients.is_some_and(|max| served_clients >= max) {
                break;
            }
            // `exit_on_disconnect`: once at least one UI has been
            // attached, exiting the moment the last one leaves lets
            // `tau` behave like a normal foreground command.
            // Before any UI attaches we wait — otherwise a slightly
            // late first connect would race us into immediate exit.
            if exit_on_disconnect && ever_attached && self.client_writers.is_empty() {
                break;
            }
            self.process_background_deadlines();
            let harness_evt = if let Some(deadline) = self.tool_turn.next_background_deadline() {
                let timeout = deadline.saturating_duration_since(Instant::now());
                match self.rx.recv_timeout(timeout) {
                    Ok(event) => event,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        self.process_background_deadlines();
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            } else {
                let Ok(event) = self.rx.recv() else {
                    break;
                };
                event
            };
            self.log_event(&harness_evt);
            match harness_evt {
                HarnessEvent::FromConnection {
                    connection_id,
                    message,
                } => {
                    let origin = self
                        .bus
                        .connection(&connection_id)
                        .map(|m| m.origin.clone());
                    match origin {
                        Some(ConnectionOrigin::Socket) => {
                            // `/detach` → stay alive even after this UI leaves;
                            // a later `tau --attach` can pick up right here.
                            if matches!(
                                message.as_ref(),
                                HarnessInputMessage::Emit(emit)
                                    if matches!(emit.event.as_ref(), Event::UiDetachRequest(_))
                            ) {
                                exit_on_disconnect = false;
                            }
                            let keep = self.handle_client_message(&connection_id, *message)?;
                            if !keep {
                                self.handle_disconnect(&connection_id);
                                served_clients += 1;
                            }
                        }
                        Some(_) => self.handle_extension_message(&connection_id, *message)?,
                        None => {}
                    }
                }
                HarnessEvent::Disconnected { connection_id } => {
                    let was_provider = self.is_provider_extension(&connection_id);
                    let was_socket = self
                        .bus
                        .connection(&connection_id)
                        .is_some_and(|m| m.origin == ConnectionOrigin::Socket);
                    self.handle_disconnect(&connection_id);
                    if was_socket {
                        served_clients += 1;
                    }
                    if was_provider {
                        return Err(provider_disconnected_error());
                    }
                }
                HarnessEvent::NewClient(stream) => {
                    self.accept_client(stream)?;
                    ever_attached = true;
                }
                HarnessEvent::Command(command) => self.handle_harness_command(command)?,
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Client acceptance
    // -----------------------------------------------------------------------

    pub(crate) fn accept_client(
        &mut self,
        stream: UnixStream,
    ) -> Result<ConnectionId, HarnessError> {
        let write_stream = stream.try_clone()?;
        self.accept_client_io(stream, write_stream, ConnectionOrigin::Socket)
    }

    pub(crate) fn accept_stdio_client(&mut self) -> Result<ConnectionId, HarnessError> {
        self.accept_client_io(io::stdin(), io::stdout(), ConnectionOrigin::Socket)
    }

    fn accept_client_io<R, W>(
        &mut self,
        read: R,
        write: W,
        origin: ConnectionOrigin,
    ) -> Result<ConnectionId, HarnessError>
    where
        R: io::Read + Send + 'static,
        W: io::Write + Send + 'static,
    {
        let writer_tx = spawn_writer_thread(write, WriterShutdown::CloseStream);
        let writer_tx_for_follower = writer_tx.clone();
        let conn_id = self.bus.connect(Connection::new(
            ConnectionMetadata {
                id: tau_proto::ConnectionId::default(),
                name: "socket-ui".to_owned(),
                kind: ClientKind::Ui,
                origin,
            },
            Box::new(ChannelSink { tx: writer_tx }),
        ));
        self.client_writers
            .insert(conn_id.clone(), writer_tx_for_follower);
        spawn_reader_thread(conn_id.clone(), read, self.tx.clone());
        Ok(conn_id)
    }

    pub(crate) fn send_startup_disconnect_to_initial_client(
        &mut self,
        client_id: Option<&ConnectionId>,
        error: &dyn std::fmt::Display,
    ) {
        let Some(client_id) = client_id else {
            return;
        };
        let _ = self.bus.send_to(
            client_id.as_str(),
            None,
            HarnessOutputMessage::Disconnect(Disconnect {
                reason: Some(format!("harness startup failed: {error}")),
            }),
        );
        self.flush_initial_client_writer(client_id);
    }

    fn flush_initial_client_writer(&mut self, client_id: &ConnectionId) {
        let Some(writer) = self.client_writers.get(client_id) else {
            return;
        };
        let (ack_tx, ack_rx) = mpsc::channel();
        if writer.send(WriterCommand::Flush(ack_tx)).is_ok() {
            let _ = ack_rx.recv_timeout(Duration::from_secs(2));
        }
    }

    // -----------------------------------------------------------------------
    // Event handlers
    // -----------------------------------------------------------------------

    fn send_agent_prompt_created_result(
        &mut self,
        connection_id: &str,
        request: tau_proto::GetAgentPromptCreated,
    ) {
        let _ = self.bus.send_to(
            connection_id,
            None,
            HarnessOutputMessage::AgentPromptCreatedResult(Box::new(
                tau_proto::AgentPromptCreatedResult {
                    request_id: request.request_id,
                    prompt: None,
                },
            )),
        );
    }

    fn send_rendered_system_prompt_result(
        &mut self,
        connection_id: &str,
        request: tau_proto::GetRenderedSystemPrompt,
    ) {
        let (prompt, error) = if !self.available_roles.contains_key(&request.role) {
            (None, Some(format!("unknown role: {}", request.role)))
        } else {
            (
                Some(self.build_system_prompt_for_role_preview(&request.role)),
                None,
            )
        };
        let _ = self.bus.send_to(
            connection_id,
            None,
            HarnessOutputMessage::RenderedSystemPromptResult(Box::new(
                tau_proto::RenderedSystemPromptResult {
                    request_id: request.request_id,
                    prompt,
                    error,
                },
            )),
        );
    }

    fn send_rendered_prompt_result(
        &mut self,
        connection_id: &str,
        request: tau_proto::GetRenderedPrompt,
    ) {
        let (prompt, error) = if !self.available_roles.contains_key(&request.role) {
            (None, Some(format!("unknown role: {}", request.role)))
        } else {
            let system_prompt = self.build_system_prompt_for_role_preview(&request.role);
            let agents_context =
                if request.enable_agents_md && !self.discovered_agents_files.is_empty() {
                    Some(render_agents_context_message(
                        self.discovered_agents_files.iter(),
                    ))
                } else {
                    None
                };
            (
                Some(render_effective_prompt_message(
                    &system_prompt,
                    agents_context.as_deref(),
                )),
                None,
            )
        };
        let _ = self.bus.send_to(
            connection_id,
            None,
            HarnessOutputMessage::RenderedPromptResult(Box::new(tau_proto::RenderedPromptResult {
                request_id: request.request_id,
                prompt,
                error,
            })),
        );
    }

    fn send_rendered_tool_definitions_result(
        &mut self,
        connection_id: &str,
        request: tau_proto::GetRenderedToolDefinitions,
    ) {
        let (tools, error) = if !self.available_roles.contains_key(&request.role) {
            (None, Some(format!("unknown role: {}", request.role)))
        } else {
            (
                Some(self.gather_tool_definitions_for_role(&request.role)),
                None,
            )
        };
        let _ = self.bus.send_to(
            connection_id,
            None,
            HarnessOutputMessage::RenderedToolDefinitionsResult(Box::new(
                tau_proto::RenderedToolDefinitionsResult {
                    request_id: request.request_id,
                    tools,
                    error,
                },
            )),
        );
    }

    fn send_extension_data_result(
        &mut self,
        connection_id: &str,
        request_id: String,
        result: tau_proto::ExtensionDataResultPayload,
    ) {
        let _ = self.bus.send_to(
            connection_id,
            None,
            HarnessOutputMessage::ExtensionDataResult(Box::new(tau_proto::ExtensionDataResult {
                request_id,
                result,
            })),
        );
    }

    fn handle_extension_data_request(
        &mut self,
        connection_id: &str,
        request: tau_proto::ExtensionDataRequest,
    ) {
        let request_id = request.request_id;
        let result = match self.run_extension_data_request(connection_id, request.scope, request.op)
        {
            Ok(value) => tau_proto::ExtensionDataResultPayload::Ok { value },
            Err(error) => tau_proto::ExtensionDataResultPayload::Error {
                kind: error.kind,
                message: error.message,
            },
        };
        self.send_extension_data_result(connection_id, request_id, result);
    }

    fn run_extension_data_request(
        &self,
        connection_id: &str,
        scope: tau_proto::ExtensionDataScope,
        op: tau_proto::ExtensionDataRequestOp,
    ) -> Result<tau_proto::ExtensionDataValue, ExtensionDataError> {
        let root = self
            .extension_data_scope_root(connection_id, scope)
            .map_err(|message| {
                ExtensionDataError::new(tau_proto::ExtensionDataErrorKind::Io, message)
            })?;
        match op {
            tau_proto::ExtensionDataRequestOp::ReadFile { path } => {
                run_extension_data_read_file(&root, path.into_string())
            }
            tau_proto::ExtensionDataRequestOp::WriteFile { path, contents } => {
                run_extension_data_write_file(&root, path.into_string(), contents)
            }
            tau_proto::ExtensionDataRequestOp::CreateFile { path, contents } => {
                run_extension_data_create_file(&root, path.into_string(), contents)
            }
            tau_proto::ExtensionDataRequestOp::AppendFile { path, contents } => {
                run_extension_data_append_file(&root, path.into_string(), contents)
            }
            tau_proto::ExtensionDataRequestOp::DeleteFile { path } => {
                run_extension_data_delete_file(&root, path.into_string())
            }
            tau_proto::ExtensionDataRequestOp::RenameFile { from, to } => {
                run_extension_data_rename_file(&root, from.into_string(), to.into_string())
            }
            tau_proto::ExtensionDataRequestOp::ListFiles { path } => {
                run_extension_data_list_files(&root, path.into_string())
            }
        }
    }

    fn extension_data_scope_root(
        &self,
        connection_id: &str,
        scope: tau_proto::ExtensionDataScope,
    ) -> Result<PathBuf, String> {
        let name = self
            .extensions
            .entries
            .get(connection_id)
            .map(|entry| entry.name.as_str())
            .ok_or_else(|| "unknown extension connection".to_owned())?;
        tau_config::settings::validate_extension_name(name).map_err(|error| error.to_string())?;
        match scope {
            tau_proto::ExtensionDataScope::Session => {
                Ok(tau_config::settings::sessions_dir_of(&self.state_dir)
                    .join(self.current_session_id.as_str())
                    .join("ext")
                    .join("data")
                    .join(name))
            }
            tau_proto::ExtensionDataScope::User => {
                tau_config::settings::extension_state_dir_of(&self.state_dir, name)
                    .map_err(|error| error.to_string())
            }
            tau_proto::ExtensionDataScope::Cache => dirs::cache_dir()
                .map(|dir| dir.join("tau").join("ext").join(name))
                .ok_or_else(|| "could not determine user cache directory".to_owned()),
        }
    }

    fn should_stage_extension_capabilities(&self, source_id: &str) -> bool {
        self.extensions
            .entries
            .get(source_id)
            .is_some_and(|entry| entry.state != ExtensionState::Ready)
    }

    fn extension_activation_stage_mut(&mut self, source_id: &str) -> &mut ExtensionActivationStage {
        self.extensions
            .activation_staging
            .entry(source_id.into())
            .or_default()
    }

    fn stage_extension_tool_registration(&mut self, source_id: &str, registration: ToolRegister) {
        self.extension_activation_stage_mut(source_id)
            .tool_registrations
            .push(registration);
    }

    fn remove_staged_tool_registration(&mut self, source_id: &str, tool_name: &ToolName) -> bool {
        let Some(stage) = self.extensions.activation_staging.get_mut(source_id) else {
            return false;
        };
        let before = stage.tool_registrations.len();
        stage
            .tool_registrations
            .retain(|registration| registration.tool.name != *tool_name);
        stage.tool_registrations.len() != before
    }

    fn stage_provider_models_update(
        &mut self,
        source_id: &str,
        update: tau_proto::ProviderModelsUpdated,
    ) {
        self.extension_activation_stage_mut(source_id)
            .provider_model_updates
            .push(update);
    }

    fn stage_extension_skill_available(
        &mut self,
        source_id: &str,
        skill: tau_proto::ExtSkillAvailable,
    ) {
        self.extension_activation_stage_mut(source_id)
            .skill_announcements
            .push(skill);
    }

    fn stage_agents_md_available(
        &mut self,
        source_id: &str,
        agents: tau_proto::ExtAgentsMdAvailable,
    ) {
        self.extension_activation_stage_mut(source_id)
            .agents_files
            .push(agents);
    }

    fn stage_agent_context_provider_register(&mut self, source_id: &str) {
        self.extension_activation_stage_mut(source_id)
            .agent_context_provider_registered = true;
    }

    fn stage_agent_context_publish(
        &mut self,
        source_id: &str,
        publish: tau_proto::ExtAgentContextPublish,
    ) {
        self.extension_activation_stage_mut(source_id)
            .agent_context_publishes
            .push(publish);
    }

    fn stage_extension_prompt_fragment(
        &mut self,
        source_id: &str,
        publish: tau_proto::ExtPromptFragmentPublish,
    ) {
        self.extension_activation_stage_mut(source_id)
            .prompt_fragments
            .insert(publish.fragment.name.clone(), publish.fragment);
    }

    fn stage_extension_intercept(&mut self, source_id: &str, intercept: tau_proto::Intercept) {
        self.extension_activation_stage_mut(source_id).intercept = Some(intercept);
    }

    fn stage_extension_context_ready(
        &mut self,
        source_id: &str,
        ready: tau_proto::ExtensionContextReady,
    ) {
        self.extension_activation_stage_mut(source_id)
            .context_ready_events
            .push(ready);
    }

    fn stage_start_agent_request(&mut self, source_id: &str, query: tau_proto::StartAgentRequest) {
        self.extension_activation_stage_mut(source_id)
            .agent_queries
            .push(query);
    }

    fn stage_extension_publish(&mut self, source_id: &str, event: Event, transient: bool) {
        self.extension_activation_stage_mut(source_id)
            .emitted_events
            .push(StagedExtensionPublish { event, transient });
    }

    fn stage_action_schema(&mut self, source_id: &str, schema: tau_actions::ActionSchema) {
        self.extension_activation_stage_mut(source_id).action_schema = Some(schema);
    }

    fn register_extension_tool(&mut self, source_id: &str, registration: ToolRegister) {
        let internal_name = registration.tool.name.clone();
        let visible_name = self.tool_model_visible_name(&registration.tool).clone();
        let was_available = !self
            .registry
            .providers_for(internal_name.as_str())
            .is_empty();
        let _ = self
            .registry
            .register_with_prompt_fragment(source_id, registration);
        self.ensure_tool_started_subscription(source_id);
        if !was_available {
            self.mark_tool_available_for_notice(internal_name, visible_name);
        }
    }

    fn ensure_tool_started_subscription(&mut self, source_id: &str) {
        let selector = EventSelector::Exact(tau_proto::EventName::TOOL_STARTED);
        let mut selectors = self
            .bus
            .subscriptions(source_id)
            .map_or_else(Vec::new, |s| s.to_vec());
        if selectors.iter().any(|existing| existing == &selector) {
            return;
        }
        selectors.push(selector);
        if let Err(error) = self.bus.set_subscriptions(source_id, selectors) {
            tracing::warn!(
                target: "tau_harness",
                connection_id = %source_id,
                %error,
                "could not subscribe tool provider to tool.started"
            );
        }
    }

    fn register_extension_interceptor(&mut self, source_id: &str, intercept: tau_proto::Intercept) {
        let component_name = self
            .bus
            .connection(source_id)
            .map(|m| ExtensionName::from(m.name.clone()))
            .unwrap_or_else(|| ExtensionName::from(source_id.to_owned()));
        self.interceptors.replace_for_connection(
            source_id,
            component_name,
            intercept.selectors,
            intercept.priority,
        );
    }

    fn publish_extension_prompt_fragment(
        &mut self,
        source_id: &str,
        publish: tau_proto::ExtPromptFragmentPublish,
    ) {
        let contributor = tau_proto::ConnectionId::from(source_id);
        self.extension_prompt_fragments
            .entry(contributor)
            .or_default()
            .insert(publish.fragment.name.clone(), publish.fragment.clone());
        self.publish_event(Some(source_id), Event::ExtPromptFragmentPublish(publish));
    }

    fn publish_extension_skill_available(
        &mut self,
        source_id: &str,
        mut skill: tau_proto::ExtSkillAvailable,
    ) {
        normalize_skill_invocation_policy(&mut skill);
        self.record_discovered_skill(source_id, &skill);
        self.publish_event(Some(source_id), Event::ExtSkillAvailable(skill));
    }

    fn publish_agents_md_available(
        &mut self,
        source_id: &str,
        agents: tau_proto::ExtAgentsMdAvailable,
    ) {
        let file_path = PathBuf::from(&agents.file_path);
        if let Some(existing) = self
            .discovered_agents_files
            .iter_mut()
            .find(|existing| existing.source_id == source_id && existing.file_path == file_path)
        {
            existing.content = agents.content.clone();
        } else {
            self.discovered_agents_files.push(DiscoveredAgentsFile {
                source_id: source_id.into(),
                file_path,
                content: agents.content.clone(),
            });
        }
        self.publish_event(Some(source_id), Event::ExtAgentsMdAvailable(agents));
        let live_agents: Vec<_> = self
            .agents
            .iter()
            .filter_map(|(cid, agent)| {
                agent
                    .agent_id
                    .as_ref()
                    .map(|agent_id| (cid.clone(), agent_id.clone()))
            })
            .collect();
        for (cid, agent_id) in live_agents {
            self.insert_agents_context_for_agent(&cid, &agent_id);
        }
    }

    fn publish_provider_models_update(
        &mut self,
        source_id: &str,
        update: tau_proto::ProviderModelsUpdated,
    ) {
        self.publish_event(
            Some(source_id),
            Event::ProviderModelsUpdated(update.clone()),
        );
        self.set_provider_models(source_id, update.models);
    }

    fn extension_action_owner(
        &self,
        source_id: &str,
    ) -> (ExtensionName, tau_proto::ExtensionInstanceId) {
        if let Some(extension) = self.extensions.entries.get(source_id) {
            return (
                ExtensionName::from(extension.name.clone()),
                extension.instance_id,
            );
        }
        self.bus.connection(source_id).map_or_else(
            || (ExtensionName::from(source_id.to_owned()), 0.into()),
            |metadata| (ExtensionName::from(metadata.name.clone()), 0.into()),
        )
    }

    fn publish_action_schema(&mut self, source_id: &str, schema: tau_actions::ActionSchema) {
        let (extension_name, instance_id) = self.extension_action_owner(source_id);
        if let Err(error) = self.action_registry.register_schema(
            source_id,
            extension_name.clone(),
            instance_id,
            schema.clone(),
        ) {
            self.emit_harness_failure(&format!(
                "extension {extension_name} published invalid action schema: {error}"
            ));
            return;
        }
        self.publish_event(
            Some(source_id),
            Event::ActionSchemaPublished(ActionSchemaPublished {
                extension_name,
                instance_id,
                schema,
            }),
        );
    }

    fn register_agent_context_provider(&mut self, source_id: &str) {
        self.agent_context_providers
            .insert(tau_proto::ConnectionId::from(source_id));
        self.publish_event(
            Some(source_id),
            Event::ExtensionContextProviderRegister(tau_proto::ExtensionContextProviderRegister {}),
        );
    }

    fn publish_agent_context_publish(
        &mut self,
        source_id: &str,
        publish: tau_proto::ExtAgentContextPublish,
    ) {
        let contributor = tau_proto::ConnectionId::from(source_id);
        let extension_name = self
            .extensions
            .entries
            .get(&contributor)
            .map(|entry| entry.name.clone())
            .unwrap_or_else(|| source_id.to_owned());
        self.agent_context.publish(
            publish.agent_id.clone(),
            publish.key.clone(),
            contributor,
            extension_name,
            publish.value.clone(),
        );
        self.publish_event(Some(source_id), Event::ExtAgentContextPublish(publish));
    }

    fn publish_extension_context_ready(
        &mut self,
        source_id: &str,
        ready: tau_proto::ExtensionContextReady,
    ) -> Result<(), HarnessError> {
        self.publish_event(Some(source_id), Event::ExtensionContextReady(ready.clone()));
        self.handle_extension_context_ready(source_id, ready)
    }

    fn activate_staged_extension_capabilities(
        &mut self,
        source_id: &str,
    ) -> (
        Vec<tau_proto::ExtensionContextReady>,
        Vec<tau_proto::StartAgentRequest>,
    ) {
        let Some(stage) = self.extensions.activation_staging.remove(source_id) else {
            return (Vec::new(), Vec::new());
        };
        if let Some(intercept) = stage.intercept {
            self.register_extension_interceptor(source_id, intercept);
        }
        for registration in stage.tool_registrations {
            self.register_extension_tool(source_id, registration);
        }
        for update in stage.provider_model_updates {
            self.publish_provider_models_update(source_id, update);
        }
        if let Some(schema) = stage.action_schema {
            self.publish_action_schema(source_id, schema);
        }
        for skill in stage.skill_announcements {
            self.publish_extension_skill_available(source_id, skill);
        }
        for agents in stage.agents_files {
            self.publish_agents_md_available(source_id, agents);
        }
        if stage.agent_context_provider_registered {
            self.register_agent_context_provider(source_id);
        }
        for publish in stage.agent_context_publishes {
            self.publish_agent_context_publish(source_id, publish);
        }
        for fragment in stage.prompt_fragments.into_values() {
            self.publish_extension_prompt_fragment(
                source_id,
                tau_proto::ExtPromptFragmentPublish { fragment },
            );
        }
        for staged in stage.emitted_events {
            self.enqueue_publish(Some(source_id), staged.event, staged.transient, false, None);
        }
        (stage.context_ready_events, stage.agent_queries)
    }

    fn handle_extension_message(
        &mut self,
        source_id: &str,
        message: impl Into<HarnessInputMessage>,
    ) -> Result<(), HarnessError> {
        let message = message.into();
        match message {
            HarnessInputMessage::Hello(hello) => {
                validate_protocol_version(&hello)?;
                self.set_extension_state(source_id, ExtensionState::Handshaking);
                self.send_lifecycle_configure(source_id);
            }
            HarnessInputMessage::ConfigError(err) => {
                let name = self
                    .extensions
                    .entries
                    .get(source_id)
                    .map(|e| e.name.clone())
                    .unwrap_or_else(|| "extension".to_owned());
                let optional = self
                    .extensions
                    .entries
                    .get(source_id)
                    .is_some_and(|entry| !entry.require);
                // This is the last line of defense for every extension's typed
                // configuration schema. Do not downgrade, drop, or make this
                // startup-only: invalid extension config must be visible in the
                // UI even when it is reported before any UI client subscribes.
                self.emit_notice(
                    tau_proto::notice_kind::EXTENSION_CONFIG_ERROR,
                    tau_proto::NoticeLevel::Warning,
                    true,
                    &format!(
                        "extension {name} rejected its config: {}\ncheck \
                         `extensions.{name}.config` and `extensions.{name}.secrets` in harness.yaml; \
                         invalid values are being ignored",
                        err.message,
                    ),
                );
                if optional {
                    tracing::warn!(
                        target: "tau_harness::startup",
                        extension = %name,
                        error = %err.message,
                        "optional extension did not initialize: rejected config"
                    );
                    self.disable_optional_extension(
                        source_id,
                        &format!("optional extension {name} did not initialize"),
                    );
                }
            }
            HarnessInputMessage::Subscribe(subscribe) => {
                // Extensions get the same subscribe-time catch-up as UI
                // clients: current-state announcements plus selector-matched
                // durable facts as replay-marked frames. Side-effecting
                // extensions must skip replay frames instead of being
                // protected by withheld delivery.
                self.complete_subscription(source_id, subscribe.selectors)
                    .map_err(HarnessError::Route)?;
            }
            HarnessInputMessage::Intercept(intercept) => {
                if self.should_stage_extension_capabilities(source_id) {
                    self.stage_extension_intercept(source_id, intercept);
                } else {
                    self.register_extension_interceptor(source_id, intercept);
                }
            }
            HarnessInputMessage::Ready(_ready) => {
                let (context_ready_events, agent_queries) =
                    self.activate_staged_extension_capabilities(source_id);
                self.set_extension_state(source_id, ExtensionState::Ready);
                self.emit_extension_ready(source_id);
                for ready in context_ready_events {
                    self.publish_extension_context_ready(source_id, ready)?;
                }
                for query in agent_queries {
                    self.handle_start_agent_request(source_id, query)?;
                }
                self.drain_pending_tool_invocations()?;
                self.try_advance_queue();
            }
            HarnessInputMessage::Emit(emit) => {
                let (event, transient) = emit.into_parts();
                self.handle_extension_event_inner_with_transient(
                    source_id,
                    event,
                    Some(transient),
                )?;
            }
            HarnessInputMessage::InterceptReply(reply) => {
                self.handle_intercept_reply(source_id, reply);
            }
            HarnessInputMessage::GetAgentPromptCreated(request) => {
                self.send_agent_prompt_created_result(source_id, request);
            }
            HarnessInputMessage::ExtensionDataRequest(request) => {
                self.handle_extension_data_request(source_id, request);
            }
            // Messages sent by clients only — extensions shouldn't round-trip
            // these. Ignore silently.
            HarnessInputMessage::Disconnect(_)
            | HarnessInputMessage::GetRenderedSystemPrompt(_)
            | HarnessInputMessage::GetRenderedPrompt(_)
            | HarnessInputMessage::GetRenderedToolDefinitions(_) => {}
        }
        Ok(())
    }

    #[cfg(test)]
    fn handle_extension_event_inner(
        &mut self,
        source_id: &str,
        event: Event,
    ) -> Result<(), HarnessError> {
        self.handle_extension_event_inner_with_transient(source_id, event, None)
    }

    fn handle_extension_event_inner_with_transient(
        &mut self,
        source_id: &str,
        event: Event,
        transient_override: Option<bool>,
    ) -> Result<(), HarnessError> {
        let event_name = event.name();
        if event_name.category() == &tau_proto::EventCategory::Provider
            && !self.accepts_provider_event_from(source_id, &event_name)
        {
            return Ok(());
        }

        match event {
            Event::ToolRegister(registration) => {
                if self.should_stage_extension_capabilities(source_id) {
                    self.stage_extension_tool_registration(source_id, registration);
                } else {
                    self.register_extension_tool(source_id, registration);
                }
            }
            Event::ActionSchemaPublished(published) => {
                if self.should_stage_extension_capabilities(source_id) {
                    self.stage_action_schema(source_id, published.schema);
                } else {
                    self.publish_action_schema(source_id, published.schema);
                }
            }
            Event::ActionResult(result) => {
                self.handle_action_result(source_id, result);
            }
            Event::ActionError(error) => {
                self.handle_action_error(source_id, error);
            }
            Event::ActionInvoke(_) => {}
            Event::ToolUnregister(unregister) => {
                self.remove_staged_tool_registration(source_id, &unregister.tool_name);
                if self.should_stage_extension_capabilities(source_id) {
                    return Ok(());
                }
                let visible_name = self
                    .registry
                    .providers_for(unregister.tool_name.as_str())
                    .into_iter()
                    .find(|provider| provider.connection_id.as_str() == source_id)
                    .map(|provider| self.tool_model_visible_name(&provider.tool).clone())
                    .unwrap_or_else(|| unregister.tool_name.clone());
                let removed = self
                    .registry
                    .unregister(source_id, unregister.tool_name.as_str());
                if removed
                    && self
                        .registry
                        .providers_for(unregister.tool_name.as_str())
                        .is_empty()
                {
                    self.mark_tool_unavailable_for_notice(
                        unregister.tool_name.clone(),
                        visible_name,
                    );
                }
                self.publish_event(Some(source_id), Event::ToolUnregister(unregister));
            }
            Event::ToolRequest(request) => {
                if let Some(message) = self.extension_tool_request_rejection(&request) {
                    self.reject_extension_tool_request(message);
                    return Ok(());
                }
                // Track extension-originated runtime metadata before
                // publishing so terminal events can be attributed and enriched.
                self.track_extension_tool_request_metadata(&request);
                // Publish with the owning agent when known so live observers
                // and runtime delivery see the same agent attribution used for
                // later terminal tool facts. `ToolRequest` itself remains a
                // runtime routing intent, not an agent-transcript fold.
                let owning_cid = self.tool_agents.get(&request.call_id).cloned();
                if let Some(cid) = owning_cid.as_ref()
                    && !self.pending_tools.contains_key(&request.call_id)
                {
                    self.pending_tools.insert(
                        request.call_id.clone(),
                        PendingTool {
                            name: request.tool_name.clone(),
                            internal_name: request.tool_name.clone(),
                            tool_type: request.tool_type,
                        },
                    );
                    self.bump_tools_started_for(cid);
                }
                let event = Event::ToolRequest(request.clone());
                match owning_cid.as_ref() {
                    Some(cid) => self.publish_event_for_agent(cid, Some(source_id), event),
                    None => self.publish_event(Some(source_id), event),
                }
                // `ToolRequest` is the runtime pre-routing intent.
                // `route_tool_request` resolves it. On success we publish
                // `ToolStarted`; subscribed tool extensions see that event and
                // the owner starts work. On
                // failure we publish `ToolRejected` and the terminal
                // `ToolError` for model-facing completion.
                match self.registry.route_tool_request(request.clone()) {
                    Ok(route) => {
                        let started = route.invoke;
                        let event = Event::ToolStarted(started.clone());
                        match owning_cid.as_ref() {
                            Some(cid) => self.publish_for_agent_from(cid, Some(source_id), event),
                            None => self.publish_event(Some(source_id), event),
                        }
                        match route.target {
                            ToolRouteTarget::Internal => {}
                            ToolRouteTarget::Extension(provider_connection_id) => {
                                self.ensure_tool_started_subscription(&provider_connection_id);
                                self.pending_tool_providers
                                    .insert(request.call_id.clone(), provider_connection_id);
                            }
                        }
                    }
                    Err(ToolRouteError::NoProvider { tool_name }) => {
                        let call_id = request.call_id.to_string();
                        let owning_cid = self.tool_agents.get(&request.call_id).cloned();
                        let message = unavailable_tool_error_message(&tool_name);
                        let rejected = ToolRejected {
                            call_id: request.call_id.clone(),
                            tool_name: tool_name.clone(),
                            tool_type: request.tool_type,
                            message: message.clone(),
                            originator: request.originator.clone(),
                        };
                        let event = Event::ToolRejected(rejected);
                        match owning_cid.as_ref() {
                            Some(cid) => self.publish_for_agent_from(cid, Some(source_id), event),
                            None => self.publish_event(Some(source_id), event),
                        }
                        let error = ToolError {
                            call_id: request.call_id,
                            tool_name: tool_name.clone(),
                            tool_type: request.tool_type,
                            message,
                            details: None,
                            originator: tau_proto::PromptOriginator::User,

                            display: None,
                        };
                        self.publish_terminal_tool_error(owning_cid.as_ref(), None, error);
                        self.clear_tool_call_tracking(&call_id);
                    }
                    Err(error) => return Err(HarnessError::ToolRoute(error)),
                }
            }
            Event::ToolResult(mut result) => {
                if !self.validate_tool_event_source(&result.call_id, source_id) {
                    return Ok(());
                }
                if self.tool_turn.is_backgrounded(&result.call_id) {
                    self.handle_background_tool_result(source_id, result);
                } else if let Some(cid) = self.tool_agents.get(&result.call_id).cloned() {
                    let call_id = result.call_id.to_string();
                    if let Some(tool) = self.pending_tools.get(&result.call_id) {
                        result.tool_name = tool.name.clone();
                        result.tool_type = tool.tool_type;
                    }
                    // Collapse byte-identical large results into a
                    // pointer back to the first call_id that produced
                    // this content on this agent's branch. See
                    // `crate::dedup` for the design.
                    self.dedup_tool_result(&cid, &mut result);
                    // Snap to the owning agent's head before
                    // folding the result. Without this, a sibling side
                    // conv that just touched the parent agent
                    // (during its teardown) leaves `tree.head` on the
                    // *parent* branch — folding the result there
                    // misplaces it and produces orphan ToolUse blocks
                    // when the parent conv is later re-prompted.
                    self.publish_terminal_tool_result(Some(&cid), Some(source_id), result);
                    self.on_tool_call_complete(&call_id);
                    self.clear_tool_call_tracking(&call_id);
                } else {
                    self.emit_info(&format!(
                        "discarding duplicate tool result for call_id={}",
                        result.call_id
                    ));
                }
            }
            Event::ToolError(mut error) => {
                if !self.validate_tool_event_source(&error.call_id, source_id) {
                    return Ok(());
                }
                if self.tool_turn.is_backgrounded(&error.call_id) {
                    self.handle_background_tool_error(Some(source_id), error);
                } else if let Some(cid) = self.tool_agents.get(&error.call_id).cloned() {
                    let call_id = error.call_id.to_string();
                    if let Some(tool) = self.pending_tools.get(&error.call_id) {
                        error.tool_name = tool.name.clone();
                        error.tool_type = tool.tool_type;
                    }
                    self.dedup_tool_error(&cid, &mut error);
                    self.publish_terminal_tool_error(Some(&cid), Some(source_id), error);
                    self.on_tool_call_complete(&call_id);
                    self.clear_tool_call_tracking(&call_id);
                } else {
                    self.emit_info(&format!(
                        "discarding duplicate tool error for call_id={}",
                        error.call_id
                    ));
                }
            }
            Event::ToolProgress(progress) => {
                if !self.tool_agents.contains_key(&progress.call_id)
                    || !self.validate_tool_event_source(&progress.call_id, source_id)
                {
                    return Ok(());
                }
                if !self.tool_turn.is_backgrounded(&progress.call_id) {
                    self.publish_event(Some(source_id), Event::ToolProgress(progress));
                }
            }
            // Keep this peer-authored event rejection in sync with
            // `is_peer_forbidden_harness_fact` and the immutable/must-pass
            // classifications in `harness/interception.rs`.
            Event::ProviderToolResult(_)
            | Event::ProviderToolError(_)
            | Event::SessionStarted(_)
            | Event::SessionShutdown(_)
            | Event::SessionAgentLoaded(_)
            | Event::SessionAgentUnloaded(_)
            | Event::AgentStarted(_)
            | Event::AgentMessageSent(_)
            | Event::AgentMessageReceived(_) => {
                return Ok(());
            }
            Event::ToolCancelled(mut cancelled) => {
                if !self.validate_tool_event_source(&cancelled.call_id, source_id) {
                    return Ok(());
                }
                if self.tool_turn.is_backgrounded(&cancelled.call_id) {
                    self.handle_background_tool_cancelled(source_id, cancelled);
                } else if let Some(cid) = self.tool_agents.get(&cancelled.call_id).cloned() {
                    let call_id = cancelled.call_id.to_string();
                    if let Some(tool) = self.pending_tools.get(&cancelled.call_id) {
                        cancelled.tool_name = tool.name.clone();
                        cancelled.tool_type = tool.tool_type;
                    }
                    self.publish_for_agent_from(
                        &cid,
                        Some(source_id),
                        Event::ToolCancelled(cancelled),
                    );
                    self.on_tool_call_complete(&call_id);
                    self.clear_tool_call_tracking(&call_id);
                }
            }
            Event::ToolBackgroundResult(_) | Event::ToolBackgroundError(_)
                if source_id != HARNESS_CONNECTION_ID =>
            {
                return Ok(());
            }
            Event::ShellCommandProgress(progress) => {
                // Pass-through: the UI renders chunks as they arrive.
                self.publish_event(Some(source_id), Event::ShellCommandProgress(progress));
            }
            Event::ShellCommandFinished(finished) => {
                // Publish first so the UI finalizes its render block
                // regardless of whether we inject into history.
                self.publish_event(
                    Some(source_id),
                    Event::ShellCommandFinished(finished.clone()),
                );
                if finished.include_in_context {
                    self.inject_user_shell_output(&finished);
                }
            }
            Event::ExtSkillAvailable(skill) => {
                if self.should_stage_extension_capabilities(source_id) {
                    self.stage_extension_skill_available(source_id, skill);
                } else {
                    self.publish_extension_skill_available(source_id, skill);
                }
            }
            Event::ExtAgentsMdAvailable(agents) => {
                if self.should_stage_extension_capabilities(source_id) {
                    self.stage_agents_md_available(source_id, agents);
                } else {
                    self.publish_agents_md_available(source_id, agents);
                }
            }
            Event::ProviderModelsUpdated(updated) => {
                if self.should_stage_extension_capabilities(source_id) {
                    self.stage_provider_models_update(source_id, updated);
                } else {
                    self.publish_provider_models_update(source_id, updated);
                }
            }
            Event::ExtensionContextProviderRegister(_) => {
                if self.should_stage_extension_capabilities(source_id) {
                    self.stage_agent_context_provider_register(source_id);
                } else {
                    self.register_agent_context_provider(source_id);
                }
            }
            Event::ExtensionContextReady(ready) => {
                if self.should_stage_extension_capabilities(source_id) {
                    self.stage_extension_context_ready(source_id, ready);
                } else {
                    self.publish_extension_context_ready(source_id, ready)?;
                }
            }
            Event::ExtAgentContextPublish(publish) => {
                if self.should_stage_extension_capabilities(source_id) {
                    self.stage_agent_context_publish(source_id, publish);
                } else {
                    self.publish_agent_context_publish(source_id, publish);
                }
            }
            Event::ExtPromptFragmentPublish(publish) => {
                if self.should_stage_extension_capabilities(source_id) {
                    self.stage_extension_prompt_fragment(source_id, publish);
                } else {
                    self.publish_extension_prompt_fragment(source_id, publish);
                }
            }
            Event::ExtPromptSubmitRequest(request) => {
                self.handle_extension_prompt_submit_request(request)?;
            }
            Event::AgentMetadataSet(set) => {
                if self.validate_agent_metadata_set(&set).is_ok() {
                    self.enqueue_publish(
                        Some(source_id),
                        Event::AgentMetadataSet(set),
                        false,
                        false,
                        None,
                    );
                }
            }
            Event::AgentMetadataUnset(unset) => {
                if self.validate_agent_metadata_unset(&unset).is_ok() {
                    self.enqueue_publish(
                        Some(source_id),
                        Event::AgentMetadataUnset(unset),
                        false,
                        false,
                        None,
                    );
                }
            }
            Event::StartAgentRequest(query) => {
                if self.should_stage_extension_capabilities(source_id) {
                    self.stage_start_agent_request(source_id, query);
                } else {
                    self.handle_start_agent_request(source_id, query)?;
                }
            }
            Event::ProviderPromptSubmitted(submitted) => {
                if !self.canceled_prompts.contains(&submitted.agent_prompt_id)
                    && self.provider_prompt_owner_matches(
                        source_id,
                        &submitted.agent_prompt_id,
                        tau_proto::EventName::PROVIDER_PROMPT_SUBMITTED,
                    )
                {
                    self.publish_event(Some(source_id), Event::ProviderPromptSubmitted(submitted));
                }
            }
            Event::ProviderResponseUpdated(mut updated) => {
                if !self.canceled_prompts.contains(&updated.agent_prompt_id)
                    && self.provider_prompt_owner_matches(
                        source_id,
                        &updated.agent_prompt_id,
                        tau_proto::EventName::PROVIDER_RESPONSE_UPDATED,
                    )
                {
                    self.enrich_provider_response_updated_compaction(&mut updated);
                    self.publish_event(Some(source_id), Event::ProviderResponseUpdated(updated));
                }
            }
            Event::ProviderResponseFinished(response) => {
                if self.provider_prompt_owner_matches(
                    source_id,
                    &response.agent_prompt_id,
                    tau_proto::EventName::PROVIDER_RESPONSE_FINISHED,
                ) {
                    self.handle_provider_response_finished_from(Some(source_id), response)?;
                }
            }
            Event::ProviderCacheMissDiagnostic(diagnostic) => {
                if self.provider_prompt_owner_matches(
                    source_id,
                    &diagnostic.agent_prompt_id,
                    tau_proto::EventName::PROVIDER_CACHE_MISS_DIAGNOSTIC,
                ) {
                    self.publish_event(
                        Some(source_id),
                        Event::ProviderCacheMissDiagnostic(diagnostic),
                    );
                }
            }
            other => {
                if !Self::is_extension_fallback_emit_allowed(&other) {
                    return Ok(());
                }
                let mut event = other;
                if let Event::HarnessNotice(notice) = &mut event {
                    notice.kind = tau_proto::notice_kind::EXTENSION_NOTICE.to_owned();
                    notice.always_show = false;
                    if notice.level == tau_proto::NoticeLevel::Critical {
                        notice.level = tau_proto::NoticeLevel::Warning;
                    }
                }
                let transient = transient_override.unwrap_or_else(|| event.defaults_to_transient());
                if self.should_stage_extension_capabilities(source_id) {
                    self.stage_extension_publish(source_id, event, transient);
                } else {
                    self.enqueue_publish(Some(source_id), event, transient, false, None);
                }
            }
        }
        Ok(())
    }

    fn handle_client_message(
        &mut self,
        client_id: &str,
        message: HarnessInputMessage,
    ) -> Result<bool, HarnessError> {
        match message {
            HarnessInputMessage::Hello(hello) => {
                if let Err(error) = validate_protocol_version(&hello) {
                    let _ = self.bus.send_to(
                        client_id,
                        None,
                        HarnessOutputMessage::Disconnect(Disconnect {
                            reason: Some(error.to_string()),
                        }),
                    );
                    return Ok(false);
                }
                Ok(true)
            }
            HarnessInputMessage::Subscribe(subscribe) => {
                match self.complete_subscription(client_id, subscribe.selectors) {
                    Ok(()) => Ok(true),
                    Err(RouteError::SubscriptionDenied { reason, .. }) => {
                        let _ = self.bus.send_to(
                            client_id,
                            None,
                            HarnessOutputMessage::Disconnect(Disconnect {
                                reason: Some(format!("subscription denied: {reason}")),
                            }),
                        );
                        Ok(false)
                    }
                    Err(other) => Err(HarnessError::Route(other)),
                }
            }
            HarnessInputMessage::Disconnect(_) => Ok(false),
            HarnessInputMessage::GetAgentPromptCreated(request) => {
                self.send_agent_prompt_created_result(client_id, request);
                Ok(true)
            }
            HarnessInputMessage::GetRenderedSystemPrompt(request) => {
                self.send_rendered_system_prompt_result(client_id, request);
                Ok(true)
            }
            HarnessInputMessage::GetRenderedPrompt(request) => {
                self.send_rendered_prompt_result(client_id, request);
                Ok(true)
            }
            HarnessInputMessage::GetRenderedToolDefinitions(request) => {
                self.send_rendered_tool_definitions_result(client_id, request);
                Ok(true)
            }
            HarnessInputMessage::Emit(emit) => {
                let (event, transient) = emit.into_parts();
                self.handle_client_event_inner_with_transient(client_id, event, Some(transient))?;
                Ok(true)
            }
            // Other input messages from clients are ignored.
            HarnessInputMessage::ConfigError(_)
            | HarnessInputMessage::Intercept(_)
            | HarnessInputMessage::InterceptReply(_)
            | HarnessInputMessage::Ready(_)
            | HarnessInputMessage::ExtensionDataRequest(_) => Ok(true),
        }
    }

    #[cfg(test)]
    fn handle_client_event_inner(
        &mut self,
        client_id: &str,
        event: Event,
    ) -> Result<bool, HarnessError> {
        self.handle_client_event_inner_with_transient(client_id, event, None)
    }

    fn handle_client_event_inner_with_transient(
        &mut self,
        client_id: &str,
        event: Event,
        transient_override: Option<bool>,
    ) -> Result<bool, HarnessError> {
        let event_name = event.name();
        if event_name.category() == &tau_proto::EventCategory::Provider {
            self.handle_extension_event_inner_with_transient(client_id, event, transient_override)?;
            return Ok(true);
        }

        match event {
            Event::UiRoleSelect(select) => self.handle_ui_role_select(select),
            Event::UiAgentModelSelect(select) => self.handle_ui_agent_model_select(select),
            Event::UiRoleUpdate(req) => self.handle_ui_role_update(req),
            Event::UiPromptSubmitted(prompt) => self.handle_ui_prompt_submitted(prompt),
            Event::ActionInvoke(invoke) => self.handle_action_invoke(client_id, invoke),
            Event::ActionSchemaPublished(_) | Event::ActionResult(_) | Event::ActionError(_) => {
                Ok(true)
            }
            Event::UiSwitchSession(req) => self.handle_ui_switch_session(client_id, req),
            Event::UiCreateAgent(req) => self.handle_ui_create_agent(req),
            Event::UiSetAgentDisplayName(req) => self.handle_ui_set_agent_display_name(req),
            Event::AgentMetadataSet(set) => {
                if self.validate_agent_metadata_set(&set).is_ok() {
                    self.enqueue_publish(
                        Some(client_id),
                        Event::AgentMetadataSet(set),
                        false,
                        false,
                        None,
                    );
                }
                Ok(true)
            }
            Event::AgentMetadataUnset(unset) => {
                if self.validate_agent_metadata_unset(&unset).is_ok() {
                    self.enqueue_publish(
                        Some(client_id),
                        Event::AgentMetadataUnset(unset),
                        false,
                        false,
                        None,
                    );
                }
                Ok(true)
            }
            Event::UiTreeRequest(req) => self.handle_ui_tree_request(client_id, req),
            Event::UiNavigateTree(req) => self.handle_ui_navigate_tree(client_id, req),
            Event::UiCompactRequest(req) => self.handle_ui_compact_request(client_id, req),
            Event::UiCancelPrompt(req) => {
                self.handle_cancel_prompt(&req);
                Ok(true)
            }
            Event::UiRecallQueuedPrompt(req) => {
                self.handle_recall_queued_prompt(&req);
                Ok(true)
            }
            other => {
                if !Self::is_client_fallback_emit_allowed(&other)
                    || Self::requires_tool_event_intake(&other)
                    || Self::is_peer_forbidden_harness_fact(&other)
                {
                    return Ok(true);
                }
                let transient = transient_override.unwrap_or_else(|| other.defaults_to_transient());
                self.enqueue_publish(Some(client_id), other, transient, false, None);
                Ok(true)
            }
        }
    }

    fn handle_ui_role_select(
        &mut self,
        select: tau_proto::UiRoleSelect,
    ) -> Result<bool, HarnessError> {
        if !self.available_roles.contains_key(&select.role) {
            self.publish_event(
                None,
                Event::HarnessNotice(tau_proto::HarnessNotice {
                    kind: tau_proto::notice_kind::UI_COMMAND_ERROR.to_owned(),
                    message: format!("unknown role: {}", select.role),
                    level: tau_proto::NoticeLevel::Info,
                    always_show: false,
                }),
            );
            return Ok(true);
        }

        let was_empty = self.selected_model.is_none();
        self.selected_role = select.role.clone();
        self.reconcile_selected_model_with_available();
        if self.selected_model.is_none() {
            self.publish_event(
                None,
                Event::HarnessNotice(tau_proto::HarnessNotice {
                    kind: tau_proto::notice_kind::MODEL_SELECTION.to_owned(),
                    message: format!("role `{}` has no available model", select.role),
                    level: tau_proto::NoticeLevel::Info,
                    always_show: false,
                }),
            );
        }
        self.publish_current_model_state();
        if was_empty && self.selected_model.is_some() && self.turn_state.is_idle() {
            self.try_advance_queue();
        }
        Ok(true)
    }

    fn handle_ui_agent_model_select(
        &mut self,
        select: tau_proto::UiAgentModelSelect,
    ) -> Result<bool, HarnessError> {
        if !self.available_models.contains(&select.model) {
            self.emit_info(&format!("unknown model: {}", select.model));
            return Ok(true);
        }
        let cid = if let Some(target_agent_id) = select.target_agent_id.as_deref() {
            self.runtime_agent_id_for_target_agent(Some(target_agent_id))
        } else {
            let mut matches = self.agents.iter().filter_map(|(cid, conv)| {
                (conv.session_id == select.session_id
                    && conv.originator.is_user()
                    && conv.agent_id.is_some())
                .then_some(cid.clone())
            });
            let first = matches.next();
            if matches.next().is_some() {
                None
            } else {
                first
            }
        };
        let Some(cid) = cid else {
            self.emit_info("/model: no selected agent to update");
            return Ok(true);
        };
        let Some(conv) = self.agents.get_mut(&cid) else {
            self.emit_info("/model: selected agent is not loaded");
            return Ok(true);
        };
        if conv.session_id != select.session_id {
            self.emit_info("/model: selected agent is not in this session");
            return Ok(true);
        }
        conv.model_override = Some(select.model.clone());
        let agent_name = conv
            .display_name
            .clone()
            .or_else(|| conv.agent_id.clone())
            .unwrap_or_else(|| cid.to_string());
        self.emit_info(&format!(
            "agent `{agent_name}` model set to {}",
            select.model
        ));
        Ok(true)
    }
    fn handle_ui_role_update(
        &mut self,
        req: tau_proto::UiRoleUpdate,
    ) -> Result<bool, HarnessError> {
        let mut selected_role_changed = false;
        let selected_was_empty = self.selected_model.is_none();
        match req.action {
            tau_proto::UiRoleUpdateAction::Delete => {
                selected_role_changed = self.handle_ui_role_delete(req.role)?;
            }
            action => {
                if let Some(next_role) = self.role_after_update(&req.role, action) {
                    self.available_roles
                        .insert(req.role.clone(), next_role.clone());
                    self.role_overrides.insert(req.role.clone(), next_role);
                    selected_role_changed = self.selected_role == req.role;
                }
            }
        }
        if selected_role_changed {
            self.reconcile_selected_model_with_available();
            self.publish_current_model_state();
            if selected_was_empty && self.selected_model.is_some() && self.turn_state.is_idle() {
                self.try_advance_queue();
            }
        }
        self.publish_event(
            None,
            Event::HarnessRolesAvailable(tau_proto::HarnessRolesAvailable {
                roles: role_infos(
                    &self.provider_model_info,
                    &self.available_roles,
                    &self.available_models,
                ),
                groups: self.current_role_groups(),
                custom_prompts: self.custom_prompts.clone(),
            }),
        );
        self.publish_delegate_roles_context();
        Ok(true)
    }

    fn handle_ui_role_delete(&mut self, role_name: String) -> Result<bool, HarnessError> {
        let was_selected = self.selected_role == role_name;
        let previous_override = self.role_overrides.remove(&role_name);
        let configured_role = load_harness_settings_or_warn(&self.dirs)
            .0
            .roles
            .get(&role_name)
            .cloned();

        if let Some(role) = configured_role {
            self.available_roles.insert(role_name, role);
            return Ok(was_selected);
        }

        let removed_role = self.available_roles.remove(&role_name);
        if self.available_roles.is_empty() {
            if let Some(role) = removed_role {
                self.available_roles.insert(role_name.clone(), role);
            }
            if let Some(role) = previous_override {
                self.role_overrides.insert(role_name.clone(), role);
            }
            self.emit_info("/role: cannot delete the last role");
            return Ok(false);
        }
        if was_selected {
            self.selected_role = fallback_role(&self.available_roles);
            return Ok(true);
        }
        Ok(false)
    }

    fn handle_extension_prompt_submit_request(
        &mut self,
        request: tau_proto::ExtPromptSubmitRequest,
    ) -> Result<(), HarnessError> {
        let agent_id = request.agent_id.to_string();
        let Some(cid) = self.agent_routes.get(&agent_id).cloned() else {
            self.emit_info(&format!(
                "extension prompt submit rejected: unknown or unloaded agent `{agent_id}`"
            ));
            return Ok(());
        };
        let Some(session_id) = self.agents.get(&cid).map(|agent| agent.session_id.clone()) else {
            self.emit_info(&format!(
                "extension prompt submit rejected: unloaded agent `{agent_id}`"
            ));
            return Ok(());
        };
        let prompt = PendingPrompt::user(request.text).with_ctx_id(request.ctx_id);
        let submission = self.submit_prompt_to_agent(session_id, &agent_id, prompt)?;
        if !matches!(submission, PromptSubmission::Rejected { .. }) {
            let _ = self.agent_store.record_agent_user_interaction(&agent_id);
        }
        if matches!(submission, PromptSubmission::Queued) {
            self.interrupt_active_waits();
        }
        Ok(())
    }

    fn handle_ui_prompt_submitted(
        &mut self,
        prompt: tau_proto::UiPromptSubmitted,
    ) -> Result<bool, HarnessError> {
        let agent_id = prompt.agent_id.to_string();
        let is_user_interaction =
            prompt.originator.is_user() && !prompt.message_class.is_internal();
        let text = if is_user_interaction {
            let Some(text) = self.expand_user_skill_command(&prompt.text) else {
                return Ok(true);
            };
            text
        } else {
            prompt.text.clone()
        };
        let pending = if prompt.message_class.is_internal() {
            PendingPrompt::internal(text)
        } else {
            PendingPrompt::user(text)
        }
        .with_ctx_id(prompt.ctx_id.clone());
        let submission = self.submit_prompt_to_agent(prompt.session_id, &agent_id, pending)?;
        if !matches!(submission, PromptSubmission::Rejected { .. }) && is_user_interaction {
            let _ = self.agent_store.record_agent_user_interaction(&agent_id);
        }
        if matches!(submission, PromptSubmission::Queued) && !prompt.message_class.is_internal() {
            self.interrupt_active_waits();
        }
        Ok(true)
    }

    fn expand_user_skill_command(&mut self, text: &str) -> Option<String> {
        let Some((name, args)) = user_skill_invocation::parse_user_skill_command(text) else {
            return Some(text.to_owned());
        };
        if let Some(message) = tau_skills::skill_name_validation_message(name) {
            self.emit_info(&format!("/skill: invalid skill name `{name}`: {message}"));
            return None;
        }
        let skill_name = tau_proto::SkillName::from(name.to_owned());
        let Some(skill) = self.discovered_skills.get(&skill_name).cloned() else {
            self.emit_info(&format!("/skill: unknown skill `{name}`"));
            return None;
        };
        if !skill.user_invocable {
            self.emit_info(&format!("/skill: skill `{name}` is not user-invocable"));
            return None;
        }
        match user_skill_invocation::read_user_invoked_skill_body(&skill.source) {
            Ok(loaded) => {
                if loaded.truncated {
                    self.emit_info_important(&format!(
                        "skill too long: {} truncated to {} bytes while invoking {name}",
                        skill.source.label(),
                        user_skill_invocation::MAX_USER_INVOKED_SKILL_BYTES
                    ));
                }
                Some(user_skill_invocation::format_user_invoked_skill_prompt(
                    name,
                    &skill.source,
                    &loaded.body,
                    loaded.truncated.then_some(loaded.total_bytes),
                    args,
                ))
            }
            Err(message) => {
                self.emit_info(&format!("/skill: failed to load `{name}`: {message}"));
                None
            }
        }
    }

    fn handle_ui_set_agent_display_name(
        &mut self,
        req: tau_proto::UiSetAgentDisplayName,
    ) -> Result<bool, HarnessError> {
        if req.session_id != self.current_session_id {
            self.emit_info(&format!(
                "harness is bound to session `{}`; agent-name request for `{}` rejected",
                self.current_session_id.as_str(),
                req.session_id.as_str()
            ));
            return Ok(true);
        }
        let display_name = normalize_display_name(Some(&req.display_name));
        let Some(display_name) = display_name else {
            self.emit_info("agent display name must not be empty");
            return Ok(true);
        };
        let agent_id = req.agent_id.to_string();
        let Some(cid) = self.agent_routes.get(&agent_id).cloned() else {
            self.emit_info(&format!("unknown agent: {agent_id}"));
            return Ok(true);
        };
        if let Some(conv) = self.agents.get_mut(&cid) {
            conv.display_name = Some(display_name.clone());
        }
        self.publish_for_agent(
            &cid,
            Event::AgentDisplayNameSet(tau_proto::AgentDisplayNameSet {
                agent_id: req.agent_id,
                display_name,
            }),
        );
        Ok(true)
    }

    fn handle_ui_switch_session(
        &mut self,
        client_id: &str,
        req: tau_proto::UiSwitchSession,
    ) -> Result<bool, HarnessError> {
        self.publish_event(Some(client_id), Event::UiSwitchSession(req.clone()));
        self.switch_session(req.new_session_id, req.reason)?;
        Ok(true)
    }

    fn handle_ui_create_agent(
        &mut self,
        req: tau_proto::UiCreateAgent,
    ) -> Result<bool, HarnessError> {
        if req.session_id != self.current_session_id {
            let reason = format!(
                "harness is bound to session `{}`; create-agent for `{}` rejected",
                self.current_session_id.as_str(),
                req.session_id.as_str()
            );
            self.emit_info(&reason);
            return Ok(true);
        }
        if !self.available_roles.contains_key(&req.role) {
            self.emit_info(&format!("unknown role `{}`", req.role));
            return Ok(true);
        }
        if let Err(error) = self.validate_initial_agent_metadata(&req.metadata) {
            self.emit_info(&format!("create-agent metadata rejected: {error}"));
            return Ok(true);
        }
        let parent_cid = match req.parent_agent.as_ref() {
            Some(agent_id) => match self.agent_routes.get(agent_id.as_str()).cloned() {
                Some(cid) => Some(cid),
                None => {
                    self.emit_info(&format!(
                        "parent_agent `{agent_id}` is not loaded in the current session"
                    ));
                    return Ok(true);
                }
            },
            None => None,
        };
        let initial_prompt = if let Some(initial_prompt) = req.initial_prompt {
            let is_user_initial_prompt =
                req.originator.is_user() && !req.message_class.is_internal();
            let initial_prompt = if is_user_initial_prompt {
                let Some(text) = self.expand_user_skill_command(&initial_prompt) else {
                    return Ok(true);
                };
                text
            } else {
                initial_prompt
            };
            Some(initial_prompt)
        } else {
            None
        };
        let cid = self.create_durable_user_agent_with_parent(
            req.session_id.clone(),
            &req.role,
            parent_cid,
            req.metadata,
        );
        if let Some(conv) = self.agents.get_mut(&cid) {
            conv.next_ctx_id = req.ctx_id.clone();
        }
        if let Some(initial_prompt) = initial_prompt {
            if !req.message_class.is_internal() {
                self.preempt_blocking_ext_side_agents(&req.session_id);
            }
            let prompt = if req.message_class.is_internal() {
                PendingPrompt::internal(initial_prompt)
            } else {
                PendingPrompt::user(initial_prompt)
            }
            .with_ctx_id(req.ctx_id.clone());
            if self.dispatch_blocked_for(&cid) || !self.session_initialized(&req.session_id) {
                if let Some(conv) = self.agents.get_mut(&cid) {
                    conv.pending_prompts.push_back(prompt.clone());
                }
                self.publish_event(
                    None,
                    Event::AgentPromptQueued(AgentPromptQueued {
                        agent_id: crate::parse_agent_id(
                            self.target_agent_id_for_agent(&cid)
                                .unwrap_or_else(|| cid.to_string()),
                        ),
                        text: prompt.text,
                        message_class: prompt.message_class,
                    }),
                );
                self.try_advance_queue();
            } else {
                self.dispatch_prompt_for_agent(&cid, prompt)?;
            }
        }
        Ok(true)
    }

    fn handle_ui_tree_request(
        &mut self,
        client_id: &str,
        req: tau_proto::UiTreeRequest,
    ) -> Result<bool, HarnessError> {
        self.publish_event(Some(client_id), Event::UiTreeRequest(req.clone()));
        self.handle_tree_request(&req.session_id, req.target_agent_id.as_deref());
        Ok(true)
    }

    fn handle_ui_navigate_tree(
        &mut self,
        _client_id: &str,
        req: tau_proto::UiNavigateTree,
    ) -> Result<bool, HarnessError> {
        // Validate the target node exists in *this* harness's bound
        // session before publishing. The durable branch-state fact is
        // agent-owned (`agent.head_moved`), not the UI-scoped request.
        if let Some((cid, agent_id, node_id)) = self.validate_navigate_tree_target(
            &req.session_id,
            req.target_agent_id.as_deref(),
            req.node_id,
        ) {
            self.publish_event_for_agent(
                &cid,
                None,
                Event::AgentHeadMoved(tau_proto::AgentHeadMoved { agent_id, node_id }),
            );
            self.emit_info(&format!("navigated to node {}", req.node_id));
        }
        Ok(true)
    }

    fn handle_ui_compact_request(
        &mut self,
        client_id: &str,
        req: tau_proto::UiCompactRequest,
    ) -> Result<bool, HarnessError> {
        self.publish_event(Some(client_id), Event::UiCompactRequest(req.clone()));
        self.handle_compact_request(req.session_id, req.target_agent_id.as_deref());
        Ok(true)
    }

    fn runtime_agent_id_for_target_agent(&self, target_agent_id: Option<&str>) -> Option<AgentId> {
        self.agent_routes.get(target_agent_id?).cloned()
    }

    fn target_agent_id_for_agent(&self, cid: &AgentId) -> Option<String> {
        self.agents.get(cid).and_then(|conv| conv.agent_id.clone())
    }

    fn resolve_shell_output_target_agent(
        &mut self,
        finished: &tau_proto::ShellCommandFinished,
    ) -> Option<(AgentId, tau_proto::AgentId)> {
        if finished.session_id != self.current_session_id {
            self.emit_info(&format!(
                "shell output ignored: harness is bound to session `{}` but command finished for `{}`",
                self.current_session_id.as_str(),
                finished.session_id.as_str(),
            ));
            return None;
        }

        if let Some(target_agent_id) = finished.target_agent_id.as_ref() {
            let target_agent_id = target_agent_id.to_string();
            let Some(cid) = self.agent_routes.get(&target_agent_id).cloned() else {
                self.emit_info(&format!(
                    "shell output ignored: unknown target agent `{target_agent_id}`"
                ));
                return None;
            };
            let Some(conv) = self.agents.get(&cid) else {
                self.emit_info(&format!(
                    "shell output ignored: target agent `{target_agent_id}` is not loaded"
                ));
                return None;
            };
            if conv.session_id != finished.session_id {
                self.emit_info(&format!(
                    "shell output ignored: target agent `{target_agent_id}` is not in session `{}`",
                    finished.session_id.as_str(),
                ));
                return None;
            }
            let Some(agent_id) = conv.agent_id.as_deref() else {
                self.emit_info(&format!(
                    "shell output ignored: target agent `{target_agent_id}` has no durable id"
                ));
                return None;
            };
            return Some((cid, crate::parse_agent_id(agent_id)));
        }

        self.default_shell_output_target_agent()
    }

    fn default_shell_output_target_agent(&mut self) -> Option<(AgentId, tau_proto::AgentId)> {
        let mut candidates: Vec<_> = self
            .agents
            .iter()
            .filter_map(|(cid, conv)| {
                if conv.session_id != self.current_session_id || !conv.originator.is_user() {
                    return None;
                }
                let agent_id = conv.agent_id.clone()?;
                let last_user_interaction_time = self
                    .agent_store
                    .agent_meta(&agent_id)
                    .ok()
                    .flatten()
                    .map(|meta| meta.last_user_interaction_time)
                    .unwrap_or_default();
                Some((last_user_interaction_time, cid.clone(), agent_id))
            })
            .collect();

        match candidates.len() {
            0 => {
                let role = self.selected_role.clone();
                let cid = self.create_durable_user_agent(self.current_session_id.clone(), &role);
                let agent_id = self
                    .agents
                    .get(&cid)
                    .and_then(|conv| conv.agent_id.as_deref())
                    .expect("new user agent has durable id");
                Some((cid, crate::parse_agent_id(agent_id)))
            }
            1 => {
                let (_, cid, agent_id) = candidates.pop().expect("one candidate");
                Some((cid, crate::parse_agent_id(&agent_id)))
            }
            _ => {
                candidates
                    .sort_by_key(|(last_user_interaction_time, _, _)| *last_user_interaction_time);
                let (selected_time, cid, agent_id) = candidates.pop().expect("last candidate");
                let Some((previous_time, _, _)) = candidates.last() else {
                    return Some((cid, crate::parse_agent_id(&agent_id)));
                };
                if *previous_time < selected_time {
                    return Some((cid, crate::parse_agent_id(&agent_id)));
                }
                self.emit_info(
                    "shell output ignored: multiple user agents exist and no explicit target was provided",
                );
                None
            }
        }
    }

    fn handle_recall_queued_prompt(&mut self, req: &tau_proto::UiRecallQueuedPrompt) {
        if req.session_id != self.current_session_id {
            return;
        }
        let Some(cid) = self.runtime_agent_id_for_target_agent(req.target_agent_id.as_deref())
        else {
            return;
        };
        let Some(prompt) = self.agents.get_mut(&cid).and_then(|conv| {
            let index = conv
                .pending_prompts
                .iter()
                .rposition(|prompt| !prompt.is_internal())?;
            conv.pending_prompts.remove(index)
        }) else {
            return;
        };
        self.publish_event(
            None,
            Event::AgentPromptRecalled(AgentPromptRecalled {
                agent_id: crate::parse_agent_id(
                    self.target_agent_id_for_agent(&cid)
                        .expect("agent has durable id"),
                ),
                text: prompt.text,
            }),
        );
    }

    fn handle_cancel_prompt(&mut self, req: &UiCancelPrompt) {
        if req.session_id != self.current_session_id {
            return;
        }
        let Some(cid) = self.runtime_agent_id_for_target_agent(req.target_agent_id.as_deref())
        else {
            return;
        };
        let Some(conv) = self.agents.get_mut(&cid) else {
            return;
        };
        if matches!(conv.turn_state, AgentTurnState::Idle) {
            self.emit_info("no active turn to cancel");
            return;
        }
        let prompt_id = conv.in_flight_prompt.clone();
        conv.pending_cancel = Some(PendingCancel {
            reason: "cancelled by user".to_owned(),
        });
        conv.pending_prompts.clear();

        if let Some(prompt_id) = prompt_id {
            self.publish_event(
                None,
                Event::UiCancelPrompt(UiCancelPrompt {
                    session_id: req.session_id.clone(),
                    target_agent_id: self
                        .target_agent_id_for_agent(&cid)
                        .map(crate::parse_agent_id),
                    agent_prompt_id: Some(prompt_id),
                }),
            );
        }
        self.apply_pending_cancel_for_agent(&cid);
    }

    fn apply_pending_cancel_for_agent(&mut self, cid: &AgentId) {
        let Some(cancel) = self
            .agents
            .get(cid)
            .and_then(|conv| conv.pending_cancel.clone())
        else {
            return;
        };
        let Some(turn_state) = self.agents.get(cid).map(|conv| conv.turn_state.clone()) else {
            return;
        };
        match turn_state {
            AgentTurnState::Idle => {
                if let Some(conv) = self.agents.get_mut(cid) {
                    conv.pending_cancel = None;
                    conv.pending_prompts.clear();
                }
            }
            AgentTurnState::AgentThinking { .. } => {
                self.finalize_canceled_in_flight_prompt(cid);
                self.emit_info("cancelling current prompt");
                self.try_advance_queue();
            }
            AgentTurnState::ToolsRunning { remaining_calls } => {
                self.cancel_remaining_tool_calls(cid, remaining_calls, &cancel.reason);
                if let Some(conv) = self.agents.get_mut(cid) {
                    conv.pending_cancel = None;
                    conv.pending_prompts.clear();
                    conv.in_flight_prompt = None;
                }
                self.set_agent_turn_state(cid, AgentTurnState::Idle);
                self.emit_info("cancelled current turn");
                self.try_advance_queue();
            }
        }
    }

    fn finalize_canceled_in_flight_prompt(&mut self, cid: &AgentId) {
        let Some((session_id, agent_prompt_id, originator)) =
            self.agents.get(cid).and_then(|conv| {
                conv.in_flight_prompt.clone().map(|agent_prompt_id| {
                    (
                        conv.session_id.clone(),
                        agent_prompt_id,
                        conv.originator.clone(),
                    )
                })
            })
        else {
            return;
        };
        self.canceled_prompts.insert(agent_prompt_id.clone());
        self.publish_prompt_terminated(
            session_id,
            agent_prompt_id,
            AgentPromptTerminationReason::Canceled,
            originator,
        );
        if let Some(conv) = self.agents.get_mut(cid) {
            conv.pending_cancel = None;
            conv.pending_prompts.clear();
            conv.in_flight_prompt = None;
        }
        self.set_agent_turn_state(cid, AgentTurnState::Idle);
    }

    fn cancel_remaining_tool_calls(
        &mut self,
        cid: &AgentId,
        remaining_calls: Vec<ToolCallId>,
        _reason: &str,
    ) {
        let remaining: std::collections::HashSet<ToolCallId> =
            remaining_calls.iter().cloned().collect();
        let mut to_cancel = self.tool_turn.cancel_queued_for(cid, &remaining);
        for call_id in remaining_calls {
            if to_cancel
                .iter()
                .any(|(queued_id, _, _)| queued_id == &call_id)
            {
                continue;
            }
            let Some(tool) = self.pending_tools.get(&call_id).cloned() else {
                continue;
            };
            to_cancel.push((call_id, tool.name, tool.tool_type));
        }

        let cancelled_call_ids: std::collections::HashSet<ToolCallId> = to_cancel
            .iter()
            .map(|(call_id, _, _)| call_id.clone())
            .collect();
        self.record_wait_tool_cancelled(&cancelled_call_ids);

        for (call_id, tool_name, tool_type) in to_cancel {
            self.publish_event(
                Some(HARNESS_CONNECTION_ID),
                Event::ToolCancelRequest(tau_proto::ToolCancelRequest {
                    target_call_id: call_id.clone(),
                }),
            );
            self.publish_for_agent(
                cid,
                Event::ToolCancelled(ToolCancelled {
                    call_id: call_id.clone(),
                    tool_name,
                    tool_type,
                }),
            );
            self.tool_turn.mark_complete(&call_id);
            self.clear_tool_call_tracking(call_id.as_str());
        }
        if let Some(conv) = self.agents.get_mut(cid) {
            conv.tools_in_flight = 0;
        }
    }

    pub(crate) fn is_running_tool_call(&self, target_call_id: &ToolCallId) -> bool {
        self.pending_tools.contains_key(target_call_id)
    }

    pub(crate) fn is_running_cancellable_tool_call(&self, target_call_id: &ToolCallId) -> bool {
        self.pending_tools.contains_key(target_call_id)
    }

    pub(crate) fn is_completed_tool_call(&self, target_call_id: &ToolCallId) -> bool {
        self.completed_tool_calls.contains(target_call_id)
    }

    pub(crate) fn publish_tool_cancel_request(&mut self, target_call_id: ToolCallId) {
        self.publish_event(
            Some(HARNESS_CONNECTION_ID),
            Event::ToolCancelRequest(tau_proto::ToolCancelRequest { target_call_id }),
        );
    }

    pub(crate) fn cancel_start_agent_request(
        &mut self,
        query_id: &str,
        target_call_id: &ToolCallId,
        suppress_background_completion_prompt: bool,
    ) -> Result<(), String> {
        let mut source_id = None;
        let mut stopped_pending_agent_ids = Vec::new();
        self.pending_start_agent_requests.retain(|pending| {
            let is_canceled = pending.query.query_id == query_id
                || pending.query.tool_call_id.as_ref() == Some(target_call_id);
            if is_canceled {
                source_id = Some(pending.source_id.clone());
                stopped_pending_agent_ids.push(pending.agent_id.clone());
            }
            !is_canceled
        });
        self.stopped_agent_ids.extend(stopped_pending_agent_ids);

        if source_id.is_none() {
            source_id = self.agents.iter().find_map(|(_, conv)| {
                if conv.parent_tool_call_id.as_ref() != Some(target_call_id) {
                    return None;
                }
                conv.source_connection.as_ref().map(|id| id.to_string())
            });
        }
        let Some(source_id) = source_id else {
            return Err("Tool call is not a running cancellable tool call".to_owned());
        };

        self.emit_info("tool call cancellation request");
        if suppress_background_completion_prompt {
            self.suppress_background_completion_prompt(target_call_id.clone());
        }
        self.cancel_delegate_side_conversation(target_call_id);
        let result = tau_proto::StartAgentResult {
            query_id: query_id.to_owned(),
            text: String::new(),
            error: Some("Tool call canceled".to_owned()),
        };
        if source_id == HARNESS_CONNECTION_ID {
            self.publish_event(Some(HARNESS_CONNECTION_ID), Event::StartAgentResult(result));
        } else {
            let _ = self.bus.send_to(
                source_id.as_str(),
                None,
                HarnessOutputMessage::deliver(Event::StartAgentResult(result)),
            );
        }
        Ok(())
    }

    fn cancel_delegate_side_conversation(&mut self, target_call_id: &ToolCallId) {
        let Some((cid, session_id, spid, turn_state, originator)) =
            self.agents.iter().find_map(|(cid, conv)| {
                if conv.parent_tool_call_id.as_ref() != Some(target_call_id) {
                    return None;
                }
                Some((
                    cid.clone(),
                    conv.session_id.clone(),
                    conv.in_flight_prompt.clone(),
                    conv.turn_state.clone(),
                    conv.originator.clone(),
                ))
            })
        else {
            return;
        };

        let mut cancelled_calls = match turn_state {
            AgentTurnState::ToolsRunning { remaining_calls } => remaining_calls,
            _ => Vec::new(),
        };
        cancelled_calls.extend(self.tool_turn.backgrounded_calls_for(&cid));
        cancelled_calls.extend(self.background_completion_call_ids_for_teardown(&cid));
        cancelled_calls.sort();
        cancelled_calls.dedup();
        self.cancel_remaining_tool_calls(&cid, cancelled_calls, "delegate cancel tool");
        if let Some(spid) = spid {
            self.canceled_prompts.insert(spid.clone());
            self.publish_prompt_terminated(
                session_id.clone(),
                spid.clone(),
                AgentPromptTerminationReason::Canceled,
                originator,
            );
            self.prompt_agents.remove(&spid);
            self.publish_event(
                None,
                Event::UiCancelPrompt(UiCancelPrompt {
                    session_id,
                    target_agent_id: self
                        .target_agent_id_for_agent(&cid)
                        .map(crate::parse_agent_id),
                    agent_prompt_id: Some(spid),
                }),
            );
        }
        self.release_start_agent_request(&cid);
        self.discard_background_completion_target_before_teardown(&cid);
        self.remove_agent(&cid);
        self.try_advance_queue();
    }

    fn remove_extension_context_for_connection(&mut self, connection_id: &str) {
        let disconnected = tau_proto::ConnectionId::from(connection_id);
        self.extension_prompt_fragments.remove(&disconnected);
        self.agent_context.remove_contributor(&disconnected);
        self.agent_context_providers.remove(&disconnected);
        self.pending_agent_context_ready.retain(|_, waiting_on| {
            waiting_on.remove(&disconnected);
            !waiting_on.is_empty()
        });
    }

    fn clear_session_agent_context(&mut self) {
        self.agent_context.clear();
        self.pending_agent_context_ready.clear();
    }

    fn disable_optional_extension(&mut self, connection_id: &str, message: &str) {
        if let Some(entry) = self.extensions.entries.get_mut(connection_id) {
            entry.respawn_allowed = false;
        }
        self.emit_optional_extension_skipped(message);
        self.handle_disconnect(connection_id);
    }

    fn handle_disconnect(&mut self, connection_id: &str) {
        let meta = self.bus.connection(connection_id).cloned();
        let is_extension = meta.as_ref().is_some_and(|meta| {
            meta.origin == ConnectionOrigin::Supervised || meta.origin == ConnectionOrigin::InMemory
        });
        if is_extension {
            // Mark the extension non-blocking before any cleanup can advance
            // session init or prompt dispatch.
            self.set_extension_state(connection_id, ExtensionState::Disconnected);
        }
        self.extensions.activation_staging.remove(connection_id);
        self.remove_discovered_context(connection_id);
        self.interceptors.remove_connection(connection_id);
        self.fail_pending_intercept_for_disconnect(connection_id);
        self.remove_extension_context_for_connection(connection_id);

        if is_extension {
            self.unregister_connection_tools_for_disconnect(connection_id);
            self.action_registry.unregister_connection(connection_id);
        }

        self.fail_pending_action_invocations_for_connection(connection_id);
        let completed_foreground_calls = self.fail_pending_tool_calls_for_connection(connection_id);
        self.pending_provider_prompts
            .retain(|_, provider_id| provider_id.as_str() != connection_id);
        self.client_writers
            .remove(&tau_proto::ConnectionId::from(connection_id));
        if self
            .provider_models_by_extension
            .remove(connection_id)
            .is_some()
        {
            self.refresh_provider_models_and_publish_state();
        }
        self.drain_pending_tool_invocations_or_report();
        for (call_id, cid) in completed_foreground_calls {
            self.maybe_complete_agent_turn_for(&cid, call_id.as_str());
        }
        self.maybe_complete_session_init_for_disconnect(connection_id);
        self.try_advance_queue();
        let Some(meta) = self.bus.disconnect(connection_id).or(meta) else {
            return;
        };
        if is_extension {
            self.emit_extension_exited(&meta.name);
        }
        if meta.origin == ConnectionOrigin::Supervised
            && let Err(error) = self.try_respawn_supervised_extension(connection_id)
        {
            self.emit_info(&format!(
                "failed to respawn extension {}: {error}",
                meta.name
            ));
        }
    }

    fn unregister_connection_tools_for_disconnect(&mut self, connection_id: &str) {
        let removing_tools: Vec<(ToolName, ToolName)> = self
            .registry
            .all_tool_names()
            .into_iter()
            .filter_map(|tool_name| {
                self.registry
                    .providers_for(tool_name.as_str())
                    .into_iter()
                    .find(|provider| provider.connection_id.as_str() == connection_id)
                    .map(|provider| {
                        (
                            tool_name.clone(),
                            self.tool_model_visible_name(&provider.tool).clone(),
                        )
                    })
            })
            .collect();

        let _ = self.registry.unregister_connection(connection_id);
        for (internal_name, visible_name) in removing_tools {
            if self
                .registry
                .providers_for(internal_name.as_str())
                .is_empty()
            {
                self.mark_tool_unavailable_for_notice(internal_name, visible_name);
            }
        }
    }

    fn is_provider_extension(&self, connection_id: &str) -> bool {
        self.extensions
            .entries
            .get(connection_id)
            .is_some_and(|entry| entry.kind == ClientKind::Provider)
    }

    fn accepts_provider_event_from(
        &self,
        source_id: &str,
        event_name: &tau_proto::EventName,
    ) -> bool {
        match self.bus.connection(source_id) {
            Some(metadata) if metadata.kind == ClientKind::Provider => true,
            Some(metadata) => {
                tracing::warn!(
                    target: "tau_harness",
                    event = %event_name,
                    source_id,
                    kind = ?metadata.kind,
                    "discarding provider event from non-provider connection"
                );
                false
            }
            None => {
                tracing::warn!(
                    target: "tau_harness",
                    event = %event_name,
                    source_id,
                    "discarding provider event from unknown connection"
                );
                false
            }
        }
    }

    fn provider_prompt_owner_matches(
        &self,
        source_id: &str,
        agent_prompt_id: &AgentPromptId,
        event_name: tau_proto::EventName,
    ) -> bool {
        match self.pending_provider_prompts.get(agent_prompt_id) {
            Some(expected) if expected.as_str() == source_id => true,
            Some(expected) => {
                tracing::warn!(
                    target: "tau_harness",
                    event = %event_name,
                    agent_prompt_id = %agent_prompt_id,
                    expected_provider = %expected,
                    source_id,
                    "discarding provider event from non-owning provider"
                );
                false
            }
            None => {
                tracing::warn!(
                    target: "tau_harness",
                    event = %event_name,
                    agent_prompt_id = %agent_prompt_id,
                    source_id,
                    "discarding provider event for prompt without a pending provider route"
                );
                false
            }
        }
    }

    fn fail_pending_tool_calls_for_connection(
        &mut self,
        connection_id: &str,
    ) -> Vec<(ToolCallId, AgentId)> {
        let mut failed_call_ids: Vec<ToolCallId> = self
            .pending_tool_providers
            .iter()
            .filter_map(|(call_id, provider_id)| {
                if provider_id.as_str() == connection_id {
                    Some(call_id.clone())
                } else {
                    None
                }
            })
            .collect();
        // Keep disconnect cleanup deterministic; queued work is drained only
        // after the whole sorted batch is terminalized below.
        failed_call_ids.sort();

        let mut completed_foreground_calls: Vec<(ToolCallId, AgentId)> = Vec::new();

        for call_id in failed_call_ids {
            let Some(tool) = self.pending_tools.get(&call_id).cloned() else {
                continue;
            };
            let mut error = ToolError {
                call_id: call_id.clone(),
                tool_name: tool.name,
                tool_type: tool.tool_type,
                message: extension_disconnected_tool_call_error_message(&call_id),
                details: None,
                originator: tau_proto::PromptOriginator::User,

                display: None,
            };
            if self.tool_turn.is_backgrounded(&call_id) {
                error.message = extension_disconnected_background_tool_call_error_message(&call_id);
                if self.tool_agents.contains_key(call_id.as_str()) {
                    self.handle_background_tool_error_without_advancing(
                        Some(HARNESS_CONNECTION_ID),
                        error,
                    );
                } else {
                    self.publish_terminal_tool_error(None, Some(HARNESS_CONNECTION_ID), error);
                    self.tool_turn.mark_complete(&call_id);
                    self.clear_tool_call_tracking(call_id.as_str());
                }
                continue;
            }

            // Publish on the owning agent's branch so the
            // synthesized failure folds onto the right node. Without
            // the snap, sibling side agents could leave
            // `tree.head` on the wrong branch and the fold would land
            // there instead. Complete the failed in-flight calls without
            // draining queued calls or advancing prompts yet; disconnect
            // handling unregisters the dead provider first, then drains
            // the scheduler and completes turns after all interrupted calls
            // have been terminalized.
            let owner = self.tool_agents.get(call_id.as_str()).cloned();
            if let Some(cid) = owner.as_ref() {
                self.publish_terminal_tool_error(Some(cid), Some(HARNESS_CONNECTION_ID), error);
            } else {
                // No conversation attribution — fall back to the
                // unsnapped publish so the error still reaches the
                // bus / log.
                self.publish_terminal_tool_error(None, Some(HARNESS_CONNECTION_ID), error);
            }
            if let Some(cid) = self.finish_tool_call_runtime_state(call_id.as_str()) {
                completed_foreground_calls.push((call_id.clone(), cid));
            }
            self.clear_tool_call_tracking(call_id.as_str());
        }

        completed_foreground_calls
    }

    fn send_action_error_to_client(
        &mut self,
        client_id: &str,
        invocation_id: ActionInvocationId,
        action_id: String,
        message: String,
    ) {
        let _ = self.bus.send_to(
            client_id,
            Some(HARNESS_CONNECTION_ID),
            HarnessOutputMessage::deliver(Event::ActionError(ActionError {
                invocation_id,
                action_id,
                message,
                details: None,
            })),
        );
    }

    fn handle_action_invoke(
        &mut self,
        client_id: &str,
        invoke: ActionInvoke,
    ) -> Result<bool, HarnessError> {
        if self
            .bus
            .connection(client_id)
            .is_none_or(|metadata| metadata.kind != ClientKind::Ui)
        {
            self.send_action_error_to_client(
                client_id,
                invoke.invocation_id,
                invoke.action_id,
                "only UI clients may invoke extension actions".to_owned(),
            );
            return Ok(true);
        }
        if invoke.session_id != self.current_session_id {
            self.send_action_error_to_client(
                client_id,
                invoke.invocation_id,
                invoke.action_id,
                format!(
                    "action invocation targets session `{}` but current session is `{}`",
                    invoke.session_id, self.current_session_id
                ),
            );
            return Ok(true);
        }
        if self
            .pending_action_invocations
            .contains_key(&invoke.invocation_id)
        {
            self.send_action_error_to_client(
                client_id,
                invoke.invocation_id,
                invoke.action_id,
                "duplicate pending action invocation id".to_owned(),
            );
            return Ok(true);
        }

        let provider_connection_id = match self.action_registry.route_action_invoke(&invoke) {
            Ok(provider_connection_id) => provider_connection_id,
            Err(error) => {
                self.send_action_error_to_client(
                    client_id,
                    invoke.invocation_id,
                    invoke.action_id,
                    error.to_string(),
                );
                return Ok(true);
            }
        };

        match self.bus.send_to(
            provider_connection_id.as_str(),
            Some(client_id),
            HarnessOutputMessage::deliver(Event::ActionInvoke(invoke.clone())),
        ) {
            Ok(report) if !report.delivered_to.is_empty() => {
                self.pending_action_invocations.insert(
                    invoke.invocation_id.clone(),
                    PendingActionInvocation {
                        provider_connection_id,
                        requester_client_id: client_id.into(),
                        action_id: invoke.action_id,
                    },
                );
            }
            Ok(report) => {
                tracing::warn!(
                    target: "tau_harness",
                    invocation_id = %invoke.invocation_id,
                    ?report,
                    "action invocation route did not deliver"
                );
                self.send_action_error_to_client(
                    client_id,
                    invoke.invocation_id,
                    invoke.action_id,
                    "action provider is unavailable".to_owned(),
                );
            }
            Err(error) => {
                tracing::warn!(
                    target: "tau_harness",
                    invocation_id = %invoke.invocation_id,
                    %error,
                    "action invocation route failed"
                );
                self.send_action_error_to_client(
                    client_id,
                    invoke.invocation_id,
                    invoke.action_id,
                    "action provider is unavailable".to_owned(),
                );
            }
        }
        Ok(true)
    }

    fn handle_action_result(&mut self, source_id: &str, result: ActionResult) {
        let Some(pending) = self
            .pending_action_invocations
            .get(&result.invocation_id)
            .cloned()
        else {
            return;
        };
        if pending.provider_connection_id.as_str() != source_id
            || pending.action_id != result.action_id
        {
            tracing::warn!(
                target: "tau_harness",
                invocation_id = %result.invocation_id,
                source_id,
                expected_provider = %pending.provider_connection_id,
                expected_action = %pending.action_id,
                action_id = %result.action_id,
                "discarding action result from non-owning or mismatched source"
            );
            return;
        }
        self.pending_action_invocations
            .remove(&result.invocation_id);
        let _ = self.bus.send_to(
            pending.requester_client_id.as_str(),
            Some(source_id),
            HarnessOutputMessage::deliver(Event::ActionResult(result)),
        );
    }

    fn handle_action_error(&mut self, source_id: &str, error: ActionError) {
        let Some(pending) = self
            .pending_action_invocations
            .get(&error.invocation_id)
            .cloned()
        else {
            return;
        };
        if pending.provider_connection_id.as_str() != source_id
            || pending.action_id != error.action_id
        {
            tracing::warn!(
                target: "tau_harness",
                invocation_id = %error.invocation_id,
                source_id,
                expected_provider = %pending.provider_connection_id,
                expected_action = %pending.action_id,
                action_id = %error.action_id,
                "discarding action error from non-owning or mismatched source"
            );
            return;
        }
        self.pending_action_invocations.remove(&error.invocation_id);
        let _ = self.bus.send_to(
            pending.requester_client_id.as_str(),
            Some(source_id),
            HarnessOutputMessage::deliver(Event::ActionError(error)),
        );
    }

    fn fail_pending_action_invocations_for_connection(&mut self, connection_id: &str) {
        let mut failed: Vec<_> = self
            .pending_action_invocations
            .iter()
            .filter_map(|(invocation_id, pending)| {
                (pending.provider_connection_id.as_str() == connection_id)
                    .then_some((invocation_id.clone(), pending.clone()))
            })
            .collect();
        failed.sort_by(|(left, _), (right, _)| left.as_str().cmp(right.as_str()));
        for (invocation_id, pending) in failed {
            self.pending_action_invocations.remove(&invocation_id);
            if pending.requester_client_id.as_str() == connection_id {
                continue;
            }
            self.send_action_error_to_client(
                pending.requester_client_id.as_str(),
                invocation_id,
                pending.action_id.clone(),
                format!(
                    "action `{}` was interrupted because extension disconnected",
                    pending.action_id
                ),
            );
        }
        self.pending_action_invocations
            .retain(|_, pending| pending.requester_client_id.as_str() != connection_id);
    }

    fn try_respawn_supervised_extension(
        &mut self,
        connection_id: &str,
    ) -> Result<(), HarnessError> {
        let Some(entry) = self.extensions.entries.get_mut(connection_id) else {
            return Ok(());
        };
        let Some(config) = entry.supervised_config.clone() else {
            return Ok(());
        };
        if entry.kind == ClientKind::Provider || !entry.respawn_allowed {
            return Ok(());
        }

        entry.restart_attempt += 1;
        let attempt = entry.restart_attempt;
        let instance_id = entry.instance_id;
        let name = entry.name.clone();
        let kind = entry.kind.clone();
        let secrets = entry.secrets.clone();
        self.publish_event(
            Some("harness"),
            Event::ExtensionRestarting(tau_proto::ExtensionRestarting {
                instance_id,
                extension_name: name.clone().into(),
                pid: None,
                attempt,
                reason: Some("unexpected disconnect".to_owned()),
            }),
        );

        let log_path = extension_stderr_log_path(
            &self.sessions_dir(),
            self.current_session_id.as_str(),
            &config.name,
        )
        .map_err(|error| HarnessError::Participant(error.to_string()))?;
        tracing::info!(
            target: "tau_harness::startup",
            extension = %config.name,
            command = %config.command,
            args = ?config.args,
            attempt,
            "respawning extension",
        );
        let spawned = spawn_supervised(&config, kind.clone(), Some(log_path), &self.tx)?;
        let new_connection_id = spawned.connection_id.clone();
        tracing::info!(
            target: "tau_harness::startup",
            extension = %config.name,
            pid = spawned.child_pid,
            attempt,
            "extension respawned",
        );

        let old_key = tau_proto::ConnectionId::from(connection_id);
        self.queue_extension_connect(ExtensionConnectCommand {
            entry: ExtensionEntry {
                name,
                instance_id,
                connection_id: new_connection_id,
                kind,
                require: config.require,
                respawn_allowed: true,
                pid: Some(spawned.child_pid),
                in_process_thread: None,
                supervised_config: Some(config),
                secrets,
                restart_attempt: attempt,
                state: ExtensionState::Spawning,
            },
            origin: ConnectionOrigin::Supervised,
            writer_tx: spawned.writer_tx,
            initialized_ack: spawned.initialized_ack,
            replaces: Some(old_key),
        })?;
        Ok(())
    }

    fn extension_tool_request_rejection(&self, request: &ToolRequest) -> Option<String> {
        if request.call_id.is_empty() {
            return Some(format!(
                "extension emitted tool request `{}` with an empty call_id; refusing to route it",
                request.tool_name
            ));
        }
        self.known_tool_call_ids().contains(&request.call_id).then(|| {
            format!(
                "extension emitted tool request `{}` with already-known call_id `{}`; refusing to route it",
                request.tool_name, request.call_id
            )
        })
    }

    fn reject_extension_tool_request(&mut self, message: String) {
        self.emit_info_important(&message);
    }

    // -----------------------------------------------------------------------
    // Tool-call session bookkeeping
    // -----------------------------------------------------------------------
    //
    // Terminal tool facts (`ToolResult` / `ToolError` and provider variants)
    // are persisted into the owning agent transcript. `ToolRequest` itself is a
    // runtime routing intent; these helpers maintain maps that attribute later
    // terminal events back to the originating agent or extension request.

    /// Records runtime bookkeeping for an extension-originated `ToolRequest`
    /// that does not have an owning agent. Agent-owned tool calls are tracked
    /// through the prompt/tool routing path instead.
    fn track_extension_tool_request_metadata(&mut self, request: &ToolRequest) {
        self.pending_tools.insert(
            request.call_id.clone(),
            PendingTool {
                name: request.tool_name.clone(),
                internal_name: request.tool_name.clone(),
                tool_type: request.tool_type,
            },
        );
    }

    fn clear_prompt_tool_snapshot(&mut self, agent_prompt_id: &AgentPromptId) {
        self.prompt_tool_specs.remove(agent_prompt_id);
        self.prompt_tool_call_prompts
            .retain(|_, prompt_id| prompt_id != agent_prompt_id);
    }

    /// Releases the conversation/name/provider mappings for a completed tool
    /// call. Must run *after* the result/error event has been published so
    /// terminal-event enrichment and transcript attribution can still read the
    /// runtime metadata.
    pub(crate) fn clear_tool_call_tracking(&mut self, call_id: &str) {
        self.completed_tool_calls.insert(call_id.into());
        self.tool_agents.remove(call_id);
        self.pending_tools.remove(call_id);
        self.pending_tool_providers.remove(call_id);
        if let Some(prompt_id) = self.prompt_tool_call_prompts.remove(call_id)
            && !self
                .prompt_tool_call_prompts
                .values()
                .any(|other_prompt_id| other_prompt_id == &prompt_id)
        {
            self.prompt_tool_specs.remove(&prompt_id);
        }
    }

    fn validate_tool_event_source(&self, call_id: &ToolCallId, source_id: &str) -> bool {
        match self.pending_tool_providers.get(call_id) {
            Some(provider_id) => provider_id.as_str() == source_id,
            None if self.is_harness_owned_tool_call(call_id) => source_id == HARNESS_CONNECTION_ID,
            None => true,
        }
    }

    fn is_extension_fallback_emit_allowed(event: &Event) -> bool {
        matches!(
            event,
            Event::ExtensionEvent(_)
                | Event::HarnessNotice(_)
                | Event::Osc1337SetUserVar(_)
                | Event::TermBell(_)
        )
    }

    fn is_client_fallback_emit_allowed(event: &Event) -> bool {
        matches!(
            event,
            Event::UiPromptDraft(_)
                | Event::UiFocusChanged(_)
                | Event::UiDetachRequest(_)
                | Event::UiShellCommand(_)
        )
    }

    fn requires_tool_event_intake(event: &Event) -> bool {
        matches!(
            event,
            Event::ToolResult(_)
                | Event::ToolError(_)
                | Event::ProviderToolResult(_)
                | Event::ProviderToolError(_)
                | Event::ToolProgress(_)
                | Event::ToolCancelled(_)
                | Event::ToolBackgroundResult(_)
                | Event::ToolBackgroundError(_)
        )
    }

    fn is_peer_forbidden_harness_fact(event: &Event) -> bool {
        // Peer intake rejects the same harness-owned facts that interception
        // treats as protected. Update this with `MUST_PASS_BY_DEFAULT` and
        // `immutable_protected_fact_was_modified` when adding new
        // harness-owned immutable facts.
        matches!(
            event,
            Event::SessionStarted(_)
                | Event::SessionShutdown(_)
                | Event::SessionAgentLoaded(_)
                | Event::SessionAgentUnloaded(_)
                | Event::AgentStarted(_)
                | Event::AgentMessageSent(_)
                | Event::AgentMessageReceived(_)
        )
    }

    fn is_harness_owned_tool_call(&self, call_id: &ToolCallId) -> bool {
        self.tool_agents.contains_key(call_id)
            && self.pending_tools.get(call_id).is_some_and(|tool| {
                self.internal_tool_handlers
                    .iter()
                    .any(|handler| handler.handles(&tool.internal_name))
            })
    }

    // -----------------------------------------------------------------------
    // Lifecycle helpers
    // -----------------------------------------------------------------------

    fn find_extension_by_name(&self, name: &str) -> Option<&ExtensionEntry> {
        self.extensions.entries.values().find(|e| e.name == name)
    }

    fn find_extension_by_connection(&self, connection_id: &str) -> Option<&ExtensionEntry> {
        self.extensions.entries.get(connection_id)
    }

    fn publish_lifecycle_event(&mut self, event: Event) {
        self.publish_event(Some("harness"), event);
    }

    fn emit_extension_starting(&mut self, extension_name: &str) {
        let (iid, pid) = self
            .find_extension_by_name(extension_name)
            .map(|e| (e.instance_id, e.pid))
            .unwrap_or((0.into(), None));
        self.lifecycle_messages
            .push(format!("extension {extension_name} starting"));
        self.publish_lifecycle_event(Event::ExtensionStarting(tau_proto::ExtensionStarting {
            instance_id: iid,
            extension_name: extension_name.into(),
            pid,
        }));
    }

    fn emit_extension_ready(&mut self, connection_id: &str) {
        let Some(ext) = self.find_extension_by_connection(connection_id) else {
            return;
        };
        let name = ext.name.clone();
        let iid = ext.instance_id;
        let pid = ext.pid;
        self.lifecycle_messages
            .push(format!("extension {name} ready"));
        self.publish_lifecycle_event(Event::ExtensionReady(tau_proto::ExtensionReady {
            instance_id: iid,
            extension_name: name.into(),
            pid,
        }));
    }

    fn emit_extension_exited(&mut self, extension_name: &str) {
        let (iid, pid) = self
            .find_extension_by_name(extension_name)
            .map(|e| (e.instance_id, e.pid))
            .unwrap_or((0.into(), None));
        self.lifecycle_messages
            .push(format!("extension {extension_name} exited"));
        self.publish_lifecycle_event(Event::ExtensionExited(tau_proto::ExtensionExited {
            instance_id: iid,
            extension_name: extension_name.into(),
            pid,
            exit_code: None,
            signal: None,
        }));
    }

    fn check_config_exists(&mut self) {
        if let Some(dir) = tau_config::settings::config_dir()
            && !dir.join("harness.yaml").exists()
        {
            self.emit_info_important(
                "no config found; run `tau init` to create sample config files",
            );
        }
    }

    /// Surface settings-file parse errors captured during the initial
    /// load as mandatory warning `HarnessNotice`s. The loaders already fell
    /// back to defaults and wrote a short stderr line, but stderr is
    /// hidden once the TUI takes over the terminal — without this the
    /// user's only symptom is "my extensions vanished" / "my roles changed"
    /// with no clue why.
    ///
    /// Taking the error as a parameter (instead of re-parsing the file
    /// here) keeps startup to a single parse and avoids a race where the
    /// user fixes the file between the two reads.
    ///
    /// `cli.json5` is intentionally not handled here: the CLI fails
    /// fast on a malformed `cli.json5` before the harness ever
    /// spawns, so there's no "silently fell back to defaults" case
    /// to surface.
    fn emit_startup_settings_errors(
        &mut self,
        harness_settings_error: Option<tau_config::settings::SettingsError>,
    ) {
        if let Some(error) = harness_settings_error {
            self.emit_info_important(&format!("harness.yaml failed to parse — ignored.\n{error}"));
        }
    }

    fn emit_missing_default_role(&mut self, missing: Option<MissingDefaultRole>) {
        if let Some(MissingDefaultRole {
            requested,
            fallback,
        }) = missing
        {
            self.emit_info_important(&format!(
                "default_role `{requested}` is not configured; selected `{fallback}` instead"
            ));
        }
    }

    /// Push the configured `config` value (from `harness.yaml`) to
    /// the just-said-Hello extension. Sends point-to-point so it
    /// arrives even if the extension hasn't subscribed to the
    /// `lifecycle` category yet. In-process extensions don't carry
    /// a `supervised_config` so they get the empty default — they
    /// already accept configuration via constructor parameters.
    fn send_lifecycle_configure(&mut self, source_id: &str) {
        let Some(entry) = self.extensions.entries.get(source_id) else {
            return;
        };
        let config_json = entry
            .supervised_config
            .as_ref()
            .map(|cfg| cfg.config.clone())
            .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
        let secrets = entry.secrets.clone();
        let state_dir =
            match tau_config::settings::extension_state_dir_of(&self.state_dir, &entry.name) {
                Ok(state_dir) => state_dir,
                Err(error) => {
                    tracing::warn!(
                        extension = %entry.name,
                        error = %error,
                        "refusing to configure extension with unsafe state directory name"
                    );
                    let _ = self.bus.send_to(
                        source_id,
                        None,
                        HarnessOutputMessage::Disconnect(Disconnect {
                            reason: Some(error.to_string()),
                        }),
                    );
                    return;
                }
            };
        if let Err(error) = std::fs::create_dir_all(&state_dir) {
            tracing::warn!(
                extension = %entry.name,
                state_dir = %state_dir.display(),
                error = %error,
                "failed to create extension state directory before configure"
            );
        }
        let _ = self.bus.send_to(
            source_id,
            None,
            HarnessOutputMessage::Configure(tau_proto::Configure {
                config: tau_proto::json_to_cbor(&config_json),
                instance_name: self
                    .extensions
                    .entries
                    .get(source_id)
                    .map(|entry| entry.name.clone().into()),
                state_dir: Some(state_dir),
                secrets,
            }),
        );
    }

    pub(crate) fn emit_info(&mut self, message: &str) {
        self.emit_info_with_level(message, tau_proto::NoticeLevel::Info);
    }

    fn emit_harness_failure(&mut self, message: &str) {
        self.emit_notice(
            tau_proto::notice_kind::HARNESS_FAILURE,
            tau_proto::NoticeLevel::Warning,
            true,
            message,
        );
    }

    pub(crate) fn emit_info_important(&mut self, message: &str) {
        self.emit_notice(
            tau_proto::notice_kind::HARNESS_INTERNAL_WARNING,
            tau_proto::NoticeLevel::Warning,
            true,
            message,
        );
    }

    fn emit_optional_extension_skipped(&mut self, message: &str) {
        self.emit_notice(
            tau_proto::notice_kind::EXTENSION_OPTIONAL_SKIPPED,
            tau_proto::NoticeLevel::Warning,
            true,
            message,
        );
    }

    fn emit_extension_startup_diagnostics(&mut self, diagnostics: &[ExtensionStartupDiagnostic]) {
        for diagnostic in diagnostics {
            self.emit_optional_extension_skipped(&diagnostic.message);
        }
    }

    fn emit_info_with_level(&mut self, message: &str, level: tau_proto::NoticeLevel) {
        let (kind, always_show) = if matches!(
            level,
            tau_proto::NoticeLevel::Critical | tau_proto::NoticeLevel::Warning
        ) {
            (tau_proto::notice_kind::HARNESS_INTERNAL_WARNING, true)
        } else {
            (tau_proto::notice_kind::HARNESS_NOTICE, false)
        };
        self.emit_notice(kind, level, always_show, message);
    }

    fn emit_notice(
        &mut self,
        kind: &str,
        level: tau_proto::NoticeLevel,
        always_show: bool,
        message: &str,
    ) {
        let always_show = always_show || level == tau_proto::NoticeLevel::Critical;
        let notice = tau_proto::HarnessNotice {
            kind: kind.to_owned(),
            message: message.to_owned(),
            level,
            always_show,
        };
        if always_show {
            self.replayable_harness_notices.push(notice.clone());
        }
        self.enqueue_publish(
            Some("harness"),
            Event::HarnessNotice(notice),
            false,
            always_show,
            None,
        );
    }

    fn publish_prompt_terminated(
        &mut self,
        _session_id: SessionId,
        agent_prompt_id: AgentPromptId,
        reason: AgentPromptTerminationReason,
        originator: PromptOriginator,
    ) {
        let agent_id = crate::parse_agent_id(
            self.prompt_agents
                .get(&agent_prompt_id)
                .and_then(|cid| self.agents.get(cid))
                .and_then(|conv| conv.agent_id.clone())
                .expect("agent has durable id"),
        );
        self.publish_event(
            None,
            Event::AgentPromptTerminated(AgentPromptTerminated {
                agent_id,
                agent_prompt_id,
                reason,
                originator,
            }),
        );
    }

    fn remove_discovered_context(&mut self, source_id: &str) {
        let affected_names = self
            .discovered_skill_candidates
            .iter_mut()
            .filter_map(|(name, candidates)| {
                let old_len = candidates.len();
                candidates.retain(|skill| {
                    matches!(skill.source, DiscoveredSkillSource::BuiltIn { .. })
                        || skill.source_id != source_id
                });
                (candidates.len() != old_len).then(|| name.clone())
            })
            .collect::<Vec<_>>();
        self.discovered_skill_candidates
            .retain(|_, candidates| !candidates.is_empty());
        for name in affected_names {
            self.recompute_discovered_skill_winner(&name);
        }
        self.discovered_agents_files
            .retain(|file| file.source_id != source_id);
    }

    fn recompute_discovered_skill_winner(&mut self, name: &tau_proto::SkillName) {
        let winner = self
            .discovered_skill_candidates
            .get(name)
            .and_then(|candidates| selected_skill_candidate(candidates).cloned());
        if let Some(winner) = winner {
            self.discovered_skills.insert(name.clone(), winner);
        } else {
            self.discovered_skills.remove(name);
        }
    }

    fn record_discovered_skill(&mut self, source_id: &str, skill: &tau_proto::ExtSkillAvailable) {
        if let Some(message) = tau_skills::skill_name_validation_message(skill.name.as_str()) {
            self.emit_info_important(&format!(
                "skill skipped: {} from {} has invalid name: {}",
                skill.name,
                skill.file_path.display(),
                message,
            ));
            return;
        }

        let description = if tau_skills::MAX_DESCRIPTION_LENGTH < skill.description.len() {
            self.emit_info_important(&format!(
                "skill warning: {} from {} description exceeds {} bytes ({}); truncating",
                skill.name,
                skill.file_path.display(),
                tau_skills::MAX_DESCRIPTION_LENGTH,
                skill.description.len(),
            ));
            tau_skills::truncate_description(&skill.description).into_owned()
        } else {
            skill.description.clone()
        };

        let modified = skill_file_modified_time(&skill.file_path);
        let candidate = DiscoveredSkill {
            source_id: source_id.into(),
            description,
            source: DiscoveredSkillSource::File(std::path::PathBuf::from(&skill.file_path)),
            add_to_prompt: skill.add_to_prompt,
            user_invocable: skill.user_invocable || skill.disable_model_invocation,
            disable_model_invocation: skill.disable_model_invocation,
            modified,
        };
        let previous_winner = self.discovered_skills.get(&skill.name).cloned();
        let candidates = self
            .discovered_skill_candidates
            .entry(skill.name.clone())
            .or_default();
        if let Some(existing) = candidates
            .iter_mut()
            .find(|existing| existing.source_id == source_id)
        {
            *existing = candidate;
        } else {
            candidates.push(candidate);
        }
        self.recompute_discovered_skill_winner(&skill.name);

        if let Some(previous_winner) = previous_winner
            && previous_winner.source_id != source_id
            && let Some(current_winner) = self.discovered_skills.get(&skill.name).cloned()
        {
            self.emit_skill_collision_notice(skill, source_id, &previous_winner, &current_winner);
        }
    }

    fn emit_skill_collision_notice(
        &mut self,
        skill: &tau_proto::ExtSkillAvailable,
        source_id: &str,
        previous_winner: &DiscoveredSkill,
        current_winner: &DiscoveredSkill,
    ) {
        if current_winner.source_id == source_id {
            self.emit_notice(
                tau_proto::notice_kind::SKILL_COLLISION,
                tau_proto::NoticeLevel::Trace,
                false,
                &format!(
                    "skill collision: {} from {} replaces {} from {} (newer modified time)",
                    skill.name,
                    skill.file_path.display(),
                    previous_winner.source.label(),
                    previous_winner.source_id,
                ),
            );
            return;
        }
        let modified = skill_file_modified_time(&skill.file_path);
        let reason = if compare_skill_modified(modified, current_winner.modified).is_eq() {
            "same or unavailable modified time"
        } else {
            "newer modified time"
        };
        self.emit_notice(
            tau_proto::notice_kind::SKILL_COLLISION,
            tau_proto::NoticeLevel::Trace,
            false,
            &format!(
                "skill collision: {} from {} ignored; keeping {} from {} ({reason})",
                skill.name,
                skill.file_path.display(),
                current_winner.source.label(),
                current_winner.source_id,
            ),
        );
    }

    fn session_init_provider_ids(&self) -> std::collections::HashSet<tau_proto::ConnectionId> {
        HashSet::new()
    }

    fn agent_context_provider_ids(
        &self,
        agent_id: tau_proto::AgentId,
    ) -> HashSet<tau_proto::ConnectionId> {
        let event = Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
            session_id: self.current_session_id.clone(),
            agent_id,
        });
        self.tool_connections_subscribed_to(&event)
            .into_iter()
            .filter(|connection_id| self.agent_context_providers.contains(connection_id))
            .collect()
    }

    fn tool_connections_subscribed_to(&self, event: &Event) -> HashSet<tau_proto::ConnectionId> {
        self.bus
            .connections()
            .into_iter()
            .filter(|connection| {
                connection.kind == ClientKind::Tool
                    && connection.origin != ConnectionOrigin::Socket
                    && self
                        .bus
                        .subscriptions(connection.id.as_str())
                        .is_some_and(|selectors| selector_matches_event(selectors, event))
            })
            .map(|connection| connection.id)
            .collect()
    }

    pub(crate) fn session_initialized(&self, session_id: &SessionId) -> bool {
        self.initialized_sessions.contains(session_id)
    }

    pub(crate) fn agent_context_ready_for(&self, cid: &AgentId) -> bool {
        let Some(agent_id) = self
            .agents
            .get(cid)
            .and_then(|agent| agent.agent_id.as_ref())
            .map(|agent_id| tau_proto::AgentId::parse(agent_id.as_str()).expect("agent id"))
        else {
            return true;
        };
        self.agent_context_ready_for_loaded_agent(&agent_id)
    }

    fn agent_context_ready_for_loaded_agent(&self, agent_id: &tau_proto::AgentId) -> bool {
        self.pending_agent_context_ready
            .get(agent_id)
            .is_none_or(HashSet::is_empty)
    }

    fn available_delegate_role_names(&self) -> Vec<String> {
        let mut names: Vec<_> = self
            .available_roles
            .keys()
            .filter(|name| {
                model_for_role(&self.provider_model_info, &self.available_roles, name).is_some()
            })
            .cloned()
            .collect();
        names.sort();
        names
    }

    fn available_delegate_roles_message(&self) -> String {
        let roles = self.available_delegate_role_names();
        if roles.is_empty() {
            "available roles: (none)".to_owned()
        } else {
            format!("available roles: {}", roles.join(", "))
        }
    }

    fn resolve_start_agent_request_role(
        &self,
        query: &tau_proto::StartAgentRequest,
    ) -> Result<String, String> {
        let requested = if let Some(role) = query.role.as_deref() {
            role
        } else if query.tool_call_id.is_some() {
            "senior-engineer"
        } else {
            self.selected_role.as_str()
        };

        if self.available_roles.contains_key(requested)
            && model_for_role(&self.provider_model_info, &self.available_roles, requested).is_some()
        {
            return Ok(requested.to_owned());
        }

        let reason = if query.role.is_none() && query.tool_call_id.is_some() {
            "agent_start requires default role `senior-engineer`, but it is not available"
        } else if self.available_roles.contains_key(requested) {
            "requested role is not backed by an available model"
        } else {
            "requested role does not exist"
        };
        Err(format!(
            "{reason}: `{requested}`; {}",
            self.available_delegate_roles_message()
        ))
    }

    fn fail_start_agent_request(&mut self, source_id: &str, query_id: String, error: String) {
        let result = tau_proto::StartAgentResult {
            query_id,
            text: String::new(),
            error: Some(error),
        };
        if source_id == HARNESS_CONNECTION_ID {
            self.publish_event(Some(HARNESS_CONNECTION_ID), Event::StartAgentResult(result));
        } else {
            let _ = self.bus.send_to(
                source_id,
                None,
                HarnessOutputMessage::deliver(Event::StartAgentResult(result)),
            );
        }
    }

    /// Queue and dispatch an extension-started sub-agent request.
    fn handle_start_agent_request(
        &mut self,
        source_id: &str,
        query: tau_proto::StartAgentRequest,
    ) -> Result<(), HarnessError> {
        let query_id = query.query_id.clone();
        let pending = match self.prepare_start_agent_request(source_id, query) {
            Ok(Some(pending)) => pending,
            Ok(None) => return Ok(()),
            Err(error) => {
                self.fail_start_agent_request(source_id, query_id, error);
                return Ok(());
            }
        };
        let accepted = tau_proto::StartAgentAccepted {
            query_id: pending.query.query_id.clone(),
            agent_id: crate::parse_agent_id(&pending.agent_id),
        };
        self.publish_event(
            Some(HARNESS_CONNECTION_ID),
            Event::StartAgentAccepted(accepted.clone()),
        );
        let _ = self.bus.send_to(
            source_id,
            None,
            HarnessOutputMessage::deliver(Event::StartAgentAccepted(accepted)),
        );
        self.pending_start_agent_requests.push_back(pending);
        self.drain_pending_start_agent_requests()
    }

    fn accept_duplicate_start_agent_request(
        &mut self,
        source_id: &str,
        query_id: &str,
        agent_id: &str,
    ) {
        let accepted = tau_proto::StartAgentAccepted {
            query_id: query_id.to_owned(),
            agent_id: crate::parse_agent_id(agent_id),
        };
        self.publish_event(
            Some(HARNESS_CONNECTION_ID),
            Event::StartAgentAccepted(accepted.clone()),
        );
        let _ = self.bus.send_to(
            source_id,
            None,
            HarnessOutputMessage::deliver(Event::StartAgentAccepted(accepted)),
        );
    }

    /// Enqueue an internal start-agent request and return its minted agent id.
    pub(crate) fn enqueue_internal_start_agent_request_without_draining(
        &mut self,
        query: tau_proto::StartAgentRequest,
    ) -> Result<String, String> {
        let Some(pending) = self.prepare_start_agent_request(HARNESS_CONNECTION_ID, query)? else {
            return Err("duplicate tool-backed start-agent request".to_owned());
        };
        let agent_id = pending.agent_id.clone();
        let accepted = tau_proto::StartAgentAccepted {
            query_id: pending.query.query_id.clone(),
            agent_id: crate::parse_agent_id(&agent_id),
        };
        self.publish_event(
            Some(HARNESS_CONNECTION_ID),
            Event::StartAgentAccepted(accepted.clone()),
        );
        let _ = self.bus.send_to(
            HARNESS_CONNECTION_ID,
            None,
            HarnessOutputMessage::deliver(Event::StartAgentAccepted(accepted)),
        );
        self.pending_start_agent_requests.push_back(pending);
        Ok(agent_id)
    }

    fn validate_agent_metadata_key(&self, key: &tau_proto::AgentMetadataKey) -> Result<(), String> {
        if key.as_str().is_empty() {
            return Err("agent metadata key must not be empty".to_owned());
        }
        if tau_proto::MAX_AGENT_METADATA_KEY_BYTES < key.as_str().len() {
            return Err("agent metadata key exceeds 256 bytes".to_owned());
        }
        Ok(())
    }

    fn validate_agent_metadata_target(
        &mut self,
        agent_id: &tau_proto::AgentId,
    ) -> Result<(), String> {
        if self.session_loaded_agents.contains(agent_id)
            || self.agent_routes.contains_key(agent_id.as_str())
        {
            return Ok(());
        }
        match self.agent_store.load_agent(agent_id.as_str()) {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err(format!("unknown agent metadata target `{agent_id}`")),
            Err(error) => Err(format!(
                "failed to load metadata target `{agent_id}`: {error}"
            )),
        }
    }

    fn validate_agent_metadata_set(
        &mut self,
        set: &tau_proto::AgentMetadataSet,
    ) -> Result<(), String> {
        self.validate_agent_metadata_target(&set.agent_id)?;
        self.validate_agent_metadata_key(&set.key)?;
        let value_bytes = tau_proto::encode_message_to_vec(&set.value)
            .map_err(|error| format!("failed to measure agent metadata value: {error}"))?;
        if tau_proto::MAX_AGENT_METADATA_VALUE_BYTES < value_bytes.len() {
            return Err("agent metadata value exceeds 64 KiB".to_owned());
        }
        Ok(())
    }

    fn validate_initial_agent_metadata(
        &self,
        metadata: &[tau_proto::AgentInitialMetadata],
    ) -> Result<(), String> {
        for item in metadata {
            self.validate_agent_metadata_key(&item.key)?;
            let value_bytes = tau_proto::encode_message_to_vec(&item.value)
                .map_err(|error| format!("failed to measure agent metadata value: {error}"))?;
            if tau_proto::MAX_AGENT_METADATA_VALUE_BYTES < value_bytes.len() {
                return Err("agent metadata value exceeds 64 KiB".to_owned());
            }
        }
        Ok(())
    }

    fn validate_agent_metadata_unset(
        &mut self,
        unset: &tau_proto::AgentMetadataUnset,
    ) -> Result<(), String> {
        self.validate_agent_metadata_target(&unset.agent_id)?;
        self.validate_agent_metadata_key(&unset.key)
    }

    pub(crate) fn validate_agent_metadata_event(&mut self, event: &Event) -> Result<(), String> {
        match event {
            Event::AgentMetadataSet(set) => self.validate_agent_metadata_set(set),
            Event::AgentMetadataUnset(unset) => self.validate_agent_metadata_unset(unset),
            _ => Ok(()),
        }
    }

    fn resolve_start_agent_parent_cid(
        &self,
        query: &tau_proto::StartAgentRequest,
    ) -> Result<Option<AgentId>, String> {
        let explicit = query
            .parent_agent
            .as_ref()
            .map(|agent_id| {
                self.agent_routes
                    .get(agent_id.as_str())
                    .cloned()
                    .ok_or_else(|| {
                        format!("parent_agent `{agent_id}` is not loaded in the current session")
                    })
            })
            .transpose()?;
        let tool_parent = query
            .tool_call_id
            .as_ref()
            .and_then(|call_id| self.tool_agents.get(call_id))
            .cloned();
        if let (Some(explicit), Some(tool_parent)) = (&explicit, &tool_parent)
            && explicit != tool_parent
        {
            return Err("parent_agent does not match tool_call_id owner".to_owned());
        }
        Ok(explicit.or(tool_parent))
    }

    fn parent_agent_id_for_cid(&self, cid: &AgentId) -> Option<tau_proto::AgentId> {
        self.agents
            .get(cid)
            .and_then(|agent| agent.parent_agent_id.as_ref())
            .and_then(|parent_cid| self.agents.get(parent_cid))
            .and_then(|parent| parent.agent_id.as_ref())
            .map(crate::parse_agent_id)
    }

    fn inherited_metadata_for_cid(
        &mut self,
        cid: &AgentId,
    ) -> Vec<(tau_proto::AgentMetadataKey, tau_core::AgentMetadataEntry)> {
        let Some(parent_agent_id) = self.parent_agent_id_for_cid(cid) else {
            return Vec::new();
        };
        match self.agent_store.load_agent(parent_agent_id.as_str()) {
            Ok(Some(tree)) => tree.inheritable_metadata().into_iter().collect(),
            Ok(None) => Vec::new(),
            Err(error) => {
                self.emit_info(&format!(
                    "failed to load parent agent `{parent_agent_id}` metadata: {error}"
                ));
                Vec::new()
            }
        }
    }

    fn prepare_start_agent_request(
        &mut self,
        source_id: &str,
        query: tau_proto::StartAgentRequest,
    ) -> Result<Option<PendingStartAgentRequest>, String> {
        let extension_name = self
            .extensions
            .entries
            .get(source_id)
            .map(|e| e.name.clone())
            .unwrap_or_else(|| source_id.to_owned());
        let role = self.resolve_start_agent_request_role(&query)?;
        let duplicate_active = self.agents.iter().find_map(|(cid, conv)| {
            let matches_query = conv.source_connection.is_some()
                && matches!(
                    &conv.originator,
                    tau_proto::PromptOriginator::Extension { name, query_id }
                        if name.as_str() == extension_name && query_id == &query.query_id
                );
            matches_query.then(|| {
                conv.agent_id
                    .clone()
                    .map(|agent_id| (cid.clone(), agent_id))
            })?
        });
        if let Some((cid, agent_id)) = duplicate_active {
            if let Some(conv) = self.agents.get_mut(&cid) {
                conv.source_connection = Some(source_id.into());
            }
            self.accept_duplicate_start_agent_request(source_id, &query.query_id, &agent_id);
            self.emit_info(&format!(
                "rebound duplicate start-agent-request `{}` from `{}` to existing agent `{}`",
                query.query_id, extension_name, agent_id
            ));
            return Ok(None);
        }
        if let Some(idx) = self
            .pending_start_agent_requests
            .iter()
            .position(|pending| {
                pending.extension_name == extension_name && pending.query.query_id == query.query_id
            })
        {
            let agent_id = self.pending_start_agent_requests[idx].agent_id.clone();
            self.pending_start_agent_requests[idx].source_id = source_id.to_owned();
            self.accept_duplicate_start_agent_request(source_id, &query.query_id, &agent_id);
            self.emit_info(&format!(
                "rebound duplicate start-agent-request `{}` from `{}` to pending agent `{}`",
                query.query_id, extension_name, agent_id
            ));
            return Ok(None);
        }
        let agent_id = self.mint_available_agent_id_for_role(&role);
        let cid: AgentId = crate::parse_agent_id(&agent_id);

        // Resolve the parent agent at enqueue time for metadata inheritance:
        // tool-backed requests derive their parent from the conversation that
        // owns the triggering tool call; non-tool requests use an explicit
        // `parent_agent` when provided; otherwise they start with no parent.
        let parent_cid = self.resolve_start_agent_parent_cid(&query)?;

        Ok(Some(PendingStartAgentRequest {
            source_id: source_id.to_owned(),
            extension_name,
            query,
            role,
            cid,
            parent_cid,
            agent_id,
            pending_agent_messages: VecDeque::new(),
        }))
    }

    /// Dispatch queued `StartAgentRequest`s in FIFO order. Directory/update
    /// coordination is owned by extensions such as `tau-ext-shell`, not by the
    /// harness.
    pub(crate) fn drain_pending_start_agent_requests(&mut self) -> Result<(), HarnessError> {
        loop {
            let Some(idx) = self.next_dispatchable_start_agent_request_index() else {
                return Ok(());
            };
            let pending = self
                .pending_start_agent_requests
                .remove(idx)
                .expect("index just located");
            self.start_agent_request(pending)?;
        }
    }

    fn next_dispatchable_start_agent_request_index(&self) -> Option<usize> {
        (!self.pending_start_agent_requests.is_empty()).then_some(0)
    }

    /// Compatibility hook for older teardown paths. Start-agent dispatch no
    /// longer holds harness-side update/exclusive locks, so release only tries
    /// to drain any queued requests left by earlier errors.
    fn release_start_agent_request(&mut self, _cid: &AgentId) {
        if !self.pending_start_agent_requests.is_empty()
            && let Err(error) = self.drain_pending_start_agent_requests()
        {
            self.emit_harness_failure(&format!("queued start-agent dispatch failed: {error}"));
        }
    }

    /// Spawn a fresh side agent runtime for an extension's
    /// [`tau_proto::StartAgentRequest`] and dispatch it after FIFO admission.
    ///
    /// All start-agent requests create an independent agent log with fresh
    /// transcript context. Tool-backed requests (for example `agent_start`) and
    /// non-tool extension requests differ in result routing and
    /// cache/tool-choice details, but neither copies parent transcript
    /// nodes. When an explicit or derived parent agent is known, only
    /// metadata entries marked inheritable are copied to the child (for
    /// example the shell extension's remembered cwd).
    fn start_agent_request(
        &mut self,
        pending: PendingStartAgentRequest,
    ) -> Result<(), HarnessError> {
        let PendingStartAgentRequest {
            source_id,
            extension_name,
            query,
            role,
            cid,
            parent_cid,
            agent_id,
            pending_agent_messages,
        } = pending;
        let parent_call_id = query.tool_call_id.clone();
        let is_tool_backed = parent_call_id.is_some();
        let task_name = query.task_name.clone();
        let display_name = self.display_name_for_new_agent(&agent_id, &role, task_name.as_deref());
        let conversation_role = if query.tool_call_id.is_some() || query.role.is_some() {
            Some(role)
        } else {
            None
        };
        let parent_agent_id = parent_cid.as_ref().and_then(|parent_cid| {
            self.agents
                .contains_key(parent_cid)
                .then(|| parent_cid.clone())
        });
        let session_id = parent_agent_id
            .as_ref()
            .and_then(|parent_cid| self.agents.get(parent_cid))
            .map(|parent| parent.session_id.clone())
            .unwrap_or_else(|| self.current_session_id.clone());
        // Start-agent requests create distinct agent transcripts, so their
        // runtime cursor starts at the root. Parent branch NodeIds belong
        // to the parent's agent log and must not be reused in the child log.
        let initial_head = None;

        let originator = tau_proto::PromptOriginator::Extension {
            name: extension_name.clone().into(),
            query_id: query.query_id.clone(),
        };
        let mut conv = Agent::new(
            cid.clone(),
            session_id.clone(),
            originator,
            initial_head,
            Some(source_id.into()),
        );
        // For tool-backed extensions (currently just `agent_start`)
        // record the parent call id and task name so subsequent
        // sub-agent state changes can be surfaced to the user under
        // that tool block via `DelegateProgress`.
        conv.parent_tool_call_id = parent_call_id;
        conv.parent_agent_id = parent_agent_id;
        conv.display_name = display_name;
        conv.task_name = task_name;
        conv.delegate_input_stats = query.input_stats;
        conv.role = conversation_role;
        conv.agent_id = Some(agent_id.clone());
        conv.pending_prompts = pending_agent_messages;
        self.agent_routes.insert(agent_id.clone(), cid.clone());
        self.agents.insert(cid.clone(), conv);
        self.ensure_loaded_agent_for_agent(&cid, &agent_id);
        if let Some(display_name) = self
            .agents
            .get(&cid)
            .and_then(|conv| normalize_display_name(conv.display_name.as_deref()))
        {
            self.publish_for_agent(
                &cid,
                Event::AgentDisplayNameSet(tau_proto::AgentDisplayNameSet {
                    agent_id: crate::parse_agent_id(&agent_id),
                    display_name,
                }),
            );
        }
        if is_tool_backed {
            self.set_agent_state(&agent_id, AgentState::ActiveDelegated);
        }

        // Emit the initial progress snapshot (`%0/0`, no ctx
        // info yet) so the parent's tool block flips from `…` to the
        // structured form as soon as the side agent exists,
        // without waiting for the sub-agent's first event.
        self.emit_delegate_progress(&cid);

        // Publish the accepted instruction into the side agent transcript and
        // dispatch only after that prompt folds into the agent head.
        self.publish_pending_prompt_for_agent(&cid, PendingPrompt::user(query.instruction))?;
        self.dispatch_prompt_after_user_message_publish(&cid);
        Ok(())
    }

    fn detach_completed_tool_backed_start_agent(&mut self, cid: &AgentId) {
        if let Some(conv) = self.agents.get_mut(cid) {
            // A completed delegate remains addressable by its `agent_id`, but
            // it is no longer fulfilling the parent tool call or owned by the
            // extension query that started it. Clearing the side-query fields
            // makes later user prompts behave like a normal active conversation
            // on the same branch.
            conv.originator = tau_proto::PromptOriginator::User;
            conv.source_connection = None;
            conv.parent_tool_call_id = None;
            conv.parent_agent_id = None;
            conv.task_name = None;
            conv.delegate_input_stats = Default::default();
        }
    }

    /// Publish a `DelegateProgress` snapshot for `cid` if it is a side
    /// conversation backing an `agent_start` tool call. No-op for user
    /// agents and for non-tool start-agent requests.
    fn emit_delegate_progress(&mut self, cid: &AgentId) {
        let Some(conv) = self.agents.get(cid) else {
            return;
        };
        let (Some(call_id), Some(task_name)) =
            (conv.parent_tool_call_id.clone(), conv.task_name.clone())
        else {
            return;
        };
        let role = conv.role.clone();
        let agent_id = conv.agent_id.clone();
        let ctx_window = conv.context_input_tokens.and_then(|_| {
            self.model_for_agent_role(conv)
                .as_ref()
                .and_then(|m| context_window_for_model(&self.provider_model_info, m))
        });
        let display = build_delegate_progress_display(
            &task_name,
            conv.context_input_tokens,
            conv.context_percent_used,
            ctx_window,
            conv.tools_in_flight,
            conv.tools_total,
            conv.delegate_input_stats,
        );
        let progress = tau_proto::DelegateProgress {
            call_id,
            task_name,
            agent_id,
            role,
            ctx_percent: conv.context_percent_used,
            ctx_input_tokens: conv.context_input_tokens,
            ctx_window,
            tools_in_flight: conv.tools_in_flight,
            tools_total: conv.tools_total,
            display: Some(display),
        };
        self.publish_event(None, Event::ToolDelegateProgress(progress));
    }

    fn handle_compact_request(&mut self, session_id: SessionId, target_agent_id: Option<&str>) {
        if session_id != self.current_session_id {
            self.emit_info(&format!(
                "cannot compact session `{session_id}` in this harness; active session is `{}`",
                self.current_session_id
            ));
            return;
        }
        let Some(cid) = self.runtime_agent_id_for_target_agent(target_agent_id) else {
            self.emit_info("unknown agent for compaction");
            return;
        };
        if self.dispatch_blocked_for(&cid) {
            self.emit_info("cannot compact while a prompt or tool turn is in flight");
            return;
        }
        if !self.agent_model_supports_compaction(&cid) {
            self.emit_info("selected model does not support compaction");
            return;
        }
        let Some(conv) = self.agents.get(&cid) else {
            self.emit_info("target user agent is missing");
            return;
        };
        let Some(agent_id) = conv.agent_id.clone() else {
            self.emit_info("nothing to compact yet");
            return;
        };
        self.publish_for_agent(
            &cid,
            Event::AgentCompactionTriggered(tau_proto::AgentCompactionTriggered {
                agent_id: crate::parse_agent_id(&agent_id),
                originator: conv.originator.clone(),
            }),
        );
        self.dispatch_prompt_after_publish_idle(&cid);
    }

    fn compaction_context_for_agent(
        &self,
        cid: &AgentId,
        model: &ModelId,
    ) -> Option<tau_proto::PromptCompactionContext> {
        let supports_compaction = self
            .provider_model_info
            .get(model)
            .is_some_and(|info| info.supports_compaction);
        if !supports_compaction {
            return None;
        }

        let role_name = self.role_name_for_agent_id(cid);
        let role_compaction = self
            .available_roles
            .get(&role_name)
            .and_then(|role| role.compaction)
            .unwrap_or(tau_config::settings::RoleCompaction::ProviderDefault);
        match role_compaction {
            tau_config::settings::RoleCompaction::ProviderDefault => {
                Some(tau_proto::PromptCompactionContext {
                    compact_threshold: None,
                })
            }
            tau_config::settings::RoleCompaction::Threshold(compact_threshold) => {
                Some(tau_proto::PromptCompactionContext {
                    compact_threshold: Some(compact_threshold),
                })
            }
            tau_config::settings::RoleCompaction::Disabled => None,
        }
    }

    fn agent_model_supports_compaction(&self, cid: &AgentId) -> bool {
        let Some(conv) = self.agents.get(cid) else {
            return false;
        };
        let Some(model) = self.model_for_agent_role(conv) else {
            return false;
        };
        self.provider_model_info
            .get(&model)
            .is_some_and(|info| info.supports_compaction)
    }

    fn refresh_provider_model_info(&mut self) {
        let mut provider_model_info = HashMap::new();
        let mut provider_model_routes = HashMap::new();
        let mut source_ids: Vec<_> = self.provider_models_by_extension.keys().collect();
        source_ids.sort();
        for source_id in source_ids {
            let Some(models) = self.provider_models_by_extension.get(source_id) else {
                continue;
            };
            let connection_id = tau_proto::ConnectionId::from(source_id.as_str());
            for model in models {
                provider_model_info.insert(model.id.clone(), model.clone());
                provider_model_routes.insert(model.id.clone(), connection_id.clone());
            }
        }
        self.provider_model_info = provider_model_info;
        self.provider_model_routes = provider_model_routes;
    }

    fn refresh_available_models(&mut self) {
        self.refresh_provider_model_info();
        let mut models: Vec<ModelId> = self.provider_model_info.keys().cloned().collect();
        models.sort();
        self.available_models = models;
    }

    fn role_after_update(
        &mut self,
        role_name: &str,
        action: tau_proto::UiRoleUpdateAction,
    ) -> Option<tau_config::settings::AgentRole> {
        let mut next_role = self
            .available_roles
            .get(role_name)
            .cloned()
            .unwrap_or_default();

        match action {
            tau_proto::UiRoleUpdateAction::Delete => unreachable!("handled by caller"),
            tau_proto::UiRoleUpdateAction::SetModel { model } => {
                next_role.model = model;
            }
            tau_proto::UiRoleUpdateAction::SetEffort { effort } => {
                next_role.effort = effort;
            }
            tau_proto::UiRoleUpdateAction::SetVerbosity { verbosity } => {
                next_role.verbosity = verbosity;
            }
            tau_proto::UiRoleUpdateAction::SetThinkingSummary { thinking_summary } => {
                next_role.thinking_summary = thinking_summary;
            }
            tau_proto::UiRoleUpdateAction::SetServiceTier { service_tier } => {
                next_role.service_tier = service_tier;
            }
            tau_proto::UiRoleUpdateAction::SetCompactionThreshold {
                compaction_threshold,
            } => {
                next_role.compaction = Some(match compaction_threshold {
                    Some(threshold) => tau_config::settings::RoleCompaction::Threshold(threshold),
                    None => tau_config::settings::RoleCompaction::ProviderDefault,
                });
            }
            tau_proto::UiRoleUpdateAction::SetTools { tools } => {
                next_role.tools = tools;
            }
            tau_proto::UiRoleUpdateAction::SetEnableToolGroups { enable_tool_groups } => {
                next_role.enable_tool_groups = enable_tool_groups;
            }
            tau_proto::UiRoleUpdateAction::SetDisableToolGroups {
                disable_tool_groups,
            } => {
                next_role.disable_tool_groups = disable_tool_groups;
            }
            tau_proto::UiRoleUpdateAction::SetEnableTools { enable_tools } => {
                next_role.enable_tools = enable_tools;
            }
            tau_proto::UiRoleUpdateAction::SetDisableTools { disable_tools } => {
                next_role.disable_tools = disable_tools;
            }
        }

        Some(next_role)
    }

    fn reconcile_selected_model_with_available(&mut self) {
        let previous_model = self.selected_model.clone();
        self.selected_model = select_model_for_role(
            &self.provider_model_info,
            &self.available_roles,
            &self.selected_role,
        );
        if previous_model != self.selected_model {
            self.current_session_state.context_input_tokens = None;
            self.current_session_state.context_cached_tokens = None;
            self.current_session_state.context_percent_used = None;
        }
    }

    fn refresh_provider_models_and_publish_state(&mut self) {
        let had_provider_models = !self.provider_model_info.is_empty();
        let had_routable_model = self
            .selected_model
            .as_ref()
            .is_some_and(|model| self.provider_model_routes.contains_key(model));
        self.refresh_available_models();
        self.reconcile_selected_model_with_available();
        self.publish_available_model_state();
        let has_provider_models = !self.provider_model_info.is_empty();
        let has_routable_model = self
            .selected_model
            .as_ref()
            .is_some_and(|model| self.provider_model_routes.contains_key(model));
        if self.turn_state.is_idle()
            && ((!had_routable_model && has_routable_model)
                || (!had_provider_models && has_provider_models))
        {
            self.try_advance_queue();
        }
    }

    fn publish_available_model_state(&mut self) {
        self.publish_event(
            None,
            Event::HarnessModelsAvailable(tau_proto::HarnessModelsAvailable {
                models: self.available_models.clone(),
            }),
        );
        self.publish_event(
            None,
            Event::HarnessRolesAvailable(tau_proto::HarnessRolesAvailable {
                roles: role_infos(
                    &self.provider_model_info,
                    &self.available_roles,
                    &self.available_models,
                ),
                groups: self.current_role_groups(),
                custom_prompts: self.custom_prompts.clone(),
            }),
        );
        self.publish_delegate_roles_context();
        self.publish_current_model_state();
    }

    fn current_role_groups(&self) -> Vec<tau_proto::HarnessRoleGroup> {
        let mut grouped = HashSet::new();
        let mut groups = Vec::new();
        for group in &self.available_role_groups {
            let roles: Vec<_> = group
                .roles
                .iter()
                .filter(|role| self.available_roles.contains_key(*role))
                .inspect(|role| {
                    grouped.insert((*role).clone());
                })
                .cloned()
                .collect();
            if !roles.is_empty() {
                groups.push(tau_proto::HarnessRoleGroup {
                    name: group.name.clone(),
                    roles,
                });
            }
        }
        let mut ungrouped: Vec<_> = self
            .available_roles
            .keys()
            .filter(|role| !grouped.contains(*role))
            .cloned()
            .collect();
        ungrouped.sort();
        groups.extend(
            ungrouped
                .into_iter()
                .map(|role| tau_proto::HarnessRoleGroup {
                    name: role.clone(),
                    roles: vec![role],
                }),
        );
        groups
    }

    fn publish_current_model_state(&mut self) {
        let selected_model = self.selected_model.clone();
        let (effort_levels, verbosity_levels, thinking_levels) =
            if let Some(model) = selected_model.as_ref() {
                (
                    efforts_for_model(&self.provider_model_info, model),
                    verbosities_for_model(&self.provider_model_info, model),
                    thinking_summaries_for_model(&self.provider_model_info, model),
                )
            } else {
                (Vec::new(), Vec::new(), Vec::new())
            };
        let context_window = selected_model
            .as_ref()
            .and_then(|model| context_window_for_model(&self.provider_model_info, model));
        self.current_session_state.context_percent_used = match (
            context_window,
            self.current_session_state.context_input_tokens,
        ) {
            (Some(context_window), Some(input_tokens)) => {
                Some(context_percent_used(input_tokens, context_window))
            }
            _ => None,
        };
        let (live_settings, _) = load_harness_settings_or_warn(&self.dirs);
        self.publish_event(
            None,
            Event::HarnessRoleSelected(HarnessRoleSelected {
                baseline_params: selected_model.as_ref().map(|model| {
                    baseline_params_for_selection(
                        &live_settings,
                        &self.provider_model_info,
                        &self.selected_role,
                        model,
                    )
                }),
                model_params: selected_model
                    .as_ref()
                    .map(|model| self.params_for_role_model(&self.selected_role, model))
                    .unwrap_or_default(),
                model: selected_model,
                context_window,
                role: self.selected_role.clone(),
            }),
        );
        self.publish_event(
            None,
            Event::HarnessContextUsageChanged(HarnessContextUsageChanged {
                input_tokens: self.current_session_state.context_input_tokens,
                cached_tokens: self.current_session_state.context_cached_tokens,
                percent_used: self.current_session_state.context_percent_used,
            }),
        );
        self.publish_event(
            None,
            Event::HarnessEffortsAvailable(tau_proto::HarnessEffortsAvailable {
                levels: effort_levels,
            }),
        );
        self.publish_event(
            None,
            Event::HarnessVerbositiesAvailable(tau_proto::HarnessVerbositiesAvailable {
                levels: verbosity_levels,
            }),
        );
        self.publish_event(
            None,
            Event::HarnessThinkingSummariesAvailable(
                tau_proto::HarnessThinkingSummariesAvailable {
                    levels: thinking_levels,
                },
            ),
        );
    }

    fn set_provider_models(&mut self, source_id: &str, models: Vec<ProviderModelInfo>) {
        if models.is_empty() {
            self.provider_models_by_extension.remove(source_id);
        } else {
            self.provider_models_by_extension
                .insert(source_id.to_owned(), models);
        }
        self.refresh_provider_models_and_publish_state();
    }

    #[cfg(test)]
    fn submit_user_prompt(
        &mut self,
        session_id: SessionId,
        text: String,
    ) -> Result<PromptSubmission, HarnessError> {
        if session_id != self.current_session_id {
            let reason = format!(
                "harness is bound to session `{}`; prompt for `{}` rejected",
                self.current_session_id.as_str(),
                session_id.as_str()
            );
            self.emit_info(&reason);
            return Ok(PromptSubmission::Rejected { reason });
        }
        let cid = self
            .agents
            .iter()
            .find_map(|(cid, conv)| {
                (conv.session_id == session_id
                    && conv.originator.is_user()
                    && conv.agent_id.is_some())
                .then_some(cid.clone())
            })
            .unwrap_or_else(|| {
                let role = self.selected_role.clone();
                self.create_durable_user_agent(session_id.clone(), &role)
            });
        self.preempt_blocking_ext_side_agents(&session_id);
        if !self.session_initialized(&session_id)
            || (self.selected_model.is_none() && self.provider_model_info.is_empty())
            || !self.turn_state.is_idle()
            || !self.extensions_all_ready()
        {
            if let Some(conv) = self.agents.get_mut(&cid) {
                conv.pending_prompts.push_back(PendingPrompt::user(text));
            }
            self.try_advance_queue();
            return Ok(PromptSubmission::Queued);
        }
        if self.dispatch_blocked_for(&cid) {
            if let Some(conv) = self.agents.get_mut(&cid) {
                conv.pending_prompts.push_back(PendingPrompt::user(text));
            }
            self.try_advance_queue();
            return Ok(PromptSubmission::Queued);
        }
        self.dispatch_prompt_for_agent(&cid, PendingPrompt::user(text))?;
        Ok(PromptSubmission::Dispatched)
    }

    fn submit_prompt_to_agent(
        &mut self,
        session_id: SessionId,
        agent_id: &str,
        prompt: impl Into<PendingPrompt>,
    ) -> Result<PromptSubmission, HarnessError> {
        let prompt = prompt.into();
        if session_id != self.current_session_id {
            let reason = format!(
                "harness is bound to session `{}`; prompt for `{}` rejected",
                self.current_session_id.as_str(),
                session_id.as_str()
            );
            self.emit_info(&reason);
            return Ok(PromptSubmission::Rejected { reason });
        }
        let Some(cid) = self.agent_routes.get(agent_id).cloned() else {
            self.emit_info(&format!("unknown agent `{agent_id}`"));
            return Ok(PromptSubmission::Rejected {
                reason: format!("unknown agent `{agent_id}`"),
            });
        };
        self.set_agent_state(agent_id, AgentState::Active);
        if !self.session_initialized(&session_id)
            || (self.selected_model.is_none() && self.provider_model_info.is_empty())
            || !self.turn_state.is_idle()
            || !self.extensions_all_ready()
        {
            if let Some(conv) = self.agents.get_mut(&cid) {
                conv.pending_prompts.push_back(prompt.clone());
            }
            self.publish_event(
                None,
                Event::AgentPromptQueued(AgentPromptQueued {
                    agent_id: crate::parse_agent_id(agent_id),
                    text: prompt.text,
                    message_class: prompt.message_class,
                }),
            );
            self.try_advance_queue();
            return Ok(PromptSubmission::Queued);
        }
        if self.dispatch_blocked_for(&cid) {
            if let Some(conv) = self.agents.get_mut(&cid) {
                conv.pending_prompts.push_back(prompt.clone());
            }
            self.publish_event(
                None,
                Event::AgentPromptQueued(AgentPromptQueued {
                    agent_id: crate::parse_agent_id(agent_id),
                    text: prompt.text,
                    message_class: prompt.message_class,
                }),
            );
            self.try_advance_queue();
            return Ok(PromptSubmission::Queued);
        }
        self.dispatch_prompt_for_agent(&cid, prompt)?;
        Ok(PromptSubmission::Dispatched)
    }

    /// Cancel every in-flight non-tool extension side agent
    /// (idle-summary and friends) so the agent's single prompt slot
    /// is free for the incoming user turn. Delegate sub-agents are
    /// left alone — they're part of an active user turn already and
    /// cancelling them would orphan the parent's tool call.
    ///
    /// Side effects per matching conversation: clear in-flight
    /// state, drop the spid from `prompt_agents`, mark it
    /// canceled, and publish a terminal prompt lifecycle event. A
    /// targeted `UiCancelPrompt` event is then published so the
    /// agent's retry-sleep wakes and aborts whatever it's currently
    /// processing.
    fn preempt_blocking_ext_side_agents(&mut self, session_id: &SessionId) {
        let to_cancel: Vec<(AgentId, SessionId, AgentPromptId, PromptOriginator)> = self
            .agents
            .iter()
            .filter_map(|(cid, conv)| {
                if conv.parent_tool_call_id.is_some() {
                    return None;
                }
                if !matches!(
                    conv.originator,
                    tau_proto::PromptOriginator::Extension { .. }
                ) {
                    return None;
                }
                let in_flight = conv.in_flight_prompt.clone()?;
                Some((
                    cid.clone(),
                    conv.session_id.clone(),
                    in_flight,
                    conv.originator.clone(),
                ))
            })
            .collect();

        if to_cancel.is_empty() {
            return;
        }

        for (cid, prompt_session_id, spid, originator) in &to_cancel {
            self.canceled_prompts.insert(spid.clone());
            if let Some(conv) = self.agents.get_mut(cid) {
                conv.in_flight_prompt = None;
                conv.pending_prompts.clear();
            }
            self.set_agent_turn_state(cid, AgentTurnState::Idle);
            self.release_start_agent_request(cid);
            self.publish_prompt_terminated(
                prompt_session_id.clone(),
                spid.clone(),
                AgentPromptTerminationReason::Canceled,
                originator.clone(),
            );
            self.prompt_agents.remove(spid);
            self.emit_info(&format!(
                "preempting side conv `{cid}` ({spid}) for incoming user prompt",
            ));
            // Targeted cancel per spid. A broadcast cancel
            // (`agent_prompt_id: None`) would also abort an
            // unrelated user/delegate prompt that happens to be
            // retry-sleeping on the agent side — the very prompt
            // we're trying to *unblock* by preempting these side
            // convs. Targeted form: the agent only aborts the
            // matching attempt, or records the spid in
            // `canceled_spids` if the prompt is still queued.
            self.publish_event(
                None,
                Event::UiCancelPrompt(UiCancelPrompt {
                    session_id: session_id.clone(),
                    target_agent_id: self
                        .target_agent_id_for_agent(cid)
                        .map(crate::parse_agent_id),
                    agent_prompt_id: Some(spid.clone()),
                }),
            );
        }
    }

    /// Renders the selected agent tree as one `harness.notice` line per node.
    /// Bound-session-only: refuses if `session_id` doesn't match.
    fn handle_tree_request(&mut self, session_id: &SessionId, target_agent_id: Option<&str>) {
        if session_id != &self.current_session_id {
            self.emit_info(&format!(
                "tree request for `{}` ignored; harness is bound to `{}`",
                session_id.as_str(),
                self.current_session_id.as_str()
            ));
            return;
        }
        let Some(cid) = self.runtime_agent_id_for_target_agent(target_agent_id) else {
            self.emit_info("tree request ignored: unknown agent");
            return;
        };
        let agent_id = self
            .target_agent_id_for_agent(&cid)
            .expect("agent has durable id");
        let lines: Vec<String> = match self.agent_store.agent(&agent_id) {
            Some(tree) if !tree.nodes().is_empty() => {
                let selected_head = self.agents.get(&cid).and_then(|conv| conv.head);
                tree.nodes()
                    .iter()
                    .map(|node| {
                        let marker = if Some(node.id) == selected_head {
                            '*'
                        } else {
                            ' '
                        };
                        let parent = node
                            .parent_id
                            .map(|p| format!("<- {}", p.get()))
                            .unwrap_or_else(|| "(root)".to_owned());
                        let preview = render_entry_preview(&node.entry);
                        format!(
                            "  {:>3} {} {:>8}  {}",
                            node.id.get(),
                            marker,
                            parent,
                            preview
                        )
                    })
                    .collect()
            }
            _ => {
                self.emit_info(&format!("agent `{}` has no entries yet", agent_id));
                return;
            }
        };
        for line in lines {
            self.emit_info(&line);
        }
    }

    /// Validates a `UiNavigateTree` request against the bound session and
    /// resolves the durable agent-owned head-move target.
    fn validate_navigate_tree_target(
        &mut self,
        session_id: &SessionId,
        target_agent_id: Option<&str>,
        node_id: u64,
    ) -> Option<(AgentId, tau_proto::AgentId, tau_core::NodeId)> {
        if session_id != &self.current_session_id {
            self.emit_info(&format!(
                "navigate ignored: harness is bound to `{}`",
                self.current_session_id.as_str()
            ));
            return None;
        }
        let Some(cid) = self.runtime_agent_id_for_target_agent(target_agent_id) else {
            self.emit_info("navigate ignored: unknown agent");
            return None;
        };
        let agent_id: tau_proto::AgentId = crate::parse_agent_id(
            self.target_agent_id_for_agent(&cid)
                .expect("agent has durable id"),
        );
        let node_id = tau_core::NodeId::new(node_id);
        let valid = self
            .agent_store
            .agent(agent_id.as_str())
            .and_then(|t| t.node(node_id))
            .is_some();
        if !valid {
            self.emit_info(&format!("no node `{}` in session", node_id.get()));
            return None;
        }
        Some((cid, agent_id, node_id))
    }

    /// Tear down the current session and bind the harness to a new one.
    ///
    /// Pi-style: emit `SessionShutdown` for the old, drop in-flight
    /// prompts, swap the bound id, then run a fresh `start_session_init`
    /// for the new id with the given reason. Extension processes are
    /// kept across sessions (they're not respawned); extensions that
    /// hold per-session state subscribe to `session.shutdown` to
    /// flush/clean up.
    fn switch_session(
        &mut self,
        new_session_id: SessionId,
        reason: tau_proto::SessionStartReason,
    ) -> Result<(), HarnessError> {
        if new_session_id == self.current_session_id
            && !matches!(reason, tau_proto::SessionStartReason::New)
        {
            self.emit_info(&format!("already on session `{}`", new_session_id.as_str()));
            return Ok(());
        }

        let old_id = self.current_session_id.clone();
        self.publish_event(
            None,
            Event::SessionShutdown(tau_proto::SessionShutdown { session_id: old_id }),
        );

        // Drop in-flight work bound to the old session. Pending prompts
        // for it are abandoned (the user explicitly switched away), and
        // each agent's per-turn state is reset.
        self.turn_state = TurnState::Idle;
        let agent_ids = self.agents.keys().cloned().collect::<Vec<_>>();
        for cid in agent_ids {
            if let Some(conv) = self.agents.get_mut(&cid) {
                conv.pending_prompts.clear();
                conv.in_flight_prompt = None;
            }
            self.set_agent_turn_state(&cid, AgentTurnState::Idle);
        }
        self.tool_turn.clear();
        self.tool_agents.clear();
        self.pending_tools.clear();
        self.completed_tool_calls.clear();
        self.pending_tool_providers.clear();
        self.pending_action_invocations.clear();
        self.prompt_agents.clear();
        self.pending_provider_prompts.clear();
        self.prompt_models.clear();
        self.prompt_tool_specs.clear();
        self.prompt_tool_call_prompts.clear();
        self.suppressed_background_completion_prompts.clear();
        self.background_completion_targets.clear();
        self.canceled_prompts.clear();
        self.pending_notices.restore_sessions.clear();
        self.pending_notices.restore_background_notices.clear();
        self.pending_notices.tool_availability.clear();
        self.pending_notices.unavailable_tools_delivered.clear();
        self.pending_start_agent_requests.clear();
        self.clear_session_agent_context();
        self.subagents = SubagentToolState::default();

        // Token and context accounting are session-scoped. Reset them
        // before `SessionStarted` so clients recreating status UI for
        // the new session do not inherit the previous transcript's
        // cumulative totals.
        self.current_session_state = CurrentSessionState::default();

        // Drop agents from the previous bound session. New user agents are
        // created explicitly by `UiCreateAgent`/first prompt in the new session.
        self.agents.clear();
        self.agent_routes.clear();
        self.agent_states.clear();
        self.stopped_agent_ids.clear();

        self.current_session_id = new_session_id.clone();
        self.current_session_start_reason = reason;
        if matches!(reason, tau_proto::SessionStartReason::Resume) {
            self.rehydrate_agents_from_session();
        }
        self.publish_delegate_roles_context();

        // Record session metadata + acquire the new session dir flock before
        // anyone tries to write to its membership log.
        self.store.record_session_meta(new_session_id.as_str())?;

        // Send the new debug log to the new session's dir, so each
        // session is self-contained.
        let _ = self.enable_debug_log(&self.sessions_dir().join(new_session_id.as_str()));
        self.start_session_init(new_session_id.clone(), reason);
        self.publish_current_session_dir();
        Ok(())
    }

    fn publish_current_session_dir(&mut self) {
        self.publish_event(
            None,
            Event::HarnessSessionDir(tau_proto::HarnessSessionDir {
                session_id: self.current_session_id.clone(),
                path: self.sessions_dir().join(self.current_session_id.as_str()),
                status: session_dir_status_from_reason(self.current_session_start_reason),
            }),
        );
    }

    fn sessions_dir(&self) -> PathBuf {
        // The harness doesn't currently store the sessions dir directly;
        // derive it from the session store's location. SessionStore
        // exposes its root via the `sessions_dir()` accessor.
        self.store.sessions_dir().to_path_buf()
    }

    fn loaded_agent_ids_for_session(&self, session_id: &SessionId) -> Vec<tau_proto::AgentId> {
        self.store
            .session(session_id.as_str())
            .map(|membership| membership.loaded_agents().into_iter().cloned().collect())
            .unwrap_or_default()
    }

    fn any_loaded_agent_event(
        &self,
        session_id: &SessionId,
        matches_event: impl Fn(&Event) -> bool,
    ) -> bool {
        self.loaded_agent_ids_for_session(session_id)
            .into_iter()
            .filter_map(|agent_id| match self.agent_store.agent_events(agent_id.as_str()) {
                Ok(events) => Some(events),
                Err(error) => {
                    tracing::warn!(target: "tau_harness", %agent_id, %error, "failed to load agent events while checking restored prompts");
                    None
                }
            })
            .flatten()
            .any(|entry| matches_event(&entry.event))
    }

    fn internal_prompt_already_persisted(&self, session_id: &SessionId, text: &str) -> bool {
        self.any_loaded_agent_event(session_id, |event| {
            event_is_internal_prompt_text(event, text)
        })
    }

    fn restore_notice_already_persisted(&self, session_id: &SessionId) -> bool {
        self.any_loaded_agent_event(session_id, event_is_internal_restore_notice)
    }

    fn last_recorded_session_event_at(
        &self,
        session_id: &SessionId,
    ) -> Option<tau_proto::UnixMicros> {
        self.loaded_agent_ids_for_session(session_id)
            .into_iter()
            .filter_map(|agent_id| match self.agent_store.agent_events(agent_id.as_str()) {
                Ok(events) => Some(events),
                Err(error) => {
                    tracing::warn!(target: "tau_harness", %agent_id, %error, "failed to load agent events while checking restored timestamps");
                    None
                }
            })
            .flatten()
            .filter_map(|entry| (entry.recorded_at.get() != 0).then_some(entry.recorded_at))
            .max_by_key(|recorded_at| recorded_at.get())
    }

    fn queue_restore_notice_for_resumed_session(&mut self, session_id: &SessionId) {
        if session_id != &self.current_session_id {
            return;
        }
        if self.restore_notice_already_persisted(session_id) {
            self.pending_notices.restore_sessions.remove(session_id);
            return;
        }
        let last_recorded_at = self.last_recorded_session_event_at(session_id);
        self.pending_notices
            .restore_sessions
            .insert(session_id.clone(), last_recorded_at);
    }

    fn queue_restore_background_notices_for_resumed_session(&mut self, session_id: &SessionId) {
        if session_id != &self.current_session_id {
            return;
        }
        let mut seen = HashSet::new();
        let mut notices = Vec::new();
        for cid in self.restored_agent_ids(session_id) {
            for state in self.restored_background_tool_states_for_agent(&cid) {
                let Some(tau_core::BackgroundToolCompletion::Error(error)) = state.completion
                else {
                    continue;
                };
                let notice = restored_background_tool_call_error_message(&error.call_id);
                if error.message != notice || !seen.insert(notice.clone()) {
                    continue;
                }
                if self.internal_prompt_already_persisted(session_id, &notice) {
                    continue;
                }
                notices.push(notice);
            }
        }
        if notices.is_empty() {
            self.pending_notices
                .restore_background_notices
                .remove(session_id);
        } else {
            self.pending_notices
                .restore_background_notices
                .insert(session_id.clone(), notices);
        }
    }

    fn mark_tool_unavailable_for_notice(
        &mut self,
        internal_name: ToolName,
        visible_name: ToolName,
    ) {
        let internal_name = internal_name.into_string();
        if matches!(
            self.pending_notices.tool_availability.get(&internal_name),
            Some(PendingToolAvailabilityNotice::Unavailable { .. })
        ) {
            return;
        }
        if matches!(
            self.pending_notices.tool_availability.get(&internal_name),
            Some(PendingToolAvailabilityNotice::AvailableAgain { .. })
        ) {
            self.pending_notices
                .tool_availability
                .remove(&internal_name);
            return;
        }
        if self
            .pending_notices
            .unavailable_tools_delivered
            .contains_key(&internal_name)
        {
            return;
        }
        self.pending_notices.tool_availability.insert(
            internal_name,
            PendingToolAvailabilityNotice::Unavailable { visible_name },
        );
    }

    fn mark_tool_available_for_notice(&mut self, internal_name: ToolName, visible_name: ToolName) {
        let internal_name = internal_name.into_string();
        if matches!(
            self.pending_notices.tool_availability.get(&internal_name),
            Some(PendingToolAvailabilityNotice::Unavailable { .. })
        ) {
            self.pending_notices
                .tool_availability
                .remove(&internal_name);
            return;
        }
        if self
            .pending_notices
            .unavailable_tools_delivered
            .contains_key(&internal_name)
        {
            self.pending_notices.tool_availability.insert(
                internal_name,
                PendingToolAvailabilityNotice::AvailableAgain { visible_name },
            );
        }
    }

    fn take_pending_tool_availability_prompts_for_user_prompt(&mut self) -> Vec<PendingPrompt> {
        let pending = std::mem::take(&mut self.pending_notices.tool_availability);
        let mut prompts = Vec::new();
        for (internal_name, notice) in pending {
            match &notice {
                PendingToolAvailabilityNotice::Unavailable { visible_name } => {
                    self.pending_notices
                        .unavailable_tools_delivered
                        .insert(internal_name, visible_name.clone());
                }
                PendingToolAvailabilityNotice::AvailableAgain { .. } => {
                    self.pending_notices
                        .unavailable_tools_delivered
                        .remove(&internal_name);
                }
            }
            prompts.push(PendingPrompt::internal(notice.prompt_text()));
        }
        prompts
    }

    /// Consume pending internal notices before the next real user prompt on the
    /// current session.
    pub(crate) fn take_pending_restore_prompts_for_user_prompt(
        &mut self,
        cid: &AgentId,
    ) -> Vec<PendingPrompt> {
        let Some(session_id) = self.agents.get(cid).map(|conv| conv.session_id.clone()) else {
            return Vec::new();
        };
        if session_id != self.current_session_id {
            return Vec::new();
        }

        let mut prompts = Vec::new();
        if self.restore_notice_already_persisted(&session_id) {
            self.pending_notices.restore_sessions.remove(&session_id);
        } else if let Some(last_recorded_at) =
            self.pending_notices.restore_sessions.remove(&session_id)
        {
            prompts.push(PendingPrompt::internal(restore_notice_prompt(
                last_recorded_at,
                tau_proto::UnixMicros::now(),
            )));
        }

        if let Some(notices) = self
            .pending_notices
            .restore_background_notices
            .remove(&session_id)
        {
            for notice in notices {
                if !self.internal_prompt_already_persisted(&session_id, &notice) {
                    prompts.push(PendingPrompt::internal(notice));
                }
            }
        }
        prompts.extend(self.take_pending_tool_availability_prompts_for_user_prompt());
        prompts
    }

    fn repair_restored_foreground_tool_calls(&mut self, session_id: &SessionId) -> usize {
        if session_id != &self.current_session_id {
            return 0;
        }
        let mut count = 0;
        for cid in self.restored_agent_ids(session_id) {
            let Some(head) = self.agents.get(&cid).map(|conv| conv.head) else {
                continue;
            };
            let calls: Vec<ToolCallItem> = self
                .agents
                .get(&cid)
                .and_then(|conv| conv.agent_id.as_deref())
                .and_then(|agent_id| self.agent_store.agent(agent_id))
                .map(|tree| {
                    tree.unresolved_foreground_tool_calls_from(head)
                        .into_iter()
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            count += calls.len();
            for call in calls {
                self.tool_agents.insert(call.call_id.clone(), cid.clone());
                let error = ToolError {
                    call_id: call.call_id.clone(),
                    tool_name: call.name,
                    tool_type: call.tool_type,
                    message: restored_tool_call_error_message(&call.call_id),
                    details: None,
                    originator: tau_proto::PromptOriginator::User,

                    display: None,
                };
                self.publish_terminal_tool_error(Some(&cid), Some(HARNESS_CONNECTION_ID), error);
            }
        }
        count
    }

    fn restored_background_tool_states_for_agent(
        &self,
        cid: &AgentId,
    ) -> Vec<tau_core::BackgroundToolCallState> {
        let Some(head) = self.agents.get(cid).map(|conv| conv.head) else {
            return Vec::new();
        };
        let Some(agent_id) = self
            .agents
            .get(cid)
            .and_then(|conv| conv.agent_id.as_deref())
        else {
            return Vec::new();
        };
        let Ok(events) = self.agent_store.agent_events(agent_id) else {
            tracing::warn!(target: "tau_harness", %agent_id, "failed to load restored agent events");
            return Vec::new();
        };
        self.agent_store
            .agent(agent_id)
            .map(|tree| tree.background_tool_calls_from(head, &events))
            .unwrap_or_default()
    }

    fn restored_agent_ids(&self, session_id: &SessionId) -> Vec<AgentId> {
        if session_id != &self.current_session_id {
            return Vec::new();
        }
        self.agents
            .iter()
            .filter_map(|(cid, conv)| {
                (conv.session_id == *session_id && conv.agent_id.is_some()).then_some(cid.clone())
            })
            .collect()
    }

    fn seed_restored_wait_background_completions(&mut self, session_id: &SessionId) {
        for cid in self.restored_agent_ids(session_id) {
            for state in self.restored_background_tool_states_for_agent(&cid) {
                self.tool_agents
                    .insert(state.placeholder.call_id.clone(), cid.clone());
                match state.completion {
                    Some(tau_core::BackgroundToolCompletion::Result(result)) => {
                        self.record_wait_background_result(result);
                    }
                    Some(tau_core::BackgroundToolCompletion::Error(error)) => {
                        self.record_wait_background_error(error);
                    }
                    None => {}
                }
            }
        }
    }

    fn repair_restored_background_tool_calls(&mut self, session_id: &SessionId) -> usize {
        if session_id != &self.current_session_id {
            return 0;
        }
        let mut count = 0;
        for cid in self.restored_agent_ids(session_id) {
            let Some(head) = self.agents.get(&cid).map(|conv| conv.head) else {
                continue;
            };
            let Some(agent_id) = self
                .agents
                .get(&cid)
                .and_then(|conv| conv.agent_id.as_deref())
            else {
                continue;
            };
            let Ok(events) = self.agent_store.agent_events(agent_id) else {
                tracing::warn!(target: "tau_harness", %agent_id, "failed to load restored agent events");
                continue;
            };
            let calls = self
                .agent_store
                .agent(agent_id)
                .map(|tree| tree.unresolved_background_tool_calls_from(head, &events))
                .unwrap_or_default();
            count += calls.len();
            for call in calls {
                self.tool_agents.insert(call.call_id.clone(), cid.clone());
                let error = ToolBackgroundError {
                    call_id: call.call_id.clone(),
                    tool_name: call.tool_name,
                    tool_type: call.tool_type,
                    message: restored_background_tool_call_error_message(&call.call_id),
                    details: None,
                    originator: call.originator,

                    display: None,
                };
                self.publish_terminal_background_error(&cid, Some(HARNESS_CONNECTION_ID), error);
            }
        }
        count
    }

    fn repair_restored_session_tool_state(&mut self, session_id: &SessionId) {
        self.repair_restored_foreground_tool_calls(session_id);
        self.repair_restored_background_tool_calls(session_id);
        self.seed_restored_wait_background_completions(session_id);
        self.queue_restore_background_notices_for_resumed_session(session_id);
    }

    pub(crate) fn start_session_init(
        &mut self,
        session_id: SessionId,
        reason: tau_proto::SessionStartReason,
    ) {
        if matches!(reason, tau_proto::SessionStartReason::Resume) {
            self.queue_restore_notice_for_resumed_session(&session_id);
        }
        let waiting_on = self.session_init_provider_ids();
        self.publish_event(
            None,
            Event::SessionStarted(tau_proto::SessionStarted {
                session_id: session_id.clone(),
                reason,
            }),
        );
        if waiting_on.is_empty() {
            if let Err(error) = self.complete_session_init(session_id, reason) {
                self.emit_harness_failure(&format!("failed to initialize session: {error}"));
                self.turn_state = TurnState::Idle;
            }
            return;
        }

        for source_id in &waiting_on {
            self.remove_discovered_context(source_id.as_str());
        }

        self.turn_state = TurnState::InitializingSession {
            session_id,
            reason,
            waiting_on,
        };
    }

    fn handle_extension_context_ready(
        &mut self,
        source_id: &str,
        ready: tau_proto::ExtensionContextReady,
    ) -> Result<(), HarnessError> {
        if ready.session_id != self.current_session_id {
            return Ok(());
        }
        let source_id = tau_proto::ConnectionId::from(source_id);
        let completed_session = match &mut self.turn_state {
            TurnState::InitializingSession {
                session_id,
                reason,
                waiting_on,
            } => {
                let removed = waiting_on.remove(&source_id);
                if removed && waiting_on.is_empty() {
                    Some((session_id.clone(), *reason))
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some((session_id, reason)) = completed_session {
            self.complete_session_init(session_id, reason)?;
        }
        if let Some(waiting_on) = self.pending_agent_context_ready.get_mut(&ready.agent_id) {
            waiting_on.remove(&source_id);
            if waiting_on.is_empty() {
                self.pending_agent_context_ready.remove(&ready.agent_id);
                self.drain_publish_idle_dispatches();
                self.try_advance_queue();
            }
        }
        Ok(())
    }

    fn maybe_complete_session_init_for_disconnect(&mut self, connection_id: &str) {
        let completed_session = match &mut self.turn_state {
            TurnState::InitializingSession {
                session_id,
                reason,
                waiting_on,
            } => {
                let removed = waiting_on.remove(connection_id);
                if removed && waiting_on.is_empty() {
                    Some((session_id.clone(), *reason))
                } else {
                    None
                }
            }
            _ => None,
        };

        if let Some((session_id, reason)) = completed_session
            && let Err(error) = self.complete_session_init(session_id, reason)
        {
            self.emit_harness_failure(&format!("failed to initialize session: {error}"));
            self.turn_state = TurnState::Idle;
        }
    }

    fn complete_session_init(
        &mut self,
        session_id: SessionId,
        reason: tau_proto::SessionStartReason,
    ) -> Result<(), HarnessError> {
        // AGENTS.md and skill context is agent-scoped. Session init only waits
        // for discovery; the discovered context is injected when a durable agent
        // is explicitly created from the UI's current role/cwd state.
        self.initialized_sessions.insert(session_id.clone());
        // Catch up before repair: repair appends its synthetic tool errors to
        // the durable log as it publishes them live, so running it first
        // would deliver each error twice (live, then replay-marked) to peers
        // subscribed before init.
        self.catch_up_subscribers_after_session_init();
        if matches!(reason, tau_proto::SessionStartReason::Resume) {
            self.repair_restored_session_tool_state(&session_id);
        }
        self.request_prompt_prewarm(&session_id);
        self.turn_state = TurnState::Idle;
        self.try_advance_queue();
        Ok(())
    }

    fn request_prompt_prewarm(&mut self, session_id: &SessionId) {
        tracing::debug!(
            target: "harness",
            session_id = %session_id,
            "skipping prompt prewarm: no agent has been created yet",
        );
    }

    // -----------------------------------------------------------------------
    // Agent prompt assembly
    // -----------------------------------------------------------------------

    fn insert_agents_context_for_agent(&mut self, cid: &AgentId, agent_id: &str) {
        if self.discovered_agents_files.is_empty() {
            return;
        }
        let text = render_agents_context_message(self.discovered_agents_files.iter());
        self.publish_event_for_agent(
            cid,
            None,
            Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
                agent_id: crate::parse_agent_id(agent_id),
                text,
                message_class: tau_proto::PromptMessageClass::User,
            }),
        );
    }

    /// Persist a user-initiated `!` shell command's output as a
    /// tagged user message so the agent sees it in the next prompt.
    ///
    /// The XML-ish `<user_shell>` envelope lets the model reliably
    /// distinguish output the user pasted vs. output from its own
    /// tool calls, and survives round-tripping through conversation
    /// assembly.
    fn inject_user_shell_output(&mut self, finished: &tau_proto::ShellCommandFinished) {
        let exit = finished
            .exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| if finished.cancelled { "cancelled" } else { "?" }.to_owned());
        let text = format!(
            "<user_shell command={:?} exit_code={:?}>\n{}\n</user_shell>",
            finished.command, exit, finished.output,
        );
        let Some((cid, agent_id)) = self.resolve_shell_output_target_agent(finished) else {
            return;
        };
        let event = Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
            agent_id,
            text,
            message_class: tau_proto::PromptMessageClass::User,
        });
        // Stamp the publish with the target agent so the fold lands on the
        // branch whose transcript owned the command and the post-commit hook
        // syncs the conversation head.
        self.publish_event_for_agent(&cid, None, event);
    }

    /// Convenience wrapper that dispatches a prompt for the first user agent in
    /// the requested session. Used by tests that want a quick "send the next
    /// prompt" without going through the full dispatch pipeline.
    #[cfg(test)]
    fn send_prompt_to_agent(&mut self, session_id: &str) -> AgentPromptId {
        let cid = self
            .agents
            .iter()
            .find(|(_, conv)| conv.session_id.as_str() == session_id && conv.originator.is_user())
            .map(|(cid, _)| cid.clone())
            .expect("test requires an existing user agent");
        self.send_prompt_to_agent_for(&cid)
            .expect("test prompt requires a selected model")
    }

    fn set_agent_state(&mut self, agent_id: &str, state: AgentState) {
        self.agent_states.insert(agent_id.to_owned(), state);
    }

    fn set_agent_turn_state(&mut self, cid: &AgentId, state: AgentTurnState) {
        let new_state = agent_runtime_state_for_turn(&state);
        let changed_agent_id = self.agents.get(cid).and_then(|agent| {
            let old_state = agent_runtime_state_for_turn(&agent.turn_state);
            if old_state == new_state {
                return None;
            }
            agent.agent_id.clone()
        });

        if let Some(agent) = self.agents.get_mut(cid) {
            agent.turn_state = state;
        }

        let Some(agent_id) = changed_agent_id else {
            return;
        };
        self.publish_event(
            Some(HARNESS_CONNECTION_ID),
            Event::AgentState(tau_proto::AgentStateChanged {
                agent_id: crate::parse_agent_id(&agent_id),
                state: new_state,
            }),
        );
    }

    fn remove_agent(&mut self, cid: &AgentId) -> Option<Agent> {
        if let Some(conv) = self.agents.get(cid)
            && let Some(agent_id) = conv.agent_id.clone()
        {
            let session_id = conv.session_id.clone();
            self.unload_agent_from_session_if_loaded(&session_id, &agent_id);
            self.agent_routes.remove(&agent_id);
            self.agent_states.remove(&agent_id);
            self.stopped_agent_ids.insert(agent_id);
        }
        self.agents.remove(cid)
    }

    fn unload_agent_from_session_if_loaded(&mut self, session_id: &SessionId, agent_id: &str) {
        if session_id != &self.current_session_id {
            return;
        }
        let agent_id_proto: tau_proto::AgentId = crate::parse_agent_id(agent_id);
        let already_loaded = self.session_loaded_agents.contains(&agent_id_proto)
            || match self.store.load_session(session_id.as_str()) {
                Ok(Some(membership)) => membership.contains_agent(&agent_id_proto),
                Ok(None) => false,
                Err(error) => {
                    self.emit_harness_failure(&format!(
                        "failed to load session while unloading agent `{agent_id}`: {error}"
                    ));
                    false
                }
            };
        if already_loaded {
            self.publish_event(
                None,
                Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
                    session_id: session_id.clone(),
                    agent_id: agent_id_proto.clone(),
                }),
            );
            self.session_loaded_agents.remove(&agent_id_proto);
        }
    }

    fn rehydrate_agents_from_session(&mut self) {
        let loaded_agents: Vec<tau_proto::AgentId> =
            match self.store.load_session(self.current_session_id.as_str()) {
                Ok(Some(membership)) => membership.loaded_agents().into_iter().cloned().collect(),
                Ok(None) => return,
                Err(error) => {
                    self.emit_harness_failure(&format!(
                        "failed to load session during restore: {error}"
                    ));
                    return;
                }
            };
        for agent_id in loaded_agents {
            let agent_id_string = agent_id.to_string();
            if let Err(error) = self.agent_store.load_agent(agent_id.as_str()) {
                self.emit_harness_failure(&format!(
                    "failed to load restored agent `{agent_id}`: {error}"
                ));
                continue;
            }
            let head = self
                .agent_head_moved_from_log(agent_id.as_str())
                .or_else(|| {
                    self.agent_store
                        .agent(agent_id.as_str())
                        .and_then(|tree| tree.head())
                });
            let cid: AgentId = crate::parse_agent_id(&agent_id_string);
            let meta = self
                .agent_store
                .agent_meta(agent_id.as_str())
                .ok()
                .flatten();
            let display_name = self
                .agent_store
                .agent(agent_id.as_str())
                .and_then(|tree| tree.display_name().map(str::to_owned))
                .or_else(|| meta.and_then(|meta| meta.display_name));
            let role = self.agent_role_from_log(agent_id.as_str());
            let originator = self.agent_originator_from_log(agent_id.as_str());
            if let Some(conv) = self.agents.get_mut(&cid) {
                conv.agent_id = Some(agent_id_string.clone());
                conv.head = head;
                conv.role = role.clone();
                conv.display_name = display_name.clone();
            } else {
                let mut conv = Agent::new(
                    cid.clone(),
                    self.current_session_id.clone(),
                    originator,
                    head,
                    None,
                );
                conv.agent_id = Some(agent_id_string.clone());
                conv.role = role.clone();
                conv.display_name = display_name.clone();
                self.agents.insert(cid.clone(), conv);
            }
            self.agent_routes.insert(agent_id_string.clone(), cid);
            self.session_loaded_agents.insert(agent_id.clone());
            self.agent_states
                .insert(agent_id_string, AgentState::Active);
        }
    }

    fn agent_head_moved_from_log(&self, agent_id: &str) -> Option<NodeId> {
        self.agent_store
            .agent_events(agent_id)
            .inspect_err(|error| {
                tracing::warn!(target: "tau_harness", %agent_id, %error, "failed to load agent events for head restore");
            })
            .ok()?
            .into_iter()
            .filter_map(|record| match record.event {
                Event::AgentHeadMoved(moved) => Some(moved.node_id),
                _ => None,
            })
            .next_back()
    }

    fn agent_originator_from_log(&self, agent_id: &str) -> tau_proto::PromptOriginator {
        self.agent_store
            .agent_events(agent_id)
            .inspect_err(|error| {
                tracing::warn!(target: "tau_harness", %agent_id, %error, "failed to load agent events for originator restore");
            })
            .ok()
            .and_then(|events| {
                events.into_iter().find_map(|record| match record.event {
                    Event::AgentPromptSubmitted(submitted) => Some(submitted.originator),
                    Event::ProviderResponseFinished(finished) => Some(finished.originator),
                    Event::ProviderToolResult(result) => Some(result.originator),
                    Event::ToolBackgroundResult(result) => Some(result.originator),
                    Event::ToolBackgroundError(error) => Some(error.originator),
                    _ => None,
                })
            })
            .unwrap_or_else(|| tau_proto::PromptOriginator::Extension {
                name: HARNESS_CONNECTION_ID.into(),
                query_id: format!("restored-{agent_id}"),
            })
    }

    fn agent_role_from_log(&self, agent_id: &str) -> Option<String> {
        self.agent_store
            .agent_events(agent_id)
            .inspect_err(|error| {
                tracing::warn!(target: "tau_harness", %agent_id, %error, "failed to load agent events for role restore");
            })
            .ok()?
            .into_iter()
            .find_map(|record| match record.event {
                Event::AgentStarted(started) => Some(started.role),
                _ => None,
            })
            .filter(|role| self.available_roles.contains_key(role))
    }

    fn role_group_name_for_role(&self, role: &str) -> String {
        self.available_role_groups
            .iter()
            .find(|group| group.roles.iter().any(|group_role| group_role == role))
            .map(|group| group.name.clone())
            .unwrap_or_else(|| role.to_owned())
    }

    fn display_name_for_new_agent(
        &mut self,
        agent_id: &str,
        role: &str,
        task_name: Option<&str>,
    ) -> Option<String> {
        let fallback = normalize_display_name(task_name);
        let Some(template) = self.agent_display_name_template.clone() else {
            return fallback;
        };
        let role_group = self.role_group_name_for_role(role);
        match render_agent_template(
            &template,
            role,
            &role_group,
            agent_id,
            task_name,
            0,
            &mut self.agent_id_rng,
        ) {
            Ok(rendered) => normalize_display_name(Some(&rendered)).or(fallback),
            Err(error) => {
                self.emit_info(&format!(
                    "agent display name template failed to render: {error}; falling back to request display name"
                ));
                fallback
            }
        }
    }

    pub(crate) fn mint_available_agent_id_for_role(&mut self, role: &str) -> String {
        let template = self.agent_id_template.clone();
        let mut warnings = Vec::new();
        let role_group = self.role_group_name_for_role(role);
        let agent_routes = &self.agent_routes;
        let stopped_agent_ids = &self.stopped_agent_ids;
        let agent_store = &self.agent_store;
        let pending_start_agent_requests = &self.pending_start_agent_requests;
        let agent_id = mint_available_agent_id_for_role_with(
            role,
            &role_group,
            &template,
            |agent_id| {
                agent_routes.contains_key(agent_id)
                    || stopped_agent_ids.contains(agent_id)
                    || agent_store.agent_exists(agent_id)
                    || pending_start_agent_requests
                        .iter()
                        .any(|pending| pending.agent_id == agent_id)
            },
            &mut self.agent_id_rng,
            |kind, warning| warnings.push((kind, warning)),
        );
        for (kind, warning) in warnings {
            self.emit_agent_id_template_warning(kind, warning);
        }
        agent_id
    }

    fn emit_agent_id_template_warning(
        &mut self,
        kind: AgentIdTemplateKind,
        warning: AgentIdMintWarning,
    ) {
        let source = match kind {
            AgentIdTemplateKind::Configured => "configured",
            AgentIdTemplateKind::Default => "default",
        };
        let message = match warning {
            AgentIdMintWarning::RenderFailed { error } => {
                format!(
                    "{source} agent id template failed to render: {error}; falling back to default template"
                )
            }
            AgentIdMintWarning::InvalidRendered { candidate, error } => format!(
                "{source} agent id template rendered invalid id `{candidate}`: {error}; falling back to default template"
            ),
            AgentIdMintWarning::CollisionsExceeded { attempts } => format!(
                "{source} agent id template failed to generate a unique id after {attempts} attempts; falling back to default template"
            ),
        };
        self.emit_info_important(&message);
    }

    pub(crate) fn create_durable_user_agent(
        &mut self,
        session_id: SessionId,
        role: &str,
    ) -> AgentId {
        self.create_durable_user_agent_with_parent(session_id, role, None, Vec::new())
    }

    pub(crate) fn create_durable_user_agent_with_parent(
        &mut self,
        session_id: SessionId,
        role: &str,
        parent_cid: Option<AgentId>,
        metadata: Vec<tau_proto::AgentInitialMetadata>,
    ) -> AgentId {
        let agent_id = self.mint_available_agent_id_for_role(role);
        let display_name = self.display_name_for_new_agent(&agent_id, role, None);
        let cid: AgentId = crate::parse_agent_id(&agent_id);
        let mut conv = Agent::new(
            cid.clone(),
            session_id,
            tau_proto::PromptOriginator::User,
            None,
            None,
        );
        conv.role = Some(role.to_owned());
        conv.parent_agent_id = parent_cid;
        conv.agent_id = Some(agent_id.clone());
        conv.display_name = display_name;
        self.agents.insert(cid.clone(), conv);
        self.publish_delegate_roles_context();
        let _ = self.agent_store.record_agent_meta(&agent_id);
        self.ensure_loaded_agent_for_agent_with_metadata(&cid, &agent_id, metadata);
        self.insert_agents_context_for_agent(&cid, &agent_id);
        cid
    }

    pub(crate) fn ensure_agent_id_for_agent(&mut self, cid: &AgentId) -> Option<String> {
        if let Some(agent_id) = self.agents.get(cid)?.agent_id.clone() {
            self.ensure_loaded_agent_for_agent(cid, &agent_id);
            if self.agent_states.get(&agent_id).copied() != Some(AgentState::ActiveDelegated) {
                self.set_agent_state(&agent_id, AgentState::Active);
            }
            return Some(agent_id);
        }
        let role = self
            .agents
            .get(cid)
            .map(|conv| self.role_name_for_agent(conv))?;
        let agent_id = self.mint_available_agent_id_for_role(&role);
        let display_name = self.display_name_for_new_agent(&agent_id, &role, None);
        if let Some(conv) = self.agents.get_mut(cid) {
            conv.agent_id = Some(agent_id.clone());
            if normalize_display_name(conv.display_name.as_deref()).is_none() {
                conv.display_name = display_name;
            }
        }
        self.ensure_loaded_agent_for_agent(cid, &agent_id);
        self.set_agent_state(&agent_id, AgentState::Active);
        Some(agent_id)
    }

    fn ensure_loaded_agent_for_agent(&mut self, cid: &AgentId, agent_id: &str) {
        self.ensure_loaded_agent_for_agent_with_metadata(cid, agent_id, Vec::new());
    }

    fn ensure_loaded_agent_for_agent_with_metadata(
        &mut self,
        cid: &AgentId,
        agent_id: &str,
        initial_metadata: Vec<tau_proto::AgentInitialMetadata>,
    ) {
        self.stopped_agent_ids.remove(agent_id);
        self.agent_routes.insert(agent_id.to_owned(), cid.clone());
        let role = self
            .agents
            .get(cid)
            .map(|conv| self.role_name_for_agent(conv));
        let _ = self.agent_store.record_agent_meta(agent_id);
        let agent_id_proto: tau_proto::AgentId = crate::parse_agent_id(agent_id);
        let prompt_index_initialized = self
            .agents
            .get(cid)
            .is_some_and(|agent| agent.prompt_index_initialized);
        if !prompt_index_initialized {
            let next_prompt_index = match self.agent_store.load_agent(agent_id) {
                Ok(Some(tree)) => tree.materialized_prompt_count(),
                Ok(None) => 0,
                Err(error) => {
                    self.emit_harness_failure(&format!(
                        "failed to load agent `{agent_id}`: {error}"
                    ));
                    0
                }
            };
            if let Some(agent) = self.agents.get_mut(cid) {
                agent.next_prompt_index = next_prompt_index;
                agent.prompt_index_initialized = true;
            }
        }
        let already_loaded = self.session_loaded_agents.contains(&agent_id_proto)
            || match self.store.load_session(self.current_session_id.as_str()) {
                Ok(Some(membership)) => membership.contains_agent(&agent_id_proto),
                Ok(None) => false,
                Err(error) => {
                    self.emit_harness_failure(&format!(
                        "failed to load session while ensuring agent `{agent_id}`: {error}"
                    ));
                    false
                }
            };
        if !already_loaded {
            self.session_loaded_agents.insert(agent_id_proto.clone());
            let waiting_on = self.agent_context_provider_ids(agent_id_proto.clone());
            if !waiting_on.is_empty() {
                self.pending_agent_context_ready
                    .insert(agent_id_proto.clone(), waiting_on);
            }
            if let Some(role) = role.as_deref() {
                let started = Event::AgentStarted(tau_proto::AgentStarted {
                    agent_id: agent_id_proto.clone(),
                    parent_agent: self.parent_agent_id_for_cid(cid),
                    role: role.to_owned(),
                    display_name: self
                        .agents
                        .get(cid)
                        .and_then(|conv| normalize_display_name(conv.display_name.as_deref())),
                    metadata: initial_metadata,
                });
                self.enqueue_publish(
                    None,
                    started,
                    false,
                    false,
                    Some(ConversationHeadSync {
                        cid: cid.clone(),
                        agent_id: Some(agent_id_proto.clone()),
                    }),
                );
            }
            for (key, entry) in self.inherited_metadata_for_cid(cid) {
                self.enqueue_publish(
                    None,
                    Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
                        agent_id: agent_id_proto.clone(),
                        key,
                        value: entry.value,
                        inheritable: entry.inheritable,
                    }),
                    false,
                    false,
                    Some(ConversationHeadSync {
                        cid: cid.clone(),
                        agent_id: Some(agent_id_proto.clone()),
                    }),
                );
            }
            self.publish_event(
                None,
                Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                    session_id: self.current_session_id.clone(),
                    agent_id: agent_id_proto,
                }),
            );
        }
    }

    /// Mints a new `AgentPromptId`, registers it with `cid`'s
    /// conversation, and dispatches `AgentPromptCreated` to the agent. Reads
    /// `system_prompt` / `messages` / `tools` from the
    /// agent's agent tree.
    ///
    /// Linear-prefix invariant: each subsequent prompt for the same
    /// agent branch must be a strict byte-prefix extension of the prior
    /// one. Provider prompt caches (OpenAI, Anthropic, etc.) key
    /// entirely off the prefix bytes, so any per-turn churn in
    /// `system_prompt`, `tools`, or earlier messages busts the cache.
    /// See `linear_agent_prompts_strictly_extend_previous_messages`.
    pub(crate) fn send_prompt_to_agent_for(&mut self, cid: &AgentId) -> Option<AgentPromptId> {
        let prompt = self.prepare_agent_prompt_for_dispatch(cid)?;
        let agent_prompt_id = prompt.agent_prompt_id.clone();
        self.publish_event(None, Event::AgentPromptCreated(prompt));
        Some(agent_prompt_id)
    }

    /// Builds one prompt request and records the live in-flight bookkeeping
    /// needed to route the corresponding provider response. The prompt payload
    /// is returned to the caller instead of cached; it is a transient delivery
    /// object, not durable harness state.
    fn prepare_agent_prompt_for_dispatch(&mut self, cid: &AgentId) -> Option<AgentPromptCreated> {
        let _ = self.ensure_agent_id_for_agent(cid);
        let conv = self
            .agents
            .get(cid)
            .expect("prepare_agent_prompt_for_dispatch: unknown agent id");
        let originator = conv.originator.clone();
        let role_name = self.role_name_for_agent(conv);
        let prompt_model = self.model_for_agent_role(conv);
        let prompt_params = prompt_model
            .as_ref()
            .map(|model| self.params_for_role_model(&role_name, model))
            .unwrap_or_default();
        let Some(model) = prompt_model else {
            self.emit_info(&format!(
                "role `{role_name}` has no available model — use /role to pick a role, /model <provider>/<model> to pick an agent model, or enable a provider"
            ));
            return None;
        };
        // Non-tool extension side agents (`std-notifications`'
        // idle summary, etc.) must not execute tools — their whole
        // job is to produce a one-line summary, and unfettered tool
        // access has historically caused destructive `edit` calls. Do NOT
        // enforce that by flipping the provider `tool_choice` to `none`:
        // `tool_choice` is serialized on the
        // wire and changing it breaks the request-body equivalence the
        // `previous_response_id` cache relies on. Keep the wire
        // request identical to the parent (`Auto`) and enforce the
        // no-tools rule locally before dispatching any returned tool
        // calls.
        let is_non_tool_ext_query = matches!(
            conv.originator,
            tau_proto::PromptOriginator::Extension { .. }
        ) && conv.parent_tool_call_id.is_none();
        let tool_choice = tau_proto::ToolChoice::Auto;
        // Legacy cache-sharing hint for older provider implementations. The
        // first-party ChatGPT/Codex provider now derives cache keys only from
        // base URL and target agent id, so prompt originator and this flag do
        // not split cache buckets.
        let share_user_cache_key = is_non_tool_ext_query;
        // Walk the agent's *own* branch, not whatever tree.head
        // currently points at. With multiple side agents
        // running concurrently their tree mutations interleave, so
        // tree.head is an unreliable signal for "where this
        // conversation lives". Reading from `conv.head` keeps the
        // assembled prompt scoped to this agent's history and
        // prevents orphan ToolUse blocks from cross-branch state.
        let head = conv.head;

        let agent_id_for_tree = conv.agent_id.clone();
        let tree = agent_id_for_tree
            .as_deref()
            .and_then(|agent_id| self.agent_store.agent(agent_id));
        let prompt_context = tree
            .map(|t| assemble_prompt_context_from(t, head))
            .unwrap_or_else(|| crate::prompt::AssembledPromptContext {
                context: tau_proto::PromptContext::default(),
            });
        let context = prompt_context.context;
        let tool_specs = self.gather_effective_tool_specs_for_role_model(&role_name, Some(&model));
        let tools = self.tool_definitions_from_specs(&tool_specs);
        let durable_agent_id = agent_id_for_tree.as_deref().map(crate::parse_agent_id);
        let system_prompt =
            self.build_system_prompt_for_role_and_agent(&role_name, durable_agent_id.as_ref());
        let durable_agent_id = agent_id_for_tree.as_deref().unwrap_or(cid.as_ref());
        let prompt_index = self
            .agents
            .get_mut(cid)
            .expect("prepare_agent_prompt_for_dispatch: unknown agent id")
            .next_prompt_index;
        let agent_prompt_id: AgentPromptId = format!("ap-{durable_agent_id}-{prompt_index}").into();
        if let Some(agent) = self.agents.get_mut(cid) {
            agent.next_prompt_index += 1;
        }
        self.prompt_agents
            .insert(agent_prompt_id.clone(), cid.clone());
        let ctx_id = self.agents.get_mut(cid).and_then(|c| c.next_ctx_id.take());
        if let Some(c) = self.agents.get_mut(cid) {
            c.in_flight_prompt = Some(agent_prompt_id.clone());
        }
        self.set_agent_turn_state(
            cid,
            AgentTurnState::AgentThinking {
                agent_prompt_id: agent_prompt_id.clone(),
            },
        );

        self.current_session_state.token_usage.start_request(&model);
        self.prompt_models
            .insert(agent_prompt_id.clone(), model.clone());
        self.prompt_tool_specs
            .insert(agent_prompt_id.clone(), tool_specs);
        let session_id = self
            .agents
            .get(cid)
            .expect("agent still exists")
            .session_id
            .clone();
        let agent_id: tau_proto::AgentId = crate::parse_agent_id(
            self.ensure_agent_id_for_agent(cid)
                .expect("agent has durable id"),
        );
        let compaction = self.compaction_context_for_agent(cid, &model);
        Some(AgentPromptCreated {
            agent_prompt_id,
            agent_id,
            session_id,
            system_prompt,
            context,
            tools,
            tools_ref: None,
            model,
            model_params: prompt_params,
            tool_choice,
            originator,
            share_user_cache_key,
            ctx_id,
            compaction,
        })
    }

    fn role_name_for_agent(&self, conv: &Agent) -> String {
        conv.role
            .clone()
            .unwrap_or_else(|| self.selected_role.clone())
    }

    fn role_name_for_agent_id(&self, cid: &AgentId) -> String {
        self.agents
            .get(cid)
            .and_then(|conv| conv.role.clone())
            .unwrap_or_else(|| self.selected_role.clone())
    }

    fn model_for_agent_role(&self, conv: &Agent) -> Option<ModelId> {
        if let Some(model) = conv.model_override.clone()
            && self.provider_model_routes.contains_key(&model)
        {
            return Some(model);
        }
        let role_name = self.role_name_for_agent(conv);
        model_for_role(&self.provider_model_info, &self.available_roles, &role_name)
    }

    pub(crate) fn selected_model_params(&self) -> tau_proto::ModelParams {
        self.selected_model
            .as_ref()
            .map(|model| self.params_for_role_model(&self.selected_role, model))
            .unwrap_or_default()
    }

    fn params_for_role_model(&self, role_name: &str, model: &ModelId) -> tau_proto::ModelParams {
        selected_params_for_role(
            &self.provider_model_info,
            &self.available_roles,
            role_name,
            model,
        )
    }

    #[cfg(test)]
    fn build_system_prompt_for_role(&self, role_name: &str) -> String {
        self.build_system_prompt_for_role_and_agent(role_name, None)
    }

    fn build_system_prompt_for_role_preview(&self, role_name: &str) -> String {
        let preview_agent_id = crate::parse_agent_id(RENDERED_PROMPT_PREVIEW_AGENT_ID);
        self.build_system_prompt_for_role_and_agent(role_name, Some(&preview_agent_id))
    }

    fn build_system_prompt_for_role_and_agent(
        &self,
        role_name: &str,
        agent_id: Option<&tau_proto::AgentId>,
    ) -> String {
        let (prompt_fragments, tool_prompt_fragments) =
            self.gather_prompt_fragment_groups_for_role(role_name);
        let system_template = self.system_template_for_role(role_name);
        let template_context = match agent_id {
            Some(agent_id) => RolePromptTemplateContext::for_agent(role_name, agent_id),
            None => RolePromptTemplateContext::for_role(role_name),
        };
        build_system_prompt_with_tool_template_context(
            system_template,
            &self.discovered_skills,
            &prompt_fragments,
            &tool_prompt_fragments,
            self.agent_context.template_value(agent_id),
            template_context,
        )
    }

    fn system_template_for_role(&self, role_name: &str) -> &str {
        let template_name = self
            .available_roles
            .get(role_name)
            .and_then(|role| role.prompt_override.as_deref())
            .unwrap_or(BUILT_IN_SYSTEM_TEMPLATE_NAME);
        self.system_prompt_templates
            .get(template_name)
            .or_else(|| {
                self.system_prompt_templates
                    .get(BUILT_IN_SYSTEM_TEMPLATE_NAME)
            })
            .map(String::as_str)
            .unwrap_or("")
    }

    #[cfg(test)]
    fn gather_prompt_fragments(&self) -> Vec<PromptFragment> {
        self.gather_prompt_fragments_for_role(&self.selected_role)
    }

    #[cfg(test)]
    fn gather_prompt_fragments_for_role(&self, role_name: &str) -> Vec<PromptFragment> {
        let (fragments, tool_fragments) = self.gather_sourced_prompt_fragment_groups(role_name);
        sorted_prompt_fragments(fragments.into_iter().chain(tool_fragments.into_iter().map(
            |sourced| SourcedPromptFragment {
                source: sourced.source,
                fragment: sourced.fragment,
            },
        )))
    }

    fn gather_prompt_fragment_groups_for_role(
        &self,
        role_name: &str,
    ) -> (Vec<PromptFragment>, Vec<ToolPromptFragment>) {
        let (fragments, tool_fragments) = self.gather_sourced_prompt_fragment_groups(role_name);
        (
            sorted_prompt_fragments(fragments),
            sorted_tool_prompt_fragments(tool_fragments),
        )
    }

    fn gather_sourced_prompt_fragment_groups(
        &self,
        role_name: &str,
    ) -> (Vec<SourcedPromptFragment>, Vec<SourcedToolPromptFragment>) {
        let mut fragments: Vec<_> = self
            .extension_prompt_fragments
            .iter()
            .flat_map(|(connection_id, fragments)| {
                fragments
                    .values()
                    .map(move |fragment| SourcedPromptFragment {
                        source: PromptFragmentSource::Extension {
                            connection_id: connection_id.clone(),
                        },
                        fragment: fragment.clone(),
                    })
            })
            .collect();
        if let Some(role) = self.available_roles.get(role_name) {
            fragments.extend(
                role.prompt_fragments
                    .iter()
                    .map(|fragment| SourcedPromptFragment {
                        source: PromptFragmentSource::RoleConfig {
                            role_name: role_name.to_owned(),
                        },
                        fragment: PromptFragment::new(
                            fragment.name.clone(),
                            fragment.priority,
                            fragment.text.clone(),
                        ),
                    }),
            );
        }
        let providers = self.registry.all_tool_providers();
        let enabled_group_keys = providers
            .iter()
            .filter(|provider| self.is_tool_provider_enabled_for_role(provider, role_name))
            .filter_map(|provider| {
                provider
                    .tool_group
                    .as_ref()
                    .map(|group| (provider.connection_id.clone(), group.name.clone()))
            })
            .collect::<HashSet<_>>();
        let mut seen_group_fragments = HashSet::new();
        let mut tool_fragments = Vec::new();
        for provider in providers {
            let tool_prompt_repeated_by_group = provider
                .tool_group
                .as_ref()
                .and_then(|group| group.prompt_fragment.as_ref())
                .is_some_and(|group_fragment| {
                    provider
                        .prompt_fragment
                        .as_ref()
                        .is_some_and(|tool_fragment| tool_fragment.name == group_fragment.name)
                });
            if !tool_prompt_repeated_by_group
                && self.is_tool_provider_enabled_for_role(provider, role_name)
                && let Some(fragment) = &provider.prompt_fragment
            {
                let visible_name = self.tool_model_visible_name(&provider.tool);
                tool_fragments.push(SourcedToolPromptFragment {
                    source: PromptFragmentSource::Tool {
                        connection_id: provider.connection_id.clone(),
                    },
                    tool_name: visible_name.clone(),
                    fragment: fragment.clone(),
                });
            }
            if let Some(group) = &provider.tool_group
                && let Some(fragment) = &group.prompt_fragment
                && enabled_group_keys
                    .contains(&(provider.connection_id.clone(), group.name.clone()))
                && seen_group_fragments.insert((
                    provider.connection_id.clone(),
                    group.name.clone(),
                    fragment.name.clone(),
                ))
            {
                tool_fragments.push(SourcedToolPromptFragment {
                    source: PromptFragmentSource::Tool {
                        connection_id: provider.connection_id.clone(),
                    },
                    tool_name: ToolName::new(group.name.as_str()),
                    fragment: fragment.clone(),
                });
            }
        }
        (fragments, tool_fragments)
    }

    fn gather_tool_definitions_for_role(&self, role_name: &str) -> Vec<ToolDefinition> {
        let model = model_for_role(&self.provider_model_info, &self.available_roles, role_name);
        let specs = self.gather_effective_tool_specs_for_role_model(role_name, model.as_ref());
        self.tool_definitions_from_specs(&specs)
    }

    fn tool_definitions_from_specs(&self, specs: &[tau_proto::ToolSpec]) -> Vec<ToolDefinition> {
        specs
            .iter()
            .map(|spec| ToolDefinition {
                name: spec.name.clone(),
                model_visible_name: spec.model_visible_name.clone(),
                description: spec.description.clone(),
                tool_type: spec.tool_type,
                parameters: spec.parameters.clone(),
                format: spec.format.clone(),
            })
            .collect()
    }

    fn gather_effective_tool_specs_for_role_model(
        &self,
        role_name: &str,
        model: Option<&ModelId>,
    ) -> Vec<tau_proto::ToolSpec> {
        self.registry
            .all_tool_providers()
            .into_iter()
            .filter(|provider| {
                self.is_tool_enabled_for_role_model(
                    &provider.tool,
                    provider.tool_group.as_ref(),
                    role_name,
                    model,
                )
            })
            .map(|provider| provider.tool.clone())
            .collect()
    }

    fn tool_model_visible_name<'a>(&self, spec: &'a tau_proto::ToolSpec) -> &'a ToolName {
        spec.model_visible_name.as_ref().unwrap_or(&spec.name)
    }

    fn has_registered_tool_name(&self, requested_name: &ToolName) -> bool {
        for spec in self.registry.all_tools() {
            if spec.name == *requested_name || self.tool_model_visible_name(spec) == requested_name
            {
                return true;
            }
        }
        false
    }

    fn tool_call_waits_for_staged_registration(
        &self,
        cid: &AgentId,
        requested_name: &ToolName,
        agent_prompt_id: Option<&AgentPromptId>,
    ) -> bool {
        let Some((internal_name, visible_name)) =
            self.staged_wait_tool_names(cid, requested_name, agent_prompt_id)
        else {
            return false;
        };
        self.extensions.activation_staging.values().any(|stage| {
            stage.tool_registrations.iter().any(|registration| {
                registration.tool.name == internal_name
                    || self.tool_model_visible_name(&registration.tool) == &visible_name
            })
        })
    }

    fn staged_wait_tool_names(
        &self,
        cid: &AgentId,
        requested_name: &ToolName,
        agent_prompt_id: Option<&AgentPromptId>,
    ) -> Option<(ToolName, ToolName)> {
        if let Some(agent_prompt_id) = agent_prompt_id {
            let spec =
                self.resolve_enabled_tool_spec_for_prompt(requested_name, agent_prompt_id)?;
            if self.registry.resolve_provider(&spec.name).is_some() {
                return None;
            }
            return Some((
                spec.name.clone(),
                self.tool_model_visible_name(spec).clone(),
            ));
        }

        let role_name = self.role_name_for_agent_id(cid);
        if self
            .resolve_enabled_tool_name_for_role(requested_name, &role_name)
            .is_some()
        {
            return None;
        }
        self.extensions
            .activation_staging
            .values()
            .flat_map(|stage| stage.tool_registrations.iter())
            .find(|registration| {
                self.is_registered_tool_enabled_for_role(registration, &role_name)
                    && (registration.tool.name == *requested_name
                        || self.tool_model_visible_name(&registration.tool) == requested_name)
            })
            .map(|registration| {
                (
                    registration.tool.name.clone(),
                    self.tool_model_visible_name(&registration.tool).clone(),
                )
            })
    }

    fn is_tool_enabled_for_role_model(
        &self,
        spec: &tau_proto::ToolSpec,
        group: Option<&tau_proto::ToolGroup>,
        role_name: &str,
        model: Option<&ModelId>,
    ) -> bool {
        let mut enabled = spec.enabled_by_default;
        let model_tags = model
            .and_then(|model| self.provider_model_info.get(model))
            .map(|info| info.tags.as_slice())
            .unwrap_or(&[]);
        let mut rules: Vec<_> = self.tool_policy.rules.iter().collect();
        rules.sort_by(|(left_name, left), (right_name, right)| {
            left.priority
                .cmp(&right.priority)
                .then_with(|| left_name.cmp(right_name))
        });
        for (_, rule) in rules {
            if !rule.enable
                || !rule.when.model_tags.iter().all(|pattern| {
                    model_tags
                        .iter()
                        .any(|model_tag| pattern.matches(model_tag))
                })
            {
                continue;
            }
            if tags_match_any(&spec.tags, &rule.disable_tool_tags) {
                enabled = false;
            }
            if tags_match_any(&spec.tags, &rule.enable_tool_tags) {
                enabled = true;
            }
        }

        let Some(role) = self.available_roles.get(role_name) else {
            return enabled;
        };
        if let Some(tools) = &role.tools {
            enabled = tools.iter().any(|name| name == &spec.name);
        }
        if tags_match_any(&spec.tags, &role.disable_tool_tags) {
            enabled = false;
        }
        if tags_match_any(&spec.tags, &role.enable_tool_tags) {
            enabled = true;
        }
        if let Some(group) = group {
            if role
                .disable_tool_groups
                .iter()
                .any(|name| name == &group.name)
            {
                enabled = false;
            }
            if role
                .enable_tool_groups
                .iter()
                .any(|name| name == &group.name)
            {
                enabled = true;
            }
        }
        if role.disable_tools.iter().any(|name| name == &spec.name) {
            enabled = false;
        }
        if role.enable_tools.iter().any(|name| name == &spec.name) {
            enabled = true;
        }
        enabled
    }

    fn resolve_enabled_tool_spec_for_role(
        &self,
        requested_name: &ToolName,
        role_name: &str,
    ) -> Option<&tau_proto::ToolSpec> {
        let mut visible_match: Option<&tau_proto::ToolSpec> = None;
        for provider in self.registry.all_tool_providers() {
            let spec = &provider.tool;
            if !self.is_tool_provider_enabled_for_role(provider, role_name) {
                continue;
            }
            if spec.name == *requested_name {
                return Some(spec);
            }
            if self.tool_model_visible_name(spec) == requested_name && visible_match.is_none() {
                visible_match = Some(spec);
            }
        }
        visible_match
    }

    fn resolve_enabled_tool_spec_for_prompt(
        &self,
        requested_name: &ToolName,
        agent_prompt_id: &AgentPromptId,
    ) -> Option<&tau_proto::ToolSpec> {
        let mut visible_match: Option<&tau_proto::ToolSpec> = None;
        let specs = self.prompt_tool_specs.get(agent_prompt_id)?;
        for spec in specs {
            if spec.name == *requested_name {
                return Some(spec);
            }
            if self.tool_model_visible_name(spec) == requested_name && visible_match.is_none() {
                visible_match = Some(spec);
            }
        }
        visible_match
    }

    fn resolve_enabled_tool_name_for_role(
        &self,
        requested_name: &ToolName,
        role_name: &str,
    ) -> Option<(ToolName, ToolName)> {
        self.resolve_enabled_tool_spec_for_role(requested_name, role_name)
            .map(|spec| {
                (
                    spec.name.clone(),
                    self.tool_model_visible_name(spec).clone(),
                )
            })
    }

    fn is_registered_tool_enabled_for_role(
        &self,
        registration: &ToolRegister,
        role_name: &str,
    ) -> bool {
        self.is_tool_enabled_for_role(
            &registration.tool,
            registration.tool_group.as_ref(),
            role_name,
        )
    }

    fn is_tool_provider_enabled_for_role(
        &self,
        provider: &tau_core::ToolProvider,
        role_name: &str,
    ) -> bool {
        self.is_tool_enabled_for_role(&provider.tool, provider.tool_group.as_ref(), role_name)
    }

    fn is_tool_enabled_for_role(
        &self,
        spec: &tau_proto::ToolSpec,
        group: Option<&tau_proto::ToolGroup>,
        role_name: &str,
    ) -> bool {
        self.is_tool_enabled_for_role_model(spec, group, role_name, self.selected_model.as_ref())
    }

    fn compaction_original_input_tokens_for_prompt(
        &self,
        agent_prompt_id: &AgentPromptId,
    ) -> Option<u64> {
        let cid = self.agent_id_for_prompt(agent_prompt_id)?;
        self.agents
            .get(&cid)
            .and_then(|conv| conv.context_input_tokens)
    }

    fn enrich_provider_response_updated_compaction(
        &self,
        updated: &mut tau_proto::ProviderResponseUpdated,
    ) {
        let has_compaction = updated.items.iter().any(|item| {
            matches!(
                item,
                tau_proto::ProviderResponseItem::InProgress(
                    tau_proto::InProgressOutputItem::Compaction { .. }
                ) | tau_proto::ProviderResponseItem::Completed(ContextItem::Compaction(_))
            )
        });
        if !has_compaction {
            return;
        }
        updated.compaction_original_input_tokens = self
            .compaction_original_input_tokens_for_prompt(&updated.agent_prompt_id)
            .or(updated.compaction_original_input_tokens);
        let completed_items: Vec<_> = updated
            .items
            .iter()
            .filter_map(|item| match item {
                tau_proto::ProviderResponseItem::Completed(item) => Some(item.clone()),
                _ => None,
            })
            .collect();
        if let Some(replay_window) = latest_compaction_replay_window(&completed_items) {
            updated.compaction_compacted_input_tokens =
                estimate_compacted_input_tokens(replay_window)
                    .or(updated.compaction_compacted_input_tokens);
        }
    }

    #[cfg(test)]
    fn handle_provider_response_finished(
        &mut self,
        response: ProviderResponseFinished,
    ) -> Result<(), HarnessError> {
        self.handle_provider_response_finished_from(None, response)
    }

    fn handle_provider_response_finished_from(
        &mut self,
        source: Option<&str>,
        mut response: ProviderResponseFinished,
    ) -> Result<(), HarnessError> {
        let mut tool_calls = tool_calls_from_output_items(&response.output_items);
        let mut requested_tool_calls = response_requests_tool_calls(&response);
        let assistant_text = assistant_text_from_output_items(&response.output_items);
        let input_tokens = response
            .usage
            .as_ref()
            .map(|usage| usage.prompt_sent_tokens);
        let cached_tokens = response
            .usage
            .as_ref()
            .map(|usage| usage.prompt_cached_tokens);
        let output_tokens = response
            .usage
            .as_ref()
            .map(|usage| usage.response_received_tokens);
        if self.canceled_prompts.remove(&response.agent_prompt_id) {
            self.prompt_agents.remove(response.agent_prompt_id.as_str());
            self.pending_provider_prompts
                .remove(&response.agent_prompt_id);
            self.prompt_models.remove(&response.agent_prompt_id);
            self.clear_prompt_tool_snapshot(&response.agent_prompt_id);
            return Ok(());
        }
        let response_cid = self.agent_id_for_prompt(&response.agent_prompt_id);
        let response_contains_compaction = response
            .output_items
            .iter()
            .any(|item| matches!(item, ContextItem::Compaction(_)));
        let compaction_original_input_tokens = response_contains_compaction
            .then(|| self.compaction_original_input_tokens_for_prompt(&response.agent_prompt_id))
            .flatten();
        // Per-conversation usage: separate from the global tracker
        // because side agents shouldn't clobber the user's
        // status bar, but the harness still needs their context %
        // to surface via `DelegateProgress`.
        if let Some(cid) = response_cid.as_ref() {
            let usage_model = self.prompt_models.get(&response.agent_prompt_id).cloned();
            self.update_agent_context_usage(cid, usage_model.as_ref(), input_tokens, cached_tokens);
            self.emit_delegate_progress(cid);
        }
        // Dedupe: under at-least-once delivery the agent may resend a
        // finished-response after a reconnect. The first delivery
        // removed the entry from `prompt_agents`; later ones
        // must be ignored rather than falling back to another
        // session route, which would silently misroute the duplicate.
        let Some(cid) = response_cid else {
            self.emit_info(&format!(
                "discarding duplicate agent response for agent_prompt_id={}",
                response.agent_prompt_id
            ));
            return Ok(());
        };
        response.agent_id = crate::parse_agent_id(
            self.target_agent_id_for_agent(&cid)
                .expect("agent has durable id"),
        );

        let stale_behind_newer_prompt = self.agents.get(&cid).is_some_and(|conv| {
            conv.last_prompt_id
                .as_ref()
                .is_some_and(|last| last != &response.agent_prompt_id)
                || conv
                    .in_flight_prompt
                    .as_ref()
                    .is_some_and(|in_flight| in_flight != &response.agent_prompt_id)
        });
        if stale_behind_newer_prompt {
            if let Some((session_id, originator)) = self
                .agents
                .get(&cid)
                .map(|conv| (conv.session_id.clone(), conv.originator.clone()))
            {
                self.publish_prompt_terminated(
                    session_id,
                    response.agent_prompt_id.clone(),
                    AgentPromptTerminationReason::Stale,
                    originator,
                );
            }
            self.emit_info(&format!(
                "discarding stale agent response for agent_prompt_id={}",
                response.agent_prompt_id
            ));
            self.prompt_agents.remove(response.agent_prompt_id.as_str());
            self.pending_provider_prompts
                .remove(&response.agent_prompt_id);
            self.prompt_models.remove(&response.agent_prompt_id);
            self.clear_prompt_tool_snapshot(&response.agent_prompt_id);
            return Ok(());
        }
        // Save the model that ran this turn before the
        // `prompt_models` entry is consumed below — we'll need it
        // again to anchor the stateful-chain state, and re-reading
        // `selected_model` later would lie if the user switched
        // models mid-turn.
        let turn_model = self.prompt_models.remove(&response.agent_prompt_id);
        if let Some(ref model) = turn_model {
            let sent_tokens = input_tokens.unwrap_or(0);
            let cached_tokens = cached_tokens.unwrap_or(0);
            let received_tokens = output_tokens.unwrap_or(0);
            self.current_session_state
                .token_usage
                .add_sent(model, sent_tokens, cached_tokens);
            self.current_session_state
                .token_usage
                .add_received(model, received_tokens);
            response.usage = Some(ProviderTokenUsage {
                model: Some(model.clone()),
                prompt_sent_tokens: sent_tokens,
                prompt_cached_tokens: cached_tokens,
                response_received_tokens: received_tokens,
                stats: self.current_session_state.token_usage.clone(),
            });
        }
        if response_contains_compaction {
            response.compaction_original_input_tokens = input_tokens
                .or(response.compaction_original_input_tokens)
                .or(compaction_original_input_tokens);
            response.compaction_compacted_input_tokens = response
                .usage
                .as_ref()
                .and_then(|usage| {
                    (0 < usage.response_received_tokens).then_some(usage.response_received_tokens)
                })
                .or_else(|| {
                    latest_compaction_replay_window(&response.output_items)
                        .and_then(estimate_compacted_input_tokens)
                })
                .or(response.compaction_compacted_input_tokens);
        }
        if requested_tool_calls && tool_calls.is_empty() {
            self.emit_info(&format!(
                "agent response {} reported tool calls but contained none; treating it as end_turn",
                response.agent_prompt_id
            ));
            requested_tool_calls = false;
        }
        let tool_calls_with_non_tool_stop = !requested_tool_calls && !tool_calls.is_empty();
        if tool_calls_with_non_tool_stop {
            requested_tool_calls = true;
        }
        let is_non_tool_ext_query = self.agents.get(&cid).is_some_and(|conv| {
            matches!(
                conv.originator,
                tau_proto::PromptOriginator::Extension { .. }
            ) && conv.parent_tool_call_id.is_none()
        });

        let mut invalid_tool_call_errors: HashMap<ToolCallId, String> = HashMap::new();
        let mut normalized_calls: Vec<(AgentToolCall, BackgroundSupport)> = Vec::new();
        if requested_tool_calls {
            let mut seen_tool_call_ids = HashSet::new();
            let mut reserved_tool_call_ids = self.known_tool_call_ids();
            normalized_calls = tool_calls
                .iter()
                .enumerate()
                .map(|(index, call)| {
                    let mut call = call.clone();
                    let invalid_message = if call.id.as_str().is_empty() {
                        Some(format!(
                            "provider emitted tool call `{}` with an empty call_id; refusing to execute it",
                            call.name
                        ))
                    } else if !seen_tool_call_ids.insert(call.id.clone()) {
                        Some(format!(
                            "provider emitted duplicate tool call_id `{}` for tool `{}`; refusing to execute the duplicate",
                            call.id, call.name
                        ))
                    } else if reserved_tool_call_ids.contains(&call.id) {
                        Some(format!(
                            "provider reused prior tool call_id `{}` for tool `{}`; refusing to execute it",
                            call.id, call.name
                        ))
                    } else if is_non_tool_ext_query {
                        Some(format!(
                            "non-tool extension query attempted to call tool `{}`; refusing to execute it",
                            call.name
                        ))
                    } else if tool_calls_with_non_tool_stop {
                        Some(format!(
                            "provider emitted tool call `{}` with stop_reason {:?}; refusing to execute it",
                            call.name, response.stop_reason
                        ))
                    } else {
                        reserved_tool_call_ids.insert(call.id.clone());
                        None
                    };
                    if let Some(message) = invalid_message {
                        call.id = unique_synthetic_tool_call_id(
                            &mut reserved_tool_call_ids,
                            &response.agent_prompt_id,
                            index,
                        );
                        seen_tool_call_ids.insert(call.id.clone());
                        invalid_tool_call_errors.insert(call.id.clone(), message);
                    }
                    self.prompt_tool_call_prompts
                        .insert(call.id.clone(), response.agent_prompt_id.clone());
                    let background_support = self.resolve_tool_background_support(call.name.as_str());
                    (call, background_support)
                })
                .collect();
            let mut normalized_calls_iter = normalized_calls.iter();
            response.output_items = response
                .output_items
                .into_iter()
                .map(|item| match item {
                    ContextItem::ToolCall(_) => {
                        let (call, _) = normalized_calls_iter
                            .next()
                            .expect("tool-call normalization count should match output items");
                        ContextItem::ToolCall(ToolCallItem {
                            call_id: call.id.clone(),
                            name: call.name.clone(),
                            tool_type: call.tool_type,
                            arguments: call.arguments.clone(),
                        })
                    }
                    item => item,
                })
                .collect();
            tool_calls = normalized_calls
                .iter()
                .map(|(call, _)| call.clone())
                .collect();
        }

        // Publish via the owning agent's branch — when text is
        // present the AgentTree fold appends an assistant response as a
        // child of `tree.head`, so an unsnapped publish would land on
        // whichever branch happened to be at `tree.head` (e.g. after
        // a sibling side conv's teardown touched another branch).
        // `publish_for_agent` snaps and updates `c.head`.
        self.publish_for_agent_from(
            &cid,
            source,
            Event::ProviderResponseFinished(response.clone()),
        );
        self.prompt_agents.remove(response.agent_prompt_id.as_str());
        self.pending_provider_prompts
            .remove(&response.agent_prompt_id);
        if let Some(conv) = self.agents.get_mut(&cid) {
            conv.in_flight_prompt = None;
        }

        // Side-conversation handling: if this prompt originated from
        // an extension via StartAgentRequest, route the final text back
        // to the requesting extension as StartAgentResult and
        // tear down the side agent. The harness routes tool
        // calls per-agent, so scheduler-selected calls for this side
        // agent have already been emitted into the bus and will
        // complete normally even after teardown.
        if let tau_proto::PromptOriginator::Extension {
            ref name,
            ref query_id,
        } = response.originator
            && (!requested_tool_calls || is_non_tool_ext_query)
        {
            if !requested_tool_calls {
                self.clear_prompt_tool_snapshot(&response.agent_prompt_id);
            }
            if requested_tool_calls {
                let remaining_calls: Vec<ToolCallId> = normalized_calls
                    .iter()
                    .map(|(call, _)| call.id.clone())
                    .collect();
                for (call, _) in &normalized_calls {
                    self.pending_tools.insert(
                        call.id.clone(),
                        PendingTool {
                            name: call.name.clone(),
                            internal_name: call.name.clone(),
                            tool_type: call.tool_type,
                        },
                    );
                }
                self.set_agent_turn_state(&cid, AgentTurnState::ToolsRunning { remaining_calls });
                for (call, _) in &normalized_calls {
                    let message = invalid_tool_call_errors
                        .remove(&call.id)
                        .unwrap_or_else(|| {
                            format!("refusing to execute tool call `{}`", call.name)
                        });
                    self.reject_agent_tool_call_before_dispatch_without_followup(
                        &cid,
                        call,
                        call.name.clone(),
                        message,
                    );
                }
                self.set_agent_turn_state(&cid, AgentTurnState::Idle);
            }
            if self.has_pending_message_received_prompt(&cid) {
                self.fold_pending_prompts_as_steered(&cid);
                self.dispatch_prompt_after_publish_idle(&cid);
                return Ok(());
            }

            let source = self
                .agents
                .get(&cid)
                .and_then(|c| c.source_connection.clone());
            let error = if is_non_tool_ext_query && requested_tool_calls {
                Some(format!(
                    "non-tool extension query attempted to call {} tool(s); refusing to execute",
                    tool_calls.len()
                ))
            } else {
                None
            };
            let result = tau_proto::StartAgentResult {
                query_id: query_id.clone(),
                text: assistant_text.clone().unwrap_or_default(),
                error,
            };
            if let Some(source) = source {
                if source.as_str() == HARNESS_CONNECTION_ID {
                    self.publish_event(
                        Some(HARNESS_CONNECTION_ID),
                        Event::StartAgentResult(result),
                    );
                } else {
                    let _ = self.bus.send_to(
                        source.as_str(),
                        None,
                        HarnessOutputMessage::deliver(Event::StartAgentResult(result)),
                    );
                }
            } else {
                // Should never happen — `source_connection` is set in
                // `handle_start_agent_request` when the conversation is
                // spawned. Surface it via `harness.notice` rather than
                // silently dropping so a future regression is visible.
                self.emit_harness_failure(&format!(
                    "start-agent-request result for `{}` (extension `{}`) had no source connection — \
                         dropping",
                    query_id, name
                ));
            }
            let completed_agent_id = self.agents.get(&cid).and_then(|conv| conv.agent_id.clone());
            let keep_tool_backed_conversation = self
                .agents
                .get(&cid)
                .is_some_and(|conv| conv.parent_tool_call_id.is_some());
            let should_auto_suspend_delegate = keep_tool_backed_conversation
                && completed_agent_id.as_deref().is_some_and(|agent_id| {
                    self.agent_states.get(agent_id).copied() == Some(AgentState::ActiveDelegated)
                });
            // Release before removing or detaching the side agent so
            // queued descendants can still resolve their parent agent
            // while starting. Active descendants keep their own copied state.
            self.set_agent_turn_state(&cid, AgentTurnState::Idle);
            self.release_start_agent_request(&cid);
            if keep_tool_backed_conversation {
                if should_auto_suspend_delegate
                    && let Some(agent_id) = completed_agent_id.as_deref()
                {
                    self.set_agent_state(agent_id, AgentState::Suspended);
                }
                self.detach_completed_tool_backed_start_agent(&cid);
            } else {
                self.transfer_background_completion_target_before_teardown(&cid);
                self.remove_agent(&cid);
            }
            self.try_advance_queue();
            return Ok(());
        }

        if requested_tool_calls {
            // Tool calls to execute — agent stays busy. After all
            // tools complete, maybe_complete_agent_turn drains any
            // prompts queued via `pending_prompts` (publishing one
            // `AgentPromptSteered` each, which folds them as
            // `UserMessage` entries onto this agent's branch)
            // and sends a new prompt with the results plus those
            // steering messages.
            // Malformed provider call ids were normalized before the assistant
            // response was published. Keep them in the turn as synthetic
            // rejected calls so the next model prompt sees a matched
            // tool-call/tool-error pair instead of the harness returning an
            // event-loop error or overwriting duplicate map entries.
            let remaining_calls: Vec<ToolCallId> = normalized_calls
                .iter()
                .map(|(call, _)| call.id.clone())
                .collect();
            for (call, _) in &normalized_calls {
                self.pending_tools.insert(
                    call.id.clone(),
                    PendingTool {
                        name: call.name.clone(),
                        internal_name: call.name.clone(),
                        tool_type: call.tool_type,
                    },
                );
            }
            self.set_agent_turn_state(&cid, AgentTurnState::ToolsRunning { remaining_calls });
            if self
                .agents
                .get(&cid)
                .is_some_and(|conv| conv.pending_cancel.is_some())
            {
                self.apply_pending_cancel_for_agent(&cid);
                return Ok(());
            }
            // Queue well-formed tool calls and turn malformed calls into
            // model-visible errors. The turn machine preserves provider order
            // for calls that are safe to dispatch.
            for (call, background_support) in normalized_calls {
                if let Some(message) = invalid_tool_call_errors.remove(&call.id) {
                    self.reject_agent_tool_call_before_dispatch(
                        &cid,
                        &call,
                        call.name.clone(),
                        message,
                    );
                } else {
                    self.tool_turn.push(cid.clone(), call, background_support);
                }
            }
            self.drain_pending_tool_invocations()?;
        } else {
            self.clear_prompt_tool_snapshot(&response.agent_prompt_id);
            self.set_agent_turn_state(&cid, AgentTurnState::Idle);
            // No tool calls — this agent's turn is done. Drain
            // any queued prompts (on this or other agents) that
            // are now eligible to dispatch.
            self.try_advance_queue();
        }

        Ok(())
    }

    fn known_tool_call_ids(&self) -> HashSet<ToolCallId> {
        let mut ids: HashSet<ToolCallId> = self
            .tool_agents
            .keys()
            .chain(self.pending_tools.keys())
            .chain(self.completed_tool_calls.iter())
            .cloned()
            .collect();
        for tree in self.agent_store.agents() {
            for node in tree.nodes() {
                let tau_core::AgentEntry::AssistantResponse { output_items, .. } = &node.entry
                else {
                    continue;
                };
                ids.extend(output_items.iter().filter_map(|item| match item {
                    ContextItem::ToolCall(call) => Some(call.call_id.clone()),
                    _ => None,
                }));
            }
        }
        ids
    }

    /// Update one agent's `context_input_tokens` /
    /// `context_percent_used` from a finished agent response. Mirrors
    /// `update_context_usage` but scoped to a single conversation —
    /// the global tracker is intentionally only fed by user-agent
    /// turns so the status bar stays stable while side agents run.
    fn update_agent_context_usage(
        &mut self,
        cid: &AgentId,
        model: Option<&ModelId>,
        input_tokens: Option<u64>,
        cached_tokens: Option<u64>,
    ) {
        let context_window =
            model.and_then(|m| context_window_for_model(&self.provider_model_info, m));
        let percent_used = match (context_window, input_tokens) {
            (Some(w), Some(tokens)) => Some(context_percent_used(tokens, w)),
            _ => None,
        };
        if let Some(conv) = self.agents.get_mut(cid) {
            if input_tokens.is_some() {
                conv.context_input_tokens = input_tokens;
            }
            if cached_tokens.is_some() {
                conv.context_cached_tokens = cached_tokens;
            }
            if percent_used.is_some() {
                conv.context_percent_used = percent_used;
            }
        }
        self.publish_event(
            None,
            Event::HarnessAgentContextUsageChanged(HarnessAgentContextUsageChanged {
                agent_id: cid.clone(),
                input_tokens,
                cached_tokens,
                context_window,
                percent_used,
            }),
        );
    }

    /// True iff every configured extension has either reached `Ready`
    /// or dropped permanently.
    ///
    /// `Disconnected` counts as "no longer blocking": a dead tool extension
    /// may be on its way to being respawned, but the old connection is gone and
    /// should not wedge fresh prompt dispatch. Provider disconnects are handled
    /// as fatal by the event loop before this predicate matters for new work.
    pub(crate) fn extensions_all_ready(&self) -> bool {
        self.extensions.entries.values().all(|e| {
            matches!(
                e.state,
                ExtensionState::Ready | ExtensionState::Disconnected
            )
        })
    }

    /// Update an extension's lifecycle state, looked up by connection id.
    /// No-op if no entry matches (e.g. for socket clients).
    fn set_extension_state(&mut self, connection_id: &str, new_state: ExtensionState) {
        if let Some(entry) = self.extensions.entries.get_mut(connection_id) {
            entry.state = new_state;
        }
    }

    /// Returns the effective foreground/background support for a tool name.
    /// Missing registration metadata uses the protocol default of
    /// `MinForegroundSeconds(5)`.
    fn resolve_tool_background_support(&self, name: &str) -> BackgroundSupport {
        self.registry
            .resolve_provider(name)
            .and_then(|provider| provider.tool.background_support)
            .unwrap_or_else(BackgroundSupport::default_effective)
    }

    /// Drain scheduler-selected tool invocations into harness side effects.
    fn drain_pending_tool_invocations(&mut self) -> Result<(), HarnessError> {
        while let Some(next) = self.tool_turn.next_dispatchable().cloned() {
            if self.tool_call_waits_for_staged_registration(
                &next.conversation_id,
                &next.invocation.name,
                self.prompt_tool_call_prompts.get(&next.invocation.id),
            ) {
                break;
            }
            let Some((
                PendingToolInvocation {
                    conversation_id,
                    invocation,
                    background_support: _,
                },
                foreground_action,
            )) = self.tool_turn.pop_dispatchable(Instant::now())
            else {
                break;
            };
            let call_id = invocation.id.clone();
            // If dispatch fails synchronously, roll back the in-flight
            // entry so a retry or clean-up is not wedged on a phantom
            // slot.
            if let Err(error) = self.execute_agent_tool_call(&conversation_id, &invocation) {
                self.tool_turn.rollback_dispatch(&call_id);
                return Err(error);
            }
            self.apply_foreground_action(foreground_action);
        }
        Ok(())
    }

    fn apply_foreground_action(&mut self, action: ForegroundAction) {
        match action {
            ForegroundAction::None => {}
            ForegroundAction::Background { call_id } => {
                if self.tool_turn.mark_backgrounded(&call_id) {
                    self.publish_synthetic_background_result(&call_id);
                }
                self.on_tool_call_foreground_complete(call_id.as_str());
            }
        }
    }

    fn publish_synthetic_background_result(&mut self, call_id: &ToolCallId) {
        self.publish_synthetic_background_result_inner(call_id, None);
    }

    pub(crate) fn publish_internal_background_placeholder(
        &mut self,
        call_id: &ToolCallId,
        result: CborValue,
    ) {
        let Some(cid) = self.tool_agents.get(call_id).cloned() else {
            return;
        };
        let Some(tool) = self.pending_tools.get(call_id).cloned() else {
            return;
        };
        let result = ToolResult {
            call_id: call_id.clone(),
            tool_name: tool.name,
            tool_type: tool.tool_type,
            result,
            kind: ToolResultKind::BackgroundPlaceholder,
            originator: PromptOriginator::User,

            display: None,
        };
        self.publish_for_agent(&cid, Event::ProviderToolResult(result.clone()));
        self.record_wait_tool_result(result);
    }

    fn publish_synthetic_background_result_inner(
        &mut self,
        call_id: &ToolCallId,
        agent_ids: Option<(&str, &str)>,
    ) {
        let Some(cid) = self.tool_agents.get(call_id).cloned() else {
            return;
        };
        let Some(tool) = self.pending_tools.get(call_id).cloned() else {
            return;
        };
        let agent_id_headers = agent_ids
            .map(|(self_agent_id, sub_agent_id)| {
                format!("self_agent_id: {self_agent_id}\nsub_agent_id: {sub_agent_id}\n")
            })
            .unwrap_or_default();
        let content = format!(
            "{}: true\n{agent_id_headers}\nTool call `{call_id}` is running in the background.",
            tau_proto::TAU_INTERNAL_HEADER_NAME
        );
        let result = ToolResult {
            call_id: call_id.clone(),
            tool_name: tool.name,
            tool_type: tool.tool_type,
            result: CborValue::Text(content),
            kind: ToolResultKind::BackgroundPlaceholder,
            originator: PromptOriginator::User,

            display: None,
        };
        self.publish_for_agent(&cid, Event::ProviderToolResult(result.clone()));
        self.record_wait_tool_result(result);
    }

    fn process_background_deadlines(&mut self) {
        for call_id in self.tool_turn.background_due(Instant::now()) {
            self.publish_synthetic_background_result(&call_id);
            self.on_tool_call_foreground_complete(call_id.as_str());
        }
    }

    pub(crate) fn on_tool_call_foreground_complete(&mut self, call_id: &str) {
        let owner = self.tool_agents.get(call_id).cloned();
        if let Some(cid) = owner {
            self.emit_delegate_progress(&cid);
        }
        self.drain_pending_tool_invocations_or_report();
        self.maybe_complete_agent_turn(call_id);
        self.try_advance_queue();
    }

    fn drain_pending_tool_invocations_or_report(&mut self) {
        if let Err(error) = self.drain_pending_tool_invocations() {
            self.emit_harness_failure(&format!("queued tool dispatch failed: {error}"));
        }
    }

    fn handle_background_tool_result(&mut self, source_id: &str, mut result: ToolResult) {
        let Some(cid) = self.tool_agents.get(&result.call_id).cloned() else {
            return;
        };
        let call_id = result.call_id.clone();
        if let Some(tool) = self.pending_tools.get(&result.call_id) {
            result.tool_name = tool.name.clone();
            result.tool_type = tool.tool_type;
        }
        self.finish_tool_call_runtime_state(call_id.as_str());
        let background = ToolBackgroundResult {
            call_id: result.call_id,
            tool_name: result.tool_name,
            tool_type: result.tool_type,
            result: result.result,
            display: result.display,
            originator: result.originator,
        };
        self.publish_for_agent_from(
            &cid,
            Some(source_id),
            Event::ToolBackgroundResult(background.clone()),
        );
        self.record_wait_background_result(background);
        self.background_completion_targets
            .insert(call_id.clone(), cid.clone());
        self.queue_background_completion_prompt(&cid, &call_id);
        // Keep the completion prompt queued before draining. If an unblocked
        // queued call closes the tool round, `maybe_complete_agent_turn` can
        // fold this background notification into that follow-up prompt.
        self.drain_pending_tool_invocations_or_report();
        self.clear_tool_call_tracking(call_id.as_str());
    }

    fn handle_background_tool_error(&mut self, source: Option<&str>, error: ToolError) {
        self.handle_background_tool_error_inner(
            source,
            error,
            BackgroundCompletionPromptMode::QueueAndAdvance,
        );
    }

    fn handle_background_tool_cancelled(&mut self, source_id: &str, cancelled: ToolCancelled) {
        let error = ToolError {
            call_id: cancelled.call_id,
            tool_name: cancelled.tool_name,
            tool_type: cancelled.tool_type,
            message: "Tool cancelled".to_owned(),
            details: None,
            display: None,
            originator: PromptOriginator::User,
        };
        self.handle_background_tool_error(Some(source_id), error);
    }
    fn handle_background_tool_error_without_advancing(
        &mut self,
        source: Option<&str>,
        error: ToolError,
    ) {
        self.handle_background_tool_error_inner(
            source,
            error,
            BackgroundCompletionPromptMode::QueueOnly,
        );
    }

    fn handle_background_tool_error_inner(
        &mut self,
        source: Option<&str>,
        mut error: ToolError,
        completion_prompt_mode: BackgroundCompletionPromptMode,
    ) {
        let Some(cid) = self.tool_agents.get(&error.call_id).cloned() else {
            return;
        };
        let call_id = error.call_id.clone();
        if let Some(tool) = self.pending_tools.get(&error.call_id) {
            error.tool_name = tool.name.clone();
            error.tool_type = tool.tool_type;
        }
        self.tool_turn.mark_complete(&call_id);
        if let Some(conv) = self.agents.get_mut(&cid) {
            conv.tools_in_flight = conv.tools_in_flight.saturating_sub(1);
        }
        self.emit_delegate_progress(&cid);
        let background = ToolBackgroundError {
            call_id: error.call_id,
            tool_name: error.tool_name,
            tool_type: error.tool_type,
            message: error.message,
            details: error.details,
            display: error.display,
            originator: error.originator,
        };
        self.publish_terminal_background_error(&cid, source, background);
        match completion_prompt_mode {
            BackgroundCompletionPromptMode::QueueAndAdvance => {
                self.background_completion_targets
                    .insert(call_id.clone(), cid.clone());
                self.queue_background_completion_prompt(&cid, &call_id);
                // Keep the completion prompt queued before draining. If an unblocked
                // queued call closes the tool round, `maybe_complete_agent_turn` can
                // fold this background notification into that follow-up prompt.
                self.drain_pending_tool_invocations_or_report();
            }
            BackgroundCompletionPromptMode::QueueOnly => {
                self.background_completion_targets
                    .insert(call_id.clone(), cid.clone());
                self.queue_background_completion_prompt_without_advancing(&cid, &call_id);
            }
        }
        self.clear_tool_call_tracking(call_id.as_str());
    }

    fn queue_background_completion_prompt(&mut self, cid: &AgentId, call_id: &ToolCallId) {
        self.queue_background_completion_prompt_inner(cid, call_id, true);
    }

    fn queue_background_completion_prompt_without_advancing(
        &mut self,
        cid: &AgentId,
        call_id: &ToolCallId,
    ) {
        self.queue_background_completion_prompt_inner(cid, call_id, false);
    }

    fn queue_background_completion_prompt_inner(
        &mut self,
        cid: &AgentId,
        call_id: &ToolCallId,
        advance_queue: bool,
    ) {
        if self
            .suppressed_background_completion_prompts
            .contains(call_id)
        {
            return;
        }
        let prompt = background_completion_prompt(call_id);
        if let Some(conv) = self.agents.get_mut(cid) {
            if conv
                .pending_prompts
                .iter()
                .any(|pending| pending.text == prompt)
            {
                return;
            }
            conv.pending_prompts
                .push_back(PendingPrompt::internal(prompt));
        }
        if advance_queue {
            self.try_advance_queue();
        }
    }

    fn suppress_background_completion_prompt(&mut self, call_id: ToolCallId) {
        self.suppressed_background_completion_prompts
            .insert(call_id.clone());
        let prompt = background_completion_prompt(&call_id);
        for conv in self.agents.values_mut() {
            conv.pending_prompts
                .retain(|pending| pending.text != prompt);
        }
    }

    fn unsuppress_background_completion_prompt(&mut self, call_id: ToolCallId) {
        self.suppressed_background_completion_prompts
            .remove(&call_id);
        if let Some(cid) = self.background_completion_targets.get(&call_id).cloned() {
            self.queue_background_completion_prompt(&cid, &call_id);
        }
    }

    fn transfer_background_completion_target_before_teardown(&mut self, cid: &AgentId) {
        let call_ids = self.background_completion_call_ids_for_teardown(cid);
        if call_ids.is_empty() {
            return;
        }
        let Some(target_cid) = self.background_completion_teardown_target(cid) else {
            return;
        };
        for call_id in call_ids {
            if self.tool_agents.get(&call_id) == Some(cid)
                && self.tool_turn.is_backgrounded(&call_id)
            {
                self.tool_agents.insert(call_id.clone(), target_cid.clone());
            }
            if self.background_completion_targets.get(&call_id) == Some(cid) {
                self.background_completion_targets
                    .insert(call_id.clone(), target_cid.clone());
            }
            self.transfer_wait_background_owner_before_teardown(&call_id, cid, &target_cid);
            self.transfer_queued_background_completion_prompt(cid, &target_cid, &call_id);
        }
    }

    fn discard_background_completion_target_before_teardown(&mut self, cid: &AgentId) {
        for call_id in self.background_completion_call_ids_for_teardown(cid) {
            self.suppressed_background_completion_prompts
                .remove(&call_id);
            self.background_completion_targets.remove(&call_id);
            self.discard_wait_background_owner_before_teardown(&call_id, cid);
            self.clear_tool_call_tracking(call_id.as_str());
        }
    }

    fn background_completion_call_ids_for_teardown(&self, cid: &AgentId) -> HashSet<ToolCallId> {
        let mut call_ids: HashSet<ToolCallId> = self
            .tool_turn
            .backgrounded_calls_for(cid)
            .into_iter()
            .collect();
        call_ids.extend(self.tool_agents.iter().filter_map(|(call_id, owner)| {
            (owner == cid && self.tool_turn.is_backgrounded(call_id)).then_some(call_id.clone())
        }));
        call_ids.extend(
            self.background_completion_targets
                .iter()
                .filter_map(|(call_id, owner)| (owner == cid).then_some(call_id.clone())),
        );
        call_ids
    }

    fn transfer_queued_background_completion_prompt(
        &mut self,
        source_cid: &AgentId,
        target_cid: &AgentId,
        call_id: &ToolCallId,
    ) {
        let prompt = background_completion_prompt(call_id);
        let removed = self.agents.get_mut(source_cid).is_some_and(|conv| {
            remove_pending_internal_prompt_text(&mut conv.pending_prompts, &prompt)
        });
        if removed {
            self.queue_background_completion_prompt(target_cid, call_id);
        }
    }

    fn background_completion_teardown_target(&self, cid: &AgentId) -> Option<AgentId> {
        let conv = self.agents.get(cid)?;
        if let Some(parent_cid) = &conv.parent_agent_id
            && parent_cid != cid
            && self.agents.contains_key(parent_cid)
        {
            return Some(parent_cid.clone());
        }
        if let Some(parent_call_id) = &conv.parent_tool_call_id
            && let Some(parent_cid) = self.tool_agents.get(parent_call_id)
            && parent_cid != cid
            && self.agents.contains_key(parent_cid)
        {
            return Some(parent_cid.clone());
        }
        self.agents.iter().find_map(|(candidate_cid, candidate)| {
            (candidate_cid != cid && candidate.session_id == conv.session_id)
                .then_some(candidate_cid.clone())
        })
    }

    /// Hook called whenever a tool call has finished (result, error,
    /// synthetic NoProvider error, or inline skill completion). Removes
    /// it from the in-flight set, drains any freshly-eligible queued
    /// calls, and then checks whether the turn is done.
    pub(crate) fn on_tool_call_complete(&mut self, call_id: &str) {
        self.on_tool_call_complete_inner(call_id, true);
    }

    fn on_tool_call_complete_inner(&mut self, call_id: &str, drain_queued: bool) {
        let owner = self.finish_tool_call_runtime_state(call_id);
        if drain_queued {
            self.drain_pending_tool_invocations_or_report();
        }
        if let Some(cid) = owner {
            self.maybe_complete_agent_turn_for(&cid, call_id);
        }
        self.try_advance_queue();
    }

    fn finish_tool_call_runtime_state(&mut self, call_id: &str) -> Option<AgentId> {
        let owned: ToolCallId = call_id.to_owned().into();
        self.tool_turn.mark_complete(&owned);
        // `tool_agents` is still populated here: the call
        // sites clear it *after* this function returns. Decrement
        // the agent's in-flight counter and surface the new
        // state to any UI watching this delegate flow before the
        // mapping is cleared.
        let owner = self.tool_agents.get(call_id).cloned();
        if let Some(cid) = owner.as_ref()
            && let Some(conv) = self.agents.get_mut(cid)
        {
            conv.tools_in_flight = conv.tools_in_flight.saturating_sub(1);
        }
        if let Some(cid) = owner.as_ref() {
            self.emit_delegate_progress(cid);
        }
        owner
    }

    /// Bump the per-agent tool counters for a freshly-started
    /// tool call. Always emits a `DelegateProgress` snapshot when the
    /// conversation is a delegate side agent (no-op otherwise),
    /// so the UI updates the moment the sub-agent starts a new call
    /// rather than waiting for completion.
    pub(crate) fn bump_tools_started_for(&mut self, cid: &AgentId) {
        if let Some(conv) = self.agents.get_mut(cid) {
            conv.tools_in_flight = conv.tools_in_flight.saturating_add(1);
            conv.tools_total = conv.tools_total.saturating_add(1);
        }
        self.emit_delegate_progress(cid);
    }

    fn maybe_complete_agent_turn(&mut self, completed_call_id: &str) {
        let Some(cid) = self.tool_agents.get(completed_call_id).cloned() else {
            return;
        };
        self.maybe_complete_agent_turn_for(&cid, completed_call_id);
    }

    fn maybe_complete_agent_turn_for(&mut self, cid: &AgentId, completed_call_id: &str) {
        let should_send = if let Some(conv) = self.agents.get_mut(cid) {
            if let AgentTurnState::ToolsRunning { remaining_calls } = &mut conv.turn_state {
                remaining_calls.retain(|id| id.as_str() != completed_call_id);
                if remaining_calls.is_empty() {
                    conv.turn_state = AgentTurnState::Idle;
                    true
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        };
        if should_send {
            self.fold_pending_prompts_as_steered(cid);
            // If folding the steered prompts parked any of them in
            // interception (e.g. an extension intercepting
            // `agent.prompt_steered`), defer the agent dispatch
            // until the whole publish chain drains. Waiting for only
            // one user-message commit is not enough when several
            // steered prompts are queued behind one interceptor.
            self.dispatch_prompt_after_publish_idle(cid);
        }
    }

    fn has_pending_message_received_prompt(&self, cid: &AgentId) -> bool {
        self.agents.get(cid).is_some_and(|conv| {
            conv.pending_prompts
                .iter()
                .any(PendingPrompt::is_agent_message_received)
        })
    }

    fn publish_prompts_as_steered(&mut self, cid: &AgentId, prompts: Vec<PendingPrompt>) {
        for prompt in prompts {
            let agent_id = self
                .agents
                .get(cid)
                .and_then(|conv| conv.agent_id.clone())
                .expect("agent has durable id");
            self.publish_for_agent(
                cid,
                Event::AgentPromptSteered(tau_proto::AgentPromptSteered {
                    agent_id: crate::parse_agent_id(&agent_id),
                    text: prompt.text,
                    message_class: prompt.message_class,
                }),
            );
        }
    }

    /// Drain any prompts queued on `cid` while the agent was in
    /// flight, and publish a `AgentPromptSteered` event for each. The
    /// folder in `AgentTree::apply_event` appends them as
    /// `UserMessage` entries on this agent's branch, so the
    /// next-round `AgentPromptCreated` (about to be emitted by the
    /// caller) picks them up alongside the tool results without any
    /// extra wiring on the prompt-assembly side.
    ///
    /// Called from `maybe_complete_agent_turn` only — fresh prompts
    /// arriving on an idle conversation go through
    /// `dispatch_prompt_for_agent`, which already publishes its
    /// own `AgentPromptSubmitted`. Folding here exists specifically to
    /// give queued prompts a chance to ride the next per-round prompt
    /// rather than waiting for the whole turn to terminate.
    fn fold_pending_prompts_as_steered(&mut self, cid: &AgentId) {
        let mut pending: Vec<PendingPrompt> = self
            .agents
            .get_mut(cid)
            .map(|c| c.pending_prompts.drain(..).collect())
            .unwrap_or_default();
        if let Some(user_prompt_pos) = pending.iter().position(|prompt| !prompt.is_internal()) {
            let restore_prompts = self.take_pending_restore_prompts_for_user_prompt(cid);
            if !restore_prompts.is_empty() {
                pending.splice(user_prompt_pos..user_prompt_pos, restore_prompts);
            }
        }
        self.publish_prompts_as_steered(cid, pending);
    }

    fn reject_agent_tool_call_before_dispatch(
        &mut self,
        cid: &AgentId,
        call: &AgentToolCall,
        tool_name: ToolName,
        message: String,
    ) {
        self.reject_agent_tool_call_before_dispatch_inner(cid, call, tool_name, message, true);
    }

    fn reject_agent_tool_call_before_dispatch_without_followup(
        &mut self,
        cid: &AgentId,
        call: &AgentToolCall,
        tool_name: ToolName,
        message: String,
    ) {
        self.reject_agent_tool_call_before_dispatch_inner(cid, call, tool_name, message, false);
    }

    fn reject_agent_tool_call_before_dispatch_inner(
        &mut self,
        cid: &AgentId,
        call: &AgentToolCall,
        tool_name: ToolName,
        message: String,
        complete_turn: bool,
    ) {
        let call_id: ToolCallId = call.id.clone();
        self.tool_agents.insert(call_id.clone(), cid.clone());
        self.publish_terminal_tool_error(
            Some(cid),
            None,
            ToolError {
                call_id: call_id.clone(),
                tool_name,
                tool_type: call.tool_type,
                message,
                details: None,
                originator: tau_proto::PromptOriginator::User,

                display: None,
            },
        );
        if complete_turn {
            self.on_tool_call_complete(call_id.as_str());
        } else {
            self.finish_tool_call_runtime_state(call_id.as_str());
        }
        self.clear_tool_call_tracking(call_id.as_str());
    }

    fn tool_owner_agent_id(&self, cid: &AgentId) -> AgentId {
        self.agents
            .get(cid)
            .and_then(|conv| conv.agent_id.clone())
            .map(crate::parse_agent_id)
            .unwrap_or_else(|| cid.clone())
    }

    fn tool_owner_originator(&self, cid: &AgentId) -> PromptOriginator {
        self.agents
            .get(cid)
            .map(|conv| conv.originator.clone())
            .unwrap_or_default()
    }

    fn execute_agent_tool_call(
        &mut self,
        cid: &AgentId,
        call: &AgentToolCall,
    ) -> Result<(), HarnessError> {
        let tool_name = call.name.clone();
        let role_name = self.role_name_for_agent_id(cid).to_owned();

        let prompt_tool_spec = self
            .prompt_tool_call_prompts
            .get(&call.id)
            .map(|prompt_id| self.resolve_enabled_tool_spec_for_prompt(&tool_name, prompt_id));
        let current_role_tool_spec =
            || self.resolve_enabled_tool_spec_for_role(&tool_name, &role_name);
        let Some(tool_spec) = prompt_tool_spec.unwrap_or_else(current_role_tool_spec) else {
            let message = if self.has_registered_tool_name(&tool_name) {
                "tool is not enabled for the current role".to_owned()
            } else {
                unavailable_tool_error_message(&tool_name)
            };
            let call_id: ToolCallId = call.id.clone();
            let owner_agent_id = self.tool_owner_agent_id(cid);
            let owner_originator = self.tool_owner_originator(cid);
            self.tool_agents.insert(call_id.clone(), cid.clone());
            self.pending_tools.insert(
                call_id.clone(),
                PendingTool {
                    name: tool_name.clone(),
                    internal_name: tool_name.clone(),
                    tool_type: call.tool_type,
                },
            );
            self.bump_tools_started_for(cid);
            self.record_wait_tool_request(&call_id);
            let request = ToolRequest {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                tool_type: call.tool_type,
                arguments: call.arguments.clone(),
                agent_id: owner_agent_id,
                originator: owner_originator.clone(),
            };
            self.publish_for_agent(cid, Event::ToolRequest(request));
            self.publish_terminal_tool_error(
                Some(cid),
                None,
                ToolError {
                    call_id: call_id.clone(),
                    tool_name,
                    tool_type: call.tool_type,
                    message,
                    details: None,
                    originator: owner_originator,

                    display: None,
                },
            );
            self.on_tool_call_complete(call_id.as_str());
            self.clear_tool_call_tracking(call_id.as_str());
            return Ok(());
        };
        let internal_tool_name = tool_spec.name.clone();
        let visible_tool_name = self.tool_model_visible_name(tool_spec).clone();
        if self
            .registry
            .resolve_provider(&internal_tool_name)
            .is_some()
            && let Err(error) = validate_tool_arguments(tool_spec, &call.arguments)
        {
            self.reject_agent_tool_call_before_dispatch(
                cid,
                call,
                visible_tool_name,
                format!("invalid arguments for tool `{tool_name}`: {error}"),
            );
            return Ok(());
        }

        let call_id: ToolCallId = call.id.clone();
        let owner_agent_id = self.tool_owner_agent_id(cid);
        let owner_originator = self.tool_owner_originator(cid);

        // Track conversation attribution before publishing the runtime
        // `ToolRequest`; terminal tool facts use this metadata to fold into the
        // owning agent transcript.
        self.tool_agents.insert(call_id.clone(), cid.clone());
        self.pending_tools.insert(
            call_id.clone(),
            PendingTool {
                name: visible_tool_name.clone(),
                internal_name: internal_tool_name.clone(),
                tool_type: call.tool_type,
            },
        );
        self.bump_tools_started_for(cid);
        self.record_wait_tool_request(&call_id);
        let published_request = ToolRequest {
            call_id: call_id.clone(),
            tool_name: visible_tool_name.clone(),
            tool_type: call.tool_type,
            arguments: call.arguments.clone(),
            agent_id: owner_agent_id.clone(),
            originator: owner_originator.clone(),
        };
        self.publish_for_agent(cid, Event::ToolRequest(published_request));
        let request = ToolRequest {
            call_id: call_id.clone(),
            tool_name: internal_tool_name.clone(),
            tool_type: call.tool_type,
            arguments: call.arguments.clone(),
            agent_id: owner_agent_id.clone(),
            originator: owner_originator.clone(),
        };

        match self.registry.route_tool_request(request) {
            Ok(route) => {
                let started = route.invoke;
                match route.target {
                    ToolRouteTarget::Internal => {
                        self.publish_for_agent(cid, Event::ToolStarted(started));
                    }
                    ToolRouteTarget::Extension(provider_connection_id) => {
                        self.ensure_tool_started_subscription(&provider_connection_id);
                        self.pending_tool_providers
                            .insert(call_id.clone(), provider_connection_id);
                        self.publish_for_agent(cid, Event::ToolStarted(started));
                    }
                }
            }
            Err(ToolRouteError::NoProvider { tool_name: _ }) => {
                let message = unavailable_tool_error_message(&visible_tool_name);
                self.publish_for_agent(
                    cid,
                    Event::ToolRejected(ToolRejected {
                        call_id: call_id.clone(),
                        tool_name: visible_tool_name.clone(),
                        tool_type: call.tool_type,
                        message: message.clone(),
                        originator: tau_proto::PromptOriginator::User,
                    }),
                );
                let error = ToolError {
                    call_id: call_id.clone(),
                    tool_name: visible_tool_name.clone(),
                    tool_type: call.tool_type,
                    message,
                    details: None,
                    originator: tau_proto::PromptOriginator::User,

                    display: None,
                };
                self.publish_terminal_tool_error(Some(cid), None, error);
                self.on_tool_call_complete(&call.id);
                self.clear_tool_call_tracking(call_id.as_str());
            }
            Err(error) => return Err(HarnessError::ToolRoute(error)),
        }

        Ok(())
    }
}

fn escape_agent_message_for_prompt(message: &str) -> String {
    let mut escaped = String::with_capacity(message.len());
    for ch in message.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

impl Harness {
    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    pub(crate) fn send_user_message(
        &mut self,
        session_id: &str,
        text: &str,
        _source_id: Option<&str>,
    ) -> Result<InteractionOutcome, HarnessError> {
        // Synchronous test entrypoint: dispatch directly without going
        // through `submit_user_prompt`'s queue. The embedded test harness
        // has no provider-published model (nothing to select from) and no UI
        // to drain a queued prompt, so the queued-until-model path would
        // deadlock. AGENTS.md session init is exercised separately in
        // unit tests via `submit_user_prompt` / manual turn-state setup.
        self.dispatch_user_prompt(session_id.into(), text.to_owned())?;

        let started_at = Instant::now();
        let mut progress_messages = Vec::new();
        loop {
            self.process_background_deadlines();
            let remaining = RESPONSE_TIMEOUT
                .checked_sub(started_at.elapsed())
                .unwrap_or(Duration::ZERO);
            let wait = self
                .tool_turn
                .next_background_deadline()
                .map(|deadline| {
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(remaining)
                })
                .unwrap_or(remaining);
            let harness_evt = match self.rx.recv_timeout(wait) {
                Ok(event) => event,
                Err(mpsc::RecvTimeoutError::Timeout)
                    if started_at.elapsed() < RESPONSE_TIMEOUT
                        && self.tool_turn.next_background_deadline().is_some() =>
                {
                    self.process_background_deadlines();
                    continue;
                }
                Err(_) => return Err(HarnessError::ResponseTimeout),
            };
            self.log_event(&harness_evt);
            match harness_evt {
                HarnessEvent::FromConnection {
                    connection_id,
                    message,
                } => {
                    let event = match message.as_ref() {
                        HarnessInputMessage::Emit(emit) => Some(emit.event.as_ref()),
                        _ => None,
                    };
                    if let Some(Event::ToolProgress(progress)) = event {
                        progress_messages.push(format_tool_progress(progress));
                    }
                    let is_final = matches!(
                        event,
                        Some(Event::ProviderResponseFinished(r))
                            if tool_calls_from_output_items(&r.output_items).is_empty()
                                && r.originator.is_user()
                    );
                    let final_text = match event {
                        Some(Event::ProviderResponseFinished(r)) => {
                            assistant_text_from_output_items(&r.output_items)
                        }
                        _ => None,
                    };
                    self.handle_extension_message(&connection_id, *message)?;
                    if is_final {
                        return Ok(InteractionOutcome {
                            lifecycle_messages: Vec::new(),
                            progress_messages,
                            response: final_text.unwrap_or_default(),
                        });
                    }
                }
                HarnessEvent::Disconnected { connection_id } => {
                    let was_provider = self.is_provider_extension(&connection_id);
                    self.handle_disconnect(&connection_id);
                    if was_provider {
                        return Err(provider_disconnected_error());
                    }
                }
                HarnessEvent::NewClient(_) => {}
                HarnessEvent::Command(command) => self.handle_harness_command(command)?,
            }
        }
    }

    pub(crate) fn dump_initial_prompt(
        out_path: &Path,
        user_message: &str,
    ) -> Result<(), HarnessError> {
        let tempdir = tempfile::TempDir::new()?;
        let state_dir = tempdir.path().join("state");
        let config = crate::settings::default_config();
        let mut harness = Self::from_config(
            &config,
            &state_dir,
            tau_config::settings::TauDirs::default(),
            "s1",
            tau_proto::SessionStartReason::Initial,
        )?;
        harness.selected_model = Some("test/model".parse().expect("model id"));

        let role = harness.selected_role.clone();
        let cid = harness.create_durable_user_agent("s1".into(), &role);
        let agent_id = harness
            .target_agent_id_for_agent(&cid)
            .expect("agent has durable id");
        harness.publish_event_for_agent(
            &cid,
            None,
            Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                agent_id: crate::parse_agent_id(&agent_id),
                text: user_message.to_owned(),
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                display_name: None,
                ctx_id: None,
            }),
        );

        let prompt = harness
            .prepare_agent_prompt_for_dispatch(&cid)
            .ok_or_else(|| HarnessError::Participant("no model available for prompt".to_owned()))?;
        let mut out = String::new();
        out.push_str("================ MODEL / EFFORT ================\n");
        out.push_str(&format!("model:  {}\n", prompt.model));
        out.push_str(&format!("params: {:?}\n\n", prompt.model_params));

        out.push_str("================ SYSTEM PROMPT ================\n");
        out.push_str(&prompt.system_prompt);
        if !prompt.system_prompt.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');

        out.push_str("================ PROMPT CONTEXT ================\n");
        out.push_str(
            &serde_json::to_string_pretty(&prompt.context)
                .map_err(|e| HarnessError::Participant(e.to_string()))?,
        );
        out.push_str("\n\n");

        out.push_str("================ TOOLS ================\n");
        out.push_str(
            &serde_json::to_string_pretty(&prompt.tools)
                .map_err(|e| HarnessError::Participant(e.to_string()))?,
        );
        out.push('\n');

        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(out_path, out)?;
        harness.shutdown()?;
        Ok(())
    }

    #[cfg(test)]
    fn read_agent_prompt_created(
        &self,
        session_id: &SessionId,
        prompt_id: &AgentPromptId,
    ) -> Result<AgentPromptCreated, HarnessError> {
        let mut cursor = crate::event_log::EventLogSeq::new(0);
        loop {
            let entry = self.event_log.get_next_from(cursor).ok_or_else(|| {
                HarnessError::Participant("prompt event missing from test observer".to_owned())
            })?;
            cursor = entry.seq.next();
            if let Event::AgentPromptCreated(prompt) = entry.event {
                if prompt.tools_ref.is_some() {
                    return Err(HarnessError::Participant(
                        "test prompt reader cannot materialize tools_ref prompts without prompt snapshots"
                            .to_owned(),
                    ));
                }
                if &prompt.session_id == session_id && &prompt.agent_prompt_id == prompt_id {
                    return Ok(prompt);
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Shutdown
    // -----------------------------------------------------------------------

    pub(crate) fn shutdown(&mut self) -> Result<(), HarnessError> {
        // Disconnect all extensions from the bus.  Dropping the
        // ChannelSink closes the writer channel, which triggers each
        // writer thread's shutdown sequence (send disconnect, close
        // stdin, wait/kill child). Walk extension spawn order so shutdown
        // honours spawn order.
        for id in &self.extensions.order {
            let _ = self.bus.disconnect(id);
        }

        // Join in-process extension threads.
        let order = self.extensions.order.clone();
        for id in &order {
            let Some(entry) = self.extensions.entries.get_mut(id) else {
                continue;
            };
            let name = entry.name.clone();
            if let Some(handle) = entry.in_process_thread.take() {
                let result = handle
                    .join()
                    .map_err(|_| HarnessError::ThreadJoin(name.clone()))?;
                result.map_err(HarnessError::Participant)?;
            }
            self.emit_extension_exited(&name);
        }
        Ok(())
    }

    #[cfg(test)]
    fn extension_connection_id(&self, name: &str) -> Option<&str> {
        self.extensions
            .entries
            .values()
            .find(|e| e.name == name)
            .map(|e| e.connection_id.as_str())
    }
}

fn provider_disconnected_error() -> HarnessError {
    HarnessError::Participant("provider disconnected".to_owned())
}

fn build_delegate_progress_display(
    task_name: &str,
    ctx_input_tokens: Option<u64>,
    ctx_percent: Option<u8>,
    ctx_window: Option<u64>,
    tools_in_flight: u32,
    tools_total: u32,
    input_stats: tau_proto::ToolUseStats,
) -> tau_proto::ToolUseState {
    use tau_proto::{ProgressCounter, ProgressUnit, ToolUseStatus};

    let tools_completed = tools_total.saturating_sub(tools_in_flight);
    let mut counters: Vec<ProgressCounter> = vec![ProgressCounter {
        label: Some("tools".to_owned()),
        unit: ProgressUnit::Count,
        complete: Some(u64::from(tools_completed)),
        total: Some(u64::from(tools_total)),
    }];
    if ctx_input_tokens.is_some() || ctx_window.is_some() {
        counters.push(ProgressCounter {
            label: Some("ctx".to_owned()),
            unit: ProgressUnit::Tokens,
            complete: ctx_input_tokens,
            total: ctx_window,
        });
    } else if ctx_percent.is_some() {
        counters.push(ProgressCounter {
            label: Some("ctx".to_owned()),
            unit: ProgressUnit::Percent,
            complete: ctx_percent.map(u64::from),
            total: None,
        });
    }
    tau_proto::ToolUseState {
        args: format!("[{task_name}]"),
        stats: input_stats,
        progress_counters: counters,
        status: ToolUseStatus::InProgress,
        status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
        ..Default::default()
    }
}

/// Replace the `originator` on a tool-related event with the owning
/// agent's originator. Non-tool events pass through unchanged.
fn stamp_tool_event_originator(event: Event, originator: tau_proto::PromptOriginator) -> Event {
    match event {
        Event::ToolRequest(mut e) => {
            e.originator = originator;
            Event::ToolRequest(e)
        }
        Event::ToolStarted(mut e) => {
            e.originator = originator;
            Event::ToolStarted(e)
        }
        Event::ToolRejected(mut e) => {
            e.originator = originator;
            Event::ToolRejected(e)
        }
        Event::ToolResult(mut e) => {
            e.originator = originator;
            Event::ToolResult(e)
        }
        Event::ToolError(mut e) => {
            e.originator = originator;
            Event::ToolError(e)
        }
        Event::ProviderToolResult(mut e) => {
            e.originator = originator;
            Event::ProviderToolResult(e)
        }
        Event::ProviderToolError(mut e) => {
            e.originator = originator;
            Event::ProviderToolError(e)
        }
        Event::ToolBackgroundResult(mut e) => {
            e.originator = originator;
            Event::ToolBackgroundResult(e)
        }
        Event::ToolBackgroundError(mut e) => {
            e.originator = originator;
            Event::ToolBackgroundError(e)
        }
        other => other,
    }
}

pub(crate) fn selector_matches_event(selectors: &[EventSelector], event: &Event) -> bool {
    let target_name = event.name();
    selectors.iter().any(|selector| match selector {
        EventSelector::Exact(expected) => *expected == target_name,
        EventSelector::Prefix(prefix) => target_name.matches_prefix(prefix),
    })
}
