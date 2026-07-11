//! Building blocks for the per-turn prompt: the system prompt body, the
//! AGENTS.md context message, and the conversation assembly that turns a
//! [`tau_core::AgentTree`] into item-based prompt context.

use tau_core::AgentEntry;
use tau_proto::{ContextItem, PromptFragment, ToolName};

use crate::discovery::{DiscoveredAgentsFile, DiscoveredSkill};
pub(crate) const BUILT_IN_SYSTEM_TEMPLATE_NAME: &str = "built-in";
const BUILT_IN_SYSTEM_PROMPT_TEMPLATE: &str = include_str!("../prompts/system.hbs");
const BIG_SYSTEM_TEMPLATE_NAME: &str = "big";
const BIG_SYSTEM_PROMPT_TEMPLATE: &str = include_str!("../prompts/big.hbs");

pub(crate) fn built_in_system_prompt_templates() -> std::collections::HashMap<String, String> {
    std::collections::HashMap::from([
        (
            BUILT_IN_SYSTEM_TEMPLATE_NAME.to_owned(),
            BUILT_IN_SYSTEM_PROMPT_TEMPLATE.to_owned(),
        ),
        (
            BIG_SYSTEM_TEMPLATE_NAME.to_owned(),
            BIG_SYSTEM_PROMPT_TEMPLATE.to_owned(),
        ),
    ])
}

/// Tool-scoped prompt fragment plus the model-visible tool name used for its
/// heading.
#[derive(Clone, Debug)]
pub(crate) struct ToolPromptFragment {
    /// Model-visible tool name used in the automatic prompt-fragment heading.
    pub(crate) tool_name: ToolName,
    /// Original tool-scoped prompt fragment template registered by the
    /// provider.
    pub(crate) fragment: PromptFragment,
}

impl ToolPromptFragment {
    /// Create a tool-scoped prompt fragment wrapper.
    #[cfg(test)]
    pub(crate) fn new(tool_name: ToolName, fragment: PromptFragment) -> Self {
        Self {
            tool_name,
            fragment,
        }
    }
}

/// Context made available to role prompt Handlebars templates.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RolePromptTemplateContext<'a> {
    /// Name of the role whose prompt is being rendered.
    pub(crate) role_name: &'a str,
    /// Durable agent id whose prompt is being rendered, when the render targets
    /// a concrete agent instead of a role-only preview.
    pub(crate) agent_id: Option<&'a tau_proto::AgentId>,
}

impl<'a> RolePromptTemplateContext<'a> {
    /// Build template context for a role-only render.
    pub(crate) fn for_role(role_name: &'a str) -> Self {
        Self {
            role_name,
            agent_id: None,
        }
    }

    /// Build template context for a concrete agent prompt render.
    pub(crate) fn for_agent(role_name: &'a str, agent_id: &'a tau_proto::AgentId) -> Self {
        Self {
            role_name,
            agent_id: Some(agent_id),
        }
    }
}

/// Builds the system prompt from Tau defaults plus role prompt and prompt
/// fragments.
///
/// Must be deterministic and stable across turns of the same session
/// — see the linear-prefix invariant in `send_prompt_to_agent`.
/// Tools and skills are sorted by name (HashMap iteration would
/// otherwise drift). The current date is intentionally omitted:
/// including it would invalidate the prompt cache every midnight
/// UTC.
#[cfg(test)]
pub(crate) fn build_system_prompt(
    skills: &std::collections::HashMap<tau_proto::SkillName, DiscoveredSkill>,
    prompt_fragments: &[PromptFragment],
) -> String {
    build_system_prompt_with_template_context(
        BUILT_IN_SYSTEM_PROMPT_TEMPLATE,
        skills,
        prompt_fragments,
        serde_json::json!({}),
        RolePromptTemplateContext::for_role(""),
    )
}

/// Builds the system prompt with role prompt sections rendered as Handlebars.
#[cfg(test)]
pub(crate) fn build_system_prompt_with_template_context(
    system_template: &str,
    skills: &std::collections::HashMap<tau_proto::SkillName, DiscoveredSkill>,
    prompt_fragments: &[PromptFragment],
    agent_context: serde_json::Value,
    template_context: RolePromptTemplateContext<'_>,
) -> String {
    build_system_prompt_with_tool_template_context(
        system_template,
        skills,
        prompt_fragments,
        &[],
        agent_context,
        template_context,
    )
}

/// Builds the system prompt with ordinary prompt fragments and tool-scoped
/// prompt fragments rendered into separate template sections.
pub(crate) fn build_system_prompt_with_tool_template_context(
    system_template: &str,
    skills: &std::collections::HashMap<tau_proto::SkillName, DiscoveredSkill>,
    prompt_fragments: &[PromptFragment],
    tool_prompt_fragments: &[ToolPromptFragment],
    agent_context: serde_json::Value,
    template_context: RolePromptTemplateContext<'_>,
) -> String {
    // Tool definitions are delivered out-of-band via the provider's
    // tool-use channel, so the built-in system template doesn't restate them.
    let fragments: Vec<_> = prompt_fragments.to_vec();
    let tool_fragments: Vec<_> = tool_prompt_fragments.to_vec();
    render_system_prompt_template(
        system_template,
        template_context,
        skills,
        &fragments,
        &tool_fragments,
        agent_context,
    )
}

fn render_system_prompt_template(
    system_template: &str,
    context: RolePromptTemplateContext<'_>,
    skills: &std::collections::HashMap<tau_proto::SkillName, DiscoveredSkill>,
    prompt_fragments: &[PromptFragment],
    tool_prompt_fragments: &[ToolPromptFragment],
    agent_context: serde_json::Value,
) -> String {
    let data = system_prompt_template_data(
        context,
        skills,
        prompt_fragments,
        tool_prompt_fragments,
        agent_context,
    );
    let handlebars = prompt_template_renderer();
    match handlebars.render_template(system_template, &data) {
        Ok(rendered) => rendered,
        Err(error) => {
            tracing::warn!(
                role = context.role_name,
                error = %error,
                "failed to render system prompt handlebars template"
            );
            match handlebars.render_template(BUILT_IN_SYSTEM_PROMPT_TEMPLATE, &data) {
                Ok(rendered) => rendered,
                Err(error) => {
                    tracing::warn!(
                        role = context.role_name,
                        error = %error,
                        "failed to render built-in system prompt handlebars template; using unrendered template"
                    );
                    BUILT_IN_SYSTEM_PROMPT_TEMPLATE.to_owned()
                }
            }
        }
    }
}

fn prompt_template_data(
    context: RolePromptTemplateContext<'_>,
    skills: &std::collections::HashMap<tau_proto::SkillName, DiscoveredSkill>,
    agent_context: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "role": {
            "name": context.role_name,
        },
        "agent_id": context.agent_id.map(ToString::to_string),
        "skills": prompt_template_skills(skills),
        "agent_context": agent_context,
    })
}

fn system_prompt_template_data(
    context: RolePromptTemplateContext<'_>,
    skills: &std::collections::HashMap<tau_proto::SkillName, DiscoveredSkill>,
    prompt_fragments: &[PromptFragment],
    tool_prompt_fragments: &[ToolPromptFragment],
    agent_context: serde_json::Value,
) -> serde_json::Value {
    let mut data = prompt_template_data(context, skills, agent_context);
    let rendered_fragments = rendered_prompt_fragment_template_parts(prompt_fragments, &data);
    let rendered_tool_fragments =
        rendered_tool_prompt_fragment_template_parts(tool_prompt_fragments, &data);
    let object = data
        .as_object_mut()
        .expect("system prompt template data is an object");
    object.insert("prompt_fragments".to_owned(), rendered_fragments);
    object.insert("tool_prompt_fragments".to_owned(), rendered_tool_fragments);
    data
}

fn rendered_prompt_fragment_template_parts(
    fragments: &[PromptFragment],
    data: &serde_json::Value,
) -> serde_json::Value {
    let handlebars = prompt_template_renderer();
    serde_json::Value::Array(
        {
            let mut ordered = fragments.iter().collect::<Vec<_>>();
            // Preserve the caller's deterministic source/name tie-break within
            // a priority bucket. The harness gathers tool fragments in
            // priority/source/name order before rendering.
            ordered.sort_by_key(|a| a.priority);
            ordered
        }
        .into_iter()
        .filter_map(|fragment| {
            if fragment.template.is_empty() {
                return None;
            }
            let content = match handlebars.render_template(fragment.template.as_str(), data) {
                Ok(rendered) => rendered,
                Err(error) => {
                    tracing::warn!(
                        fragment_name = fragment.name,
                        priority = fragment.priority.get(),
                        error = %error,
                        "failed to render prompt fragment template; skipping fragment"
                    );
                    return None;
                }
            };
            Some(serde_json::json!({
                "name": fragment.name,
                "priority": fragment.priority.get(),
                "content": content,
                "early": fragment.priority.get() < 100,
            }))
        })
        .collect(),
    )
}

fn rendered_tool_prompt_fragment_template_parts(
    fragments: &[ToolPromptFragment],
    data: &serde_json::Value,
) -> serde_json::Value {
    let handlebars = prompt_template_renderer();
    serde_json::Value::Array(
        {
            let mut ordered = fragments.iter().collect::<Vec<_>>();
            // Preserve the caller's deterministic source/name tie-break within
            // a priority bucket. The harness gathers tool fragments in
            // priority/source/name order before rendering.
            ordered.sort_by_key(|item| item.fragment.priority);
            ordered
        }
        .into_iter()
        .filter_map(|item| {
            let fragment = &item.fragment;
            if fragment.template.is_empty() {
                return None;
            }
            let rendered = match handlebars.render_template(fragment.template.as_str(), data) {
                Ok(rendered) => rendered,
                Err(error) => {
                    tracing::warn!(
                        fragment_name = fragment.name,
                        priority = fragment.priority.get(),
                        error = %error,
                        "failed to render tool prompt fragment template; skipping fragment"
                    );
                    return None;
                }
            };
            let rendered = rendered.trim();
            if rendered.is_empty() {
                return None;
            }
            Some(serde_json::json!({
                "name": fragment.name,
                "priority": fragment.priority.get(),
                "tool_name": item.tool_name,
                "content": rendered,
                "early": fragment.priority.get() < 100,
            }))
        })
        .collect(),
    )
}
fn prompt_template_renderer() -> handlebars::Handlebars<'static> {
    let mut handlebars = handlebars::Handlebars::new();
    handlebars.set_strict_mode(true);
    handlebars.register_escape_fn(handlebars::no_escape);
    handlebars.register_helper("sort", Box::new(SortHelper));
    handlebars.register_helper("eq", Box::new(EqHelper));
    handlebars.register_helper("starts_with", Box::new(StartsWithHelper));
    handlebars.register_helper("trim", Box::new(TrimHelper));
    handlebars.register_helper("xml_escape", Box::new(XmlEscapeHelper));
    handlebars
}

fn prompt_template_skills(
    skills: &std::collections::HashMap<tau_proto::SkillName, DiscoveredSkill>,
) -> Vec<serde_json::Value> {
    let mut skills: Vec<_> = skills
        .iter()
        .filter(|(_, skill)| skill.add_to_prompt && !skill.disable_model_invocation)
        .map(|(name, skill)| {
            let base_dir = match &skill.source {
                crate::discovery::DiscoveredSkillSource::File(path) => path
                    .parent()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| path.display().to_string()),
                crate::discovery::DiscoveredSkillSource::BuiltIn { .. } => "<builtin>".to_owned(),
            };
            serde_json::json!({
                "name": name.as_str(),
                "description": tau_skills::truncate_description(&skill.description),
                "baseDir": base_dir,
            })
        })
        .collect();
    skills.sort_by(|a, b| compare_template_values(a, b, Some("name")));
    skills
}

struct EqHelper;

impl handlebars::HelperDef for EqHelper {
    fn call_inner<'reg: 'rc, 'rc>(
        &self,
        h: &handlebars::Helper<'rc>,
        _: &'reg handlebars::Handlebars<'reg>,
        _: &'rc handlebars::Context,
        _: &mut handlebars::RenderContext<'reg, 'rc>,
    ) -> Result<handlebars::ScopedJson<'rc>, handlebars::RenderError> {
        use handlebars::JsonRender;

        let Some(left) = h.param(0) else {
            return Ok(handlebars::ScopedJson::Derived(serde_json::Value::Bool(
                false,
            )));
        };
        let Some(right) = h.param(1) else {
            return Ok(handlebars::ScopedJson::Derived(serde_json::Value::Bool(
                false,
            )));
        };
        let equal = if left.value().is_string() || right.value().is_string() {
            left.value().render() == right.value().render()
        } else {
            left.value() == right.value()
        };
        Ok(handlebars::ScopedJson::Derived(serde_json::Value::Bool(
            equal,
        )))
    }
}

struct StartsWithHelper;

impl handlebars::HelperDef for StartsWithHelper {
    fn call_inner<'reg: 'rc, 'rc>(
        &self,
        h: &handlebars::Helper<'rc>,
        _: &'reg handlebars::Handlebars<'reg>,
        _: &'rc handlebars::Context,
        _: &mut handlebars::RenderContext<'reg, 'rc>,
    ) -> Result<handlebars::ScopedJson<'rc>, handlebars::RenderError> {
        use handlebars::JsonRender;

        let Some(value) = h.param(0) else {
            return Ok(handlebars::ScopedJson::Derived(serde_json::Value::Bool(
                false,
            )));
        };
        let Some(prefix) = h.param(1) else {
            return Ok(handlebars::ScopedJson::Derived(serde_json::Value::Bool(
                false,
            )));
        };
        Ok(handlebars::ScopedJson::Derived(serde_json::Value::Bool(
            value.value().render().starts_with(&prefix.value().render()),
        )))
    }
}

struct TrimHelper;

impl handlebars::HelperDef for TrimHelper {
    fn call_inner<'reg: 'rc, 'rc>(
        &self,
        h: &handlebars::Helper<'rc>,
        _: &'reg handlebars::Handlebars<'reg>,
        _: &'rc handlebars::Context,
        _: &mut handlebars::RenderContext<'reg, 'rc>,
    ) -> Result<handlebars::ScopedJson<'rc>, handlebars::RenderError> {
        use handlebars::JsonRender;

        let Some(value) = h.param(0) else {
            return Ok(handlebars::ScopedJson::Derived(serde_json::Value::String(
                String::new(),
            )));
        };
        Ok(handlebars::ScopedJson::Derived(serde_json::Value::String(
            value.value().render().trim().to_owned(),
        )))
    }
}

struct XmlEscapeHelper;

impl handlebars::HelperDef for XmlEscapeHelper {
    fn call_inner<'reg: 'rc, 'rc>(
        &self,
        h: &handlebars::Helper<'rc>,
        _: &'reg handlebars::Handlebars<'reg>,
        _: &'rc handlebars::Context,
        _: &mut handlebars::RenderContext<'reg, 'rc>,
    ) -> Result<handlebars::ScopedJson<'rc>, handlebars::RenderError> {
        use handlebars::JsonRender;

        let Some(value) = h.param(0) else {
            return Ok(handlebars::ScopedJson::Derived(serde_json::Value::String(
                String::new(),
            )));
        };
        Ok(handlebars::ScopedJson::Derived(serde_json::Value::String(
            xml_escape(&value.value().render()),
        )))
    }
}

fn xml_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

/// Build model-visible text for a genuine watched-agent turn transition.
///
/// Initial watch snapshots are durable state for clients, not new activity the
/// watching model needs to act on, so they deliberately produce no prompt.
pub(crate) fn watch_turn_transition_text(
    sender_label: &str,
    state: &tau_proto::AgentWatchTurnStateNotification,
) -> Option<String> {
    if state.initial {
        return None;
    }
    let transition = match state.state {
        tau_proto::AgentRuntimeState::Running => "started a model turn",
        tau_proto::AgentRuntimeState::Idle => "stopped its model turn",
    };
    Some(format!(
        "[tau-internal]: Watched agent {sender_label} {transition}"
    ))
}

struct SortHelper;

impl handlebars::HelperDef for SortHelper {
    fn call_inner<'reg: 'rc, 'rc>(
        &self,
        h: &handlebars::Helper<'rc>,
        _: &'reg handlebars::Handlebars<'reg>,
        _: &'rc handlebars::Context,
        _: &mut handlebars::RenderContext<'reg, 'rc>,
    ) -> Result<handlebars::ScopedJson<'rc>, handlebars::RenderError> {
        let Some(values) = h.param(0).and_then(|param| param.value().as_array()) else {
            return Ok(handlebars::ScopedJson::Derived(serde_json::Value::Array(
                Vec::new(),
            )));
        };
        let key = h.hash_get("by").and_then(|param| param.value().as_str());
        let mut sorted = values.clone();
        sorted.sort_by(|a, b| compare_template_values(a, b, key));
        Ok(handlebars::ScopedJson::Derived(serde_json::Value::Array(
            sorted,
        )))
    }
}

fn compare_template_values(
    a: &serde_json::Value,
    b: &serde_json::Value,
    key: Option<&str>,
) -> std::cmp::Ordering {
    let a = key.and_then(|key| a.get(key)).unwrap_or(a);
    let b = key.and_then(|key| b.get(key)).unwrap_or(b);
    match (a, b) {
        (serde_json::Value::Number(a), serde_json::Value::Number(b)) => a
            .as_f64()
            .partial_cmp(&b.as_f64())
            .unwrap_or(std::cmp::Ordering::Equal),
        (serde_json::Value::String(a), serde_json::Value::String(b)) => a.cmp(b),
        (serde_json::Value::Bool(a), serde_json::Value::Bool(b)) => a.cmp(b),
        _ => value_type_rank(a)
            .cmp(&value_type_rank(b))
            .then_with(|| a.to_string().cmp(&b.to_string())),
    }
}

fn value_type_rank(value: &serde_json::Value) -> u8 {
    match value {
        serde_json::Value::Null => 0,
        serde_json::Value::Bool(_) => 1,
        serde_json::Value::Number(_) => 2,
        serde_json::Value::String(_) => 3,
        serde_json::Value::Array(_) => 4,
        serde_json::Value::Object(_) => 5,
    }
}

pub(crate) fn render_agents_context_message<'a>(
    files: impl IntoIterator<Item = &'a DiscoveredAgentsFile>,
) -> String {
    let mut text = String::from(
        "# AGENTS.md instructions\n\n\
The following instructions were loaded from AGENTS.md files.\n\
More specific files usually override broader ones.\n\n",
    );

    for file in files {
        text.push_str(&format!(
            "<AGENTS_FILE path=\"{}\">\n",
            file.file_path.display()
        ));
        text.push_str(&file.content);
        if !file.content.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("</AGENTS_FILE>\n\n");
    }

    text
}

pub(crate) fn render_effective_prompt_message(
    system_prompt: &str,
    agents_context: Option<&str>,
) -> String {
    let mut text = String::new();
    text.push_str("<message role=\"system\">\n");
    text.push_str(system_prompt);
    if !system_prompt.ends_with('\n') {
        text.push('\n');
    }
    text.push_str("</message>\n");

    if let Some(agents_context) = agents_context {
        text.push_str("\n<message role=\"user\" synthetic=\"true\" source=\"AGENTS.md\">\n");
        text.push_str(agents_context);
        if !agents_context.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("</message>\n");
    }

    text
}

/// Returns the current date as YYYY-MM-DD without chrono.
pub(crate) fn chrono_free_date() -> String {
    // Use UNIX timestamp to derive date.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86400;
    // Simple days-since-epoch to Y-M-D (good enough, no leap second edge cases).
    let mut y = 1970_i64;
    let mut remaining = days as i64;
    loop {
        let days_in_year = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0;
    for md in &month_days {
        if remaining < *md {
            break;
        }
        remaining -= md;
        m += 1;
    }
    format!("{y}-{:02}-{:02}", m + 1, remaining + 1)
}

/// Converts the branch ending at `head` into LLM prompt context
/// items. Each conversation tracks its own head; with multiple
/// side agents interleaving tree mutations (one delegate's
/// teardown snapping `tree.head` to the default conv, another
/// delegate's tool result arriving moments later), `tree.head()` is
/// not reliable as the prompt-assembly cursor — use the conv's own
/// head instead.
pub(crate) struct AssembledPromptContext {
    /// Provider context with prompt-local presentation facts applied.
    pub(crate) context: tau_proto::PromptContext,
}

/// Assembles provider context while projecting currently live reply tools.
///
/// `live_send_tools` supplies model-visible aliases for routes authorized by
/// the target agent's effective tool snapshot. The durable envelope and its
/// source-owned reply path remain unchanged.
pub(crate) fn assemble_prompt_context_from(
    tree: &tau_core::AgentTree,
    head: Option<tau_core::NodeId>,
    live_send_tools: &std::collections::HashMap<tau_proto::MessageId, tau_proto::ToolName>,
) -> AssembledPromptContext {
    let mut blocks: Vec<tau_proto::ContextBlock> = Vec::new();

    for entry in tree.branch_from(head) {
        match entry {
            AgentEntry::UserInput { items } => {
                blocks.push(tau_proto::ContextBlock::UserInput(
                    tau_proto::UserInputBlock {
                        items: items.clone(),
                    },
                ));
            }
            AgentEntry::AssistantResponse {
                provider_response_id,
                backend,
                output_items,
                usage,
            } => {
                blocks.push(tau_proto::ContextBlock::AssistantResponse(
                    tau_proto::AssistantResponseBlock {
                        provider_response_id: provider_response_id.clone(),
                        backend: backend.clone(),
                        output_items: output_items.clone(),
                        usage: usage.clone(),
                    },
                ));
            }
            AgentEntry::ToolResults { items } => {
                blocks.push(tau_proto::ContextBlock::ToolResults(
                    tau_proto::ToolResultsBlock {
                        items: items.clone(),
                    },
                ));
            }
            AgentEntry::AgentMessage {
                direction,
                sender_id,
                sender_session_id,
                kind,
                watch_turn_state,
                message,
                ..
            } => match kind {
                tau_proto::AgentMessageKind::Message => {
                    let message_text = match (direction, sender_session_id) {
                        (tau_core::AgentMessageDirection::Inbound, Some(sender_session_id)) => {
                            format!(
                                "[tau-internal]: Message from {sender_session_id}/{sender_id}\n\n{}",
                                message
                            )
                        }
                        _ => message.clone(),
                    };
                    blocks.push(tau_proto::ContextBlock::UserInput(
                        tau_proto::UserInputBlock {
                            items: vec![ContextItem::Message(tau_proto::MessageItem {
                                role: match direction {
                                    tau_core::AgentMessageDirection::Outbound => {
                                        tau_proto::ContextRole::Assistant
                                    }
                                    tau_core::AgentMessageDirection::Inbound => {
                                        tau_proto::ContextRole::User
                                    }
                                },
                                content: vec![tau_proto::ContentPart::Text { text: message_text }],
                                phase: None,
                                responses_raw_json: None,
                            })],
                        },
                    ));
                }
                tau_proto::AgentMessageKind::WatchResponse => {
                    if *direction == tau_core::AgentMessageDirection::Inbound {
                        let sender_label = sender_session_id
                            .as_ref()
                            .map(|session_id| format!("{session_id}/{sender_id}"))
                            .unwrap_or_else(|| sender_id.to_string());
                        blocks.push(tau_proto::ContextBlock::UserInput(
                            tau_proto::UserInputBlock {
                                items: vec![ContextItem::Message(tau_proto::MessageItem {
                                    role: tau_proto::ContextRole::User,
                                    content: vec![tau_proto::ContentPart::Text {
                                        text: format!(
                                            "[tau-internal]: Watched agent {sender_label} emitted a response\n\n<response>\n{}\n</response>",
                                            xml_escape(message)
                                        ),
                                    }],
                                    phase: None,
                                    responses_raw_json: None,
                                })],
                            },
                        ));
                    }
                }
                tau_proto::AgentMessageKind::WatchPrompt => {
                    if *direction == tau_core::AgentMessageDirection::Inbound {
                        let sender_label = sender_session_id
                            .as_ref()
                            .map(|session_id| format!("{session_id}/{sender_id}"))
                            .unwrap_or_else(|| sender_id.to_string());
                        blocks.push(tau_proto::ContextBlock::UserInput(
                            tau_proto::UserInputBlock {
                                items: vec![ContextItem::Message(tau_proto::MessageItem {
                                    role: tau_proto::ContextRole::User,
                                    content: vec![tau_proto::ContentPart::Text {
                                        text: format!(
                                            "[tau-internal]: Watched agent {sender_label} received a user prompt\n\n<prompt>\n{}\n</prompt>",
                                            xml_escape(message)
                                        ),
                                    }],
                                    phase: None,
                                    responses_raw_json: None,
                                })],
                            },
                        ));
                    }
                }
                tau_proto::AgentMessageKind::WatchTurnState => {
                    if let (tau_core::AgentMessageDirection::Inbound, Some(text)) = (
                        direction,
                        watch_turn_state.as_ref().and_then(|state| {
                            let sender_label = sender_session_id
                                .as_ref()
                                .map(|session_id| format!("{session_id}/{sender_id}"))
                                .unwrap_or_else(|| sender_id.to_string());
                            watch_turn_transition_text(&sender_label, state)
                        }),
                    ) {
                        blocks.push(tau_proto::ContextBlock::UserInput(
                            tau_proto::UserInputBlock {
                                items: vec![ContextItem::Message(tau_proto::MessageItem {
                                    role: tau_proto::ContextRole::User,
                                    content: vec![tau_proto::ContentPart::Text { text }],
                                    phase: None,
                                    responses_raw_json: None,
                                })],
                            },
                        ));
                    }
                }
            },
            AgentEntry::MessageEnvelope { item } => {
                let mut item = item.clone();
                item.model_presentation.live_send_tool =
                    live_send_tools.get(&item.envelope.message_id).cloned();
                blocks.push(tau_proto::ContextBlock::UserInput(
                    tau_proto::UserInputBlock {
                        items: vec![ContextItem::MessageEnvelope(item)],
                    },
                ));
            }
        }
    }

    AssembledPromptContext {
        context: tau_proto::PromptContext { blocks },
    }
}

/// Converts a CBOR value to human-readable text for tool results.
#[cfg(test)]
pub(crate) fn cbor_to_text(v: &tau_proto::CborValue) -> String {
    use tau_proto::CborValue;
    match v {
        CborValue::Null => String::new(),
        CborValue::Bool(b) => b.to_string(),
        CborValue::Integer(i) => {
            let n: i128 = (*i).into();
            n.to_string()
        }
        CborValue::Float(f) => f.to_string(),
        CborValue::Text(s) => s.clone(),
        CborValue::Bytes(b) => format!("<{} bytes>", b.len()),
        CborValue::Array(arr) => arr.iter().map(cbor_to_text).collect::<Vec<_>>().join("\n"),
        CborValue::Map(entries) => {
            // For maps, extract text values cleanly.
            let mut parts = Vec::new();
            for (k, val) in entries {
                let key = match k {
                    CborValue::Text(s) => s.clone(),
                    other => cbor_to_text(other),
                };
                let value = cbor_to_text(val);
                if key == "output" || key == "line-numbered content" {
                    parts.push(value);
                } else if value.contains('\n') {
                    parts.push(format!("{key}:\n{value}"));
                } else {
                    parts.push(format!("{key}: {value}"));
                }
            }
            parts.join("\n")
        }
        CborValue::Tag(_, inner) => cbor_to_text(inner),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests;
