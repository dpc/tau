//! Building blocks for the per-turn prompt: the system prompt body, the
//! AGENTS.md context message, and the conversation assembly that turns a
//! [`tau_core::AgentTree`] into item-based prompt context.

use std::{cmp as path_std_cmp, collections as path_std_collections, time as path_std_time};

use tau_core::AgentEntry;
use tau_proto::{ContextItem, PromptFragment, ToolName};

use crate::discovery as path_crate_discovery;
use crate::discovery::{DiscoveredAgentsFile, DiscoveredSkill};
pub(crate) const BUILT_IN_SYSTEM_TEMPLATE_NAME: &str = "built-in";
const BUILT_IN_SYSTEM_PROMPT_TEMPLATE: &str = include_str!("../prompts/system.hbs");
const BIG_SYSTEM_TEMPLATE_NAME: &str = "big";
const BIG_SYSTEM_PROMPT_TEMPLATE: &str = include_str!("../prompts/big.hbs");
const USER_CLOSE: &str = "</user>";
const USER_CLOSE_VISIBLE: &str = "&lt;/user&gt;";
const MESSAGE_CLOSE: &str = "</message>";
const MESSAGE_CLOSE_VISIBLE: &str = "&lt;/message&gt;";
const PEER_MESSAGE_CLOSE: &str = "</tau_peer_message>";
const PEER_MESSAGE_CLOSE_VISIBLE: &str = "&lt;/tau_peer_message&gt;";
const WATCH_RESPONSE_CLOSE: &str = "</response>";
const WATCH_RESPONSE_CLOSE_VISIBLE: &str = "&lt;/response&gt;";
const WATCH_PROMPT_CLOSE: &str = "</prompt>";
const WATCH_PROMPT_CLOSE_VISIBLE: &str = "&lt;/prompt&gt;";

pub(crate) fn built_in_system_prompt_templates() -> std::collections::HashMap<String, String> {
    path_std_collections::HashMap::from([
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
///
/// Dynamic system-prompt values remain template inputs as required by
/// `GATE-tau-harness-system-prompt-templates`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RolePromptTemplateContext<'a> {
    /// Name of the role whose prompt is being rendered.
    pub(crate) role_name: &'a str,
    /// Stable configured role-group name containing the rendered role.
    pub(crate) role_group: &'a str,
    /// Durable agent id whose prompt is being rendered, when the render targets
    /// a concrete agent instead of a role-only preview.
    pub(crate) agent_id: Option<&'a tau_proto::AgentId>,
    /// Conditional provenance rule for selected exact-sentinel context.
    pub(crate) exact_sentinel_boundary_rule: Option<&'a str>,
}

/// Harness-owned capabilities visible to one prompt render.
///
/// Source-of-truth and render-failure semantics are governed by
/// `SPEC-tau-harness-prompt-dispatch`.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub(crate) struct PromptCapabilities {
    /// Model-visible tools authorized for this turn.
    pub(crate) tools: PromptToolCapabilities,
    /// Configured and currently ready extensions.
    pub(crate) extensions: PromptExtensionCapabilities,
}

/// Tool capability context for one prompt render.
#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct PromptToolCapabilities {
    /// Sorted, deduplicated model-visible tool names.
    pub(crate) available: Vec<String>,
    /// Whether the selected effective provider route supports parallel calls.
    pub(crate) parallel_calls: bool,
}

impl Default for PromptToolCapabilities {
    fn default() -> Self {
        Self {
            available: Vec::new(),
            parallel_calls: true,
        }
    }
}

/// Extension capability context for one prompt render.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub(crate) struct PromptExtensionCapabilities {
    /// Sorted, deduplicated names enabled by final startup configuration.
    pub(crate) enabled: Vec<String>,
    /// Sorted, deduplicated names whose runtime is currently ready.
    pub(crate) active: Vec<String>,
}

impl PromptCapabilities {
    /// Build deterministic capability data for a turn.
    pub(crate) fn new(
        available_tools: impl IntoIterator<Item = String>,
        enabled_extensions: impl IntoIterator<Item = String>,
        active_extensions: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            tools: PromptToolCapabilities {
                available: sorted_unique(available_tools),
                parallel_calls: true,
            },
            extensions: PromptExtensionCapabilities {
                enabled: sorted_unique(enabled_extensions),
                active: sorted_unique(active_extensions),
            },
        }
    }

    /// Set the effective provider route's parallel-tool-call capability.
    pub(crate) fn with_parallel_tool_calls(mut self, supported: bool) -> Self {
        self.tools.parallel_calls = supported;
        self
    }
}

fn sorted_unique(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut values = values.into_iter().collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values
}

impl<'a> RolePromptTemplateContext<'a> {
    /// Build template context for a role-only render.
    pub(crate) fn for_role(role_name: &'a str) -> Self {
        Self {
            role_name,
            role_group: role_name,
            agent_id: None,
            exact_sentinel_boundary_rule: None,
        }
    }

    /// Build template context for a concrete agent prompt render.
    pub(crate) fn for_agent(role_name: &'a str, agent_id: &'a tau_proto::AgentId) -> Self {
        Self {
            role_name,
            role_group: role_name,
            agent_id: Some(agent_id),
            exact_sentinel_boundary_rule: None,
        }
    }

    /// Supply the configured group containing this role.
    pub(crate) fn with_role_group(mut self, role_group: &'a str) -> Self {
        self.role_group = role_group;
        self
    }

    /// Supply the explicit conditional exact-sentinel provenance input.
    pub(crate) fn with_exact_sentinel_boundary_rule(mut self, rule: Option<&'a str>) -> Self {
        self.exact_sentinel_boundary_rule = rule;
        self
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
        PromptCapabilities::default(),
    )
}

/// Builds the system prompt with ordinary prompt fragments and tool-scoped
/// prompt fragments rendered into separate template sections.
#[cfg(test)]
pub(crate) fn build_system_prompt_with_tool_template_context(
    system_template: &str,
    skills: &std::collections::HashMap<tau_proto::SkillName, DiscoveredSkill>,
    prompt_fragments: &[PromptFragment],
    tool_prompt_fragments: &[ToolPromptFragment],
    agent_context: serde_json::Value,
    template_context: RolePromptTemplateContext<'_>,
    capabilities: PromptCapabilities,
) -> String {
    try_build_system_prompt_with_tool_template_context(
        system_template,
        skills,
        prompt_fragments,
        tool_prompt_fragments,
        agent_context,
        template_context,
        capabilities,
    )
    .expect("test prompt template should render")
}

/// Render a complete system prompt, returning any template error to the caller.
pub(crate) fn try_build_system_prompt_with_tool_template_context(
    system_template: &str,
    skills: &std::collections::HashMap<tau_proto::SkillName, DiscoveredSkill>,
    prompt_fragments: &[PromptFragment],
    tool_prompt_fragments: &[ToolPromptFragment],
    agent_context: serde_json::Value,
    template_context: RolePromptTemplateContext<'_>,
    capabilities: PromptCapabilities,
) -> Result<String, handlebars::RenderError> {
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
        capabilities,
    )
}

fn render_system_prompt_template(
    system_template: &str,
    context: RolePromptTemplateContext<'_>,
    skills: &std::collections::HashMap<tau_proto::SkillName, DiscoveredSkill>,
    prompt_fragments: &[PromptFragment],
    tool_prompt_fragments: &[ToolPromptFragment],
    agent_context: serde_json::Value,
    capabilities: PromptCapabilities,
) -> Result<String, handlebars::RenderError> {
    let data = system_prompt_template_data(
        context,
        skills,
        prompt_fragments,
        tool_prompt_fragments,
        agent_context,
        capabilities,
    )?;
    let handlebars = prompt_template_renderer();
    handlebars.render_template(system_template, &data)
}

fn prompt_template_data(
    context: RolePromptTemplateContext<'_>,
    skills: &std::collections::HashMap<tau_proto::SkillName, DiscoveredSkill>,
    mut agent_context: serde_json::Value,
    capabilities: PromptCapabilities,
) -> serde_json::Value {
    if let Some(object) = agent_context.as_object_mut() {
        // The built-in shell fragment intentionally renders before discovery
        // has published cwd context. Preserve that documented optional context
        // as an empty list while keeping all other missing paths strict.
        object
            .entry("cwd")
            .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    }
    serde_json::json!({
        "role": {
            "name": context.role_name,
            "group": context.role_group,
        },
        "agent_id": context.agent_id.map(ToString::to_string),
        "skills": prompt_template_skills(skills),
        "agent_context": agent_context,
        "capabilities": capabilities,
    })
}

fn system_prompt_template_data(
    context: RolePromptTemplateContext<'_>,
    skills: &std::collections::HashMap<tau_proto::SkillName, DiscoveredSkill>,
    prompt_fragments: &[PromptFragment],
    tool_prompt_fragments: &[ToolPromptFragment],
    agent_context: serde_json::Value,
    capabilities: PromptCapabilities,
) -> Result<serde_json::Value, handlebars::RenderError> {
    let exact_sentinel_boundary_rule = context.exact_sentinel_boundary_rule;
    let mut data = prompt_template_data(context, skills, agent_context, capabilities);
    let rendered_fragments = rendered_prompt_fragment_template_parts(prompt_fragments, &data)?;
    let rendered_tool_fragments =
        rendered_tool_prompt_fragment_template_parts(tool_prompt_fragments, &data)?;
    let object = data
        .as_object_mut()
        .expect("system prompt template data is an object");
    object.insert("prompt_fragments".to_owned(), rendered_fragments);
    object.insert("tool_prompt_fragments".to_owned(), rendered_tool_fragments);
    object.insert(
        "exact_sentinel_boundary_rule".to_owned(),
        serde_json::to_value(exact_sentinel_boundary_rule)
            .expect("optional exact-sentinel boundary rule serializes"),
    );
    Ok(data)
}

fn rendered_prompt_fragment_template_parts(
    fragments: &[PromptFragment],
    data: &serde_json::Value,
) -> Result<serde_json::Value, handlebars::RenderError> {
    let handlebars = prompt_template_renderer();
    Ok(serde_json::Value::Array(
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
                Ok(content) => content,
                Err(error) => return Some(Err(error)),
            };
            if content.trim().is_empty() {
                return None;
            }
            Some(Ok(serde_json::json!({
                "name": fragment.name,
                "priority": fragment.priority.get(),
                "content": content,
                "early": fragment.priority.get() < 100,
            })))
        })
        .collect::<Result<Vec<_>, _>>()?,
    ))
}

fn rendered_tool_prompt_fragment_template_parts(
    fragments: &[ToolPromptFragment],
    data: &serde_json::Value,
) -> Result<serde_json::Value, handlebars::RenderError> {
    let handlebars = prompt_template_renderer();
    Ok(serde_json::Value::Array(
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
                Err(error) => return Some(Err(error)),
            };
            let rendered = rendered.trim();
            if rendered.is_empty() {
                return None;
            }
            Some(Ok(serde_json::json!({
                "name": fragment.name,
                "priority": fragment.priority.get(),
                "tool_name": item.tool_name,
                "content": rendered,
                "early": fragment.priority.get() < 100,
            })))
        })
        .collect::<Result<Vec<_>, _>>()?,
    ))
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
    handlebars.register_helper(
        "tool_available",
        Box::new(CapabilityMembershipHelper::tool()),
    );
    handlebars.register_helper(
        "extension_enabled",
        Box::new(CapabilityMembershipHelper::extension("enabled")),
    );
    handlebars.register_helper(
        "extension_active",
        Box::new(CapabilityMembershipHelper::extension("active")),
    );
    handlebars
}

struct CapabilityMembershipHelper {
    field: &'static str,
    tool_name: bool,
}

impl CapabilityMembershipHelper {
    const fn tool() -> Self {
        Self {
            field: "available",
            tool_name: true,
        }
    }

    const fn extension(field: &'static str) -> Self {
        Self {
            field,
            tool_name: false,
        }
    }
}

impl handlebars::HelperDef for CapabilityMembershipHelper {
    fn call_inner<'reg: 'rc, 'rc>(
        &self,
        h: &handlebars::Helper<'rc>,
        _: &'reg handlebars::Handlebars<'reg>,
        _: &'rc handlebars::Context,
        _: &mut handlebars::RenderContext<'reg, 'rc>,
    ) -> Result<handlebars::ScopedJson<'rc>, handlebars::RenderError> {
        if h.params().len() != 2 {
            return Err(handlebars::RenderErrorReason::Other(
                "capability helper requires exactly two arguments".to_owned(),
            )
            .into());
        }
        let capabilities = h.param(0).expect("arity checked").value();
        let name = h
            .param(1)
            .expect("arity checked")
            .value()
            .as_str()
            .ok_or_else(|| {
                handlebars::RenderError::from(handlebars::RenderErrorReason::Other(
                    "capability name must be a string".to_owned(),
                ))
            })?;
        if self.tool_name {
            ToolName::try_new(name).ok_or_else(|| {
                handlebars::RenderError::from(handlebars::RenderErrorReason::Other(
                    "invalid tool capability name".to_owned(),
                ))
            })?;
        } else {
            tau_config::settings::validate_extension_name(name).map_err(|error| {
                handlebars::RenderError::from(handlebars::RenderErrorReason::Other(format!(
                    "invalid extension capability name: {error}"
                )))
            })?;
        }
        let values = capabilities
            .as_object()
            .and_then(|object| object.get(self.field))
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                handlebars::RenderError::from(handlebars::RenderErrorReason::Other(format!(
                    "capability object must contain an array field `{}`",
                    self.field
                )))
            })?;
        if !values.iter().all(serde_json::Value::is_string) {
            return Err(handlebars::RenderErrorReason::Other(format!(
                "capability field `{}` must contain only strings",
                self.field
            ))
            .into());
        }
        Ok(handlebars::ScopedJson::Derived(serde_json::Value::Bool(
            values.iter().any(|value| value.as_str() == Some(name)),
        )))
    }
}

fn prompt_template_skills(
    skills: &std::collections::HashMap<tau_proto::SkillName, DiscoveredSkill>,
) -> Vec<serde_json::Value> {
    let mut skills: Vec<_> = skills
        .iter()
        .filter(|(_, skill)| skill.add_to_prompt && !skill.disable_model_invocation)
        .map(|(name, skill)| {
            let base_dir = match &skill.source {
                path_crate_discovery::DiscoveredSkillSource::File(path) => path
                    .parent()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| path.display().to_string()),
                path_crate_discovery::DiscoveredSkillSource::BuiltIn { .. } => {
                    "<builtin>".to_owned()
                }
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

/// Render a non-initial work-status transition while escaping the
/// model-authored title as untrusted visible metadata.
pub(crate) fn watch_work_status_text(
    sender_label: &str,
    status: &tau_proto::AgentWatchWorkStatusNotification,
) -> Option<String> {
    if status.initial {
        return None;
    }
    let title = status
        .title
        .as_deref()
        .map(tau_proto::visible_escape_metadata)
        .unwrap_or_default();
    let state = match status.phase {
        tau_proto::AgentWorkStatusPhase::Unreported => return None,
        tau_proto::AgentWorkStatusPhase::Working => "working",
        tau_proto::AgentWorkStatusPhase::Done => "done",
        tau_proto::AgentWorkStatusPhase::Blocked => "blocked",
        tau_proto::AgentWorkStatusPhase::Unknown => "unknown",
    };
    Some(format!(
        "[tau-internal]: Watched agent {sender_label} status: {state} on {title}"
    ))
}

/// Render only closed structured provider categories; no provider-authored text
/// crosses the watch boundary.
pub(crate) fn watch_provider_status_text(
    sender_label: &str,
    status: &tau_proto::AgentWatchProviderStatusNotification,
) -> String {
    match status.state {
        tau_proto::AgentWatchProviderState::Retrying {
            category,
            attempt,
            next_retry_delay_secs,
        } => {
            let delay =
                tau_proto::format_approximate_duration_secs(u64::from(next_retry_delay_secs));
            format!(
                "[tau-internal]: Watched agent {sender_label} provider status: retrying ({category}, attempt {attempt}, next retry about {delay})",
                category = category.as_str(),
            )
        }
        tau_proto::AgentWatchProviderState::RecoveringContext { .. } => format!(
            "[tau-internal]: Watched agent {sender_label} provider status: recovering_context (context_window)"
        ),
        tau_proto::AgentWatchProviderState::Blocked { category } => format!(
            "[tau-internal]: Watched agent {sender_label} provider status: blocked ({})",
            category.as_str()
        ),
        tau_proto::AgentWatchProviderState::DispatchUncertain { category } => format!(
            "[tau-internal]: Watched agent {sender_label} provider status: dispatch_uncertain ({})",
            category.as_str()
        ),
        tau_proto::AgentWatchProviderState::TerminalError { failure_kind, .. } => format!(
            "[tau-internal]: Watched agent {sender_label} provider status: terminal error ({})",
            failure_kind.as_str()
        ),
    }
}

/// Render a concise provider snapshot for an `agent_watch` tool result.
pub(crate) fn watch_provider_status_summary(state: &tau_proto::AgentWatchProviderState) -> String {
    match state {
        tau_proto::AgentWatchProviderState::Retrying {
            category,
            attempt,
            next_retry_delay_secs,
        } => {
            let delay =
                tau_proto::format_approximate_duration_secs(u64::from(*next_retry_delay_secs));
            format!(
                "retrying ({}, attempt {attempt}, next retry about {delay})",
                category.as_str()
            )
        }
        tau_proto::AgentWatchProviderState::RecoveringContext { .. } => {
            "recovering context".to_owned()
        }
        tau_proto::AgentWatchProviderState::Blocked { category } => {
            format!("blocked ({})", category.as_str())
        }
        tau_proto::AgentWatchProviderState::DispatchUncertain { category } => {
            format!("dispatch uncertain ({})", category.as_str())
        }
        tau_proto::AgentWatchProviderState::TerminalError { failure_kind, .. } => {
            format!("terminal error ({})", failure_kind.as_str())
        }
    }
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
            .unwrap_or(path_std_cmp::Ordering::Equal),
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
    use std::fmt::Write as _;

    let mut files = files.into_iter().peekable();
    let mut text = String::from(
        "# AGENTS.md instructions\n\n\
The following instructions were loaded from AGENTS.md files.\n\
More specific files usually override broader ones.\n\n",
    );

    if files.peek().is_some() {
        text.push_str("# agents.md files\n\n");
    }

    for file in files {
        let _ = writeln!(
            &mut text,
            "<AGENTS_FILE path=\"{}\">",
            file.file_path.display()
        );
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
    let secs = path_std_time::SystemTime::now()
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
    /// Whether selected context contains a Tau-stamped payload envelope.
    pub(crate) contains_exact_sentinel_envelope: bool,
}

fn is_exact_sentinel_projection(text: &str) -> bool {
    [
        ("<user>", USER_CLOSE),
        ("<message", MESSAGE_CLOSE),
        ("<tau_peer_message", PEER_MESSAGE_CLOSE),
        ("<response>", WATCH_RESPONSE_CLOSE),
        ("<prompt>", WATCH_PROMPT_CLOSE),
        ("<tau_web_content", "</tau_web_content>"),
    ]
    .into_iter()
    .any(|(open, close)| {
        text.ends_with(close)
            && (text.starts_with(open)
                || (text.starts_with("[tau-internal]:") && text.contains(open)))
    })
}

fn context_items_contain_exact_sentinel(items: &[ContextItem]) -> bool {
    items.iter().any(|item| match item {
        ContextItem::Message(message) => message.content.iter().any(|part| {
            let tau_proto::ContentPart::Text { text } = part;
            is_exact_sentinel_projection(text)
        }),
        ContextItem::ToolResult(result) => is_exact_sentinel_projection(&result.output.body),
        _ => false,
    })
}

fn tool_results_contain_exact_sentinel(items: &[tau_proto::ToolResultItem]) -> bool {
    items
        .iter()
        .any(|item| is_exact_sentinel_projection(&item.output.body))
}

/// Assembles provider context from the selected transcript branch.
pub(crate) fn assemble_prompt_context_from(
    tree: &tau_core::AgentTree,
    head: Option<tau_core::NodeId>,
) -> AssembledPromptContext {
    let mut blocks: Vec<tau_proto::ContextBlock> = Vec::new();
    let mut contains_exact_sentinel_envelope = false;
    let branch_ids = tree.branch_node_ids_from(head);
    let branch: Vec<_> = branch_ids
        .iter()
        .filter_map(|node_id| tree.node(*node_id).map(|node| &node.entry))
        .collect();
    let mut selected: Vec<&AgentEntry> = branch.clone();
    if let Some((boundary_index, replacement_window, cut)) =
        branch.iter().enumerate().rev().find_map(|(index, entry)| {
            let AgentEntry::Compaction {
                replacement_window,
                transaction_id: Some(_),
                cut: Some(cut),
                suffix_end: Some(_),
            } = entry
            else {
                return None;
            };
            Some((index, replacement_window, *cut))
        })
    {
        contains_exact_sentinel_envelope = context_items_contain_exact_sentinel(replacement_window);
        blocks.push(tau_proto::ContextBlock::UserInput(
            tau_proto::UserInputBlock {
                items: replacement_window.clone(),
            },
        ));
        let suffix_start = match cut {
            tau_proto::AgentHead::Root => 0,
            tau_proto::AgentHead::Node(cut_node) => branch_ids
                .iter()
                .position(|node_id| *node_id == cut_node)
                .map_or(boundary_index, |index| index.saturating_add(1)),
        };
        selected = branch[suffix_start..boundary_index]
            .iter()
            .chain(branch[boundary_index.saturating_add(1)..].iter())
            .copied()
            .collect();
    }

    for entry in selected {
        match entry {
            AgentEntry::Compaction {
                replacement_window, ..
            } => {
                blocks.clear();
                contains_exact_sentinel_envelope =
                    context_items_contain_exact_sentinel(replacement_window);
                blocks.push(tau_proto::ContextBlock::UserInput(
                    tau_proto::UserInputBlock {
                        items: replacement_window.clone(),
                    },
                ));
            }
            AgentEntry::CompactionTrigger { .. } => {
                blocks.push(tau_proto::ContextBlock::UserInput(
                    tau_proto::UserInputBlock {
                        items: vec![ContextItem::CompactionTrigger],
                    },
                ));
            }
            AgentEntry::UserInput {
                items,
                submission_source,
                ..
            } => {
                contains_exact_sentinel_envelope |=
                    submission_source.as_ref() == Some(&tau_proto::PromptSubmissionSource::HumanUi);
                blocks.push(tau_proto::ContextBlock::UserInput(
                    tau_proto::UserInputBlock {
                        items: project_user_prompt_items(items, submission_source.as_ref()),
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
                contains_exact_sentinel_envelope |= tool_results_contain_exact_sentinel(items);
                blocks.push(tau_proto::ContextBlock::ToolResults(
                    tau_proto::ToolResultsBlock {
                        items: items.clone(),
                    },
                ));
            }
            AgentEntry::AgentMessage {
                durable_event_seq: _,
                message_id: _,
                direction,
                sender_id,
                sender_session_id,
                recipient: _,
                kind,
                watch_provider_status,
                watch_work_status,
                watch_long_wait,
                message,
            } => match kind {
                tau_proto::AgentMessageKind::Message => {
                    contains_exact_sentinel_envelope |=
                        *direction == tau_core::AgentMessageDirection::Inbound;
                    let message_text = match (direction, sender_session_id) {
                        (tau_core::AgentMessageDirection::Inbound, Some(sender_session_id)) => {
                            let body = tau_proto::escape_exact_sentinel_close(
                                message,
                                PEER_MESSAGE_CLOSE,
                                PEER_MESSAGE_CLOSE_VISIBLE,
                            );
                            format!(
                                "[tau-internal]: Authenticated peer message\n\n<tau_peer_message sender_session=\"{}\" sender_agent=\"{}\">\n{}\n</tau_peer_message>",
                                xml_escape(sender_session_id.as_str()),
                                xml_escape(sender_id.as_str()),
                                body
                            )
                        }
                        (tau_core::AgentMessageDirection::Inbound, None) => {
                            let body = tau_proto::escape_exact_sentinel_close(
                                message,
                                MESSAGE_CLOSE,
                                MESSAGE_CLOSE_VISIBLE,
                            );
                            format!(
                                "[tau-internal]: You have received a message from {sender_id}\n\n<message>\n{body}\n</message>"
                            )
                        }
                        (tau_core::AgentMessageDirection::Outbound, _) => message.clone(),
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
                        contains_exact_sentinel_envelope = true;
                        let sender_label = sender_session_id
                            .as_ref()
                            .map(|session_id| format!("{session_id}/{sender_id}"))
                            .unwrap_or_else(|| sender_id.to_string());
                        blocks.push(tau_proto::ContextBlock::UserInput(
                            tau_proto::UserInputBlock {
                                items: vec![ContextItem::Message(tau_proto::MessageItem {
                                    role: tau_proto::ContextRole::User,
                                    content: vec![tau_proto::ContentPart::Text {
                                        text: {
                                            let body =
                                                tau_proto::escape_exact_sentinel_close(
                                                    message,
                                                    WATCH_RESPONSE_CLOSE,
                                                    WATCH_RESPONSE_CLOSE_VISIBLE,
                                                );
                                            format!(
                                                "[tau-internal]: Watched agent {sender_label} emitted a response\n\n<response>\n{body}\n</response>"
                                            )
                                        },
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
                        contains_exact_sentinel_envelope = true;
                        let sender_label = sender_session_id
                            .as_ref()
                            .map(|session_id| format!("{session_id}/{sender_id}"))
                            .unwrap_or_else(|| sender_id.to_string());
                        blocks.push(tau_proto::ContextBlock::UserInput(
                            tau_proto::UserInputBlock {
                                items: vec![ContextItem::Message(tau_proto::MessageItem {
                                    role: tau_proto::ContextRole::User,
                                    content: vec![tau_proto::ContentPart::Text {
                                        text: {
                                            let body =
                                                tau_proto::escape_exact_sentinel_close(
                                                    message,
                                                    WATCH_PROMPT_CLOSE,
                                                    WATCH_PROMPT_CLOSE_VISIBLE,
                                                );
                                            format!(
                                                "[tau-internal]: Watched agent {sender_label} received a user prompt\n\n<prompt>\n{body}\n</prompt>"
                                            )
                                        },
                                    }],
                                    phase: None,
                                    responses_raw_json: None,
                                })],
                            },
                        ));
                    }
                }
                tau_proto::AgentMessageKind::WatchProviderStatus => {
                    if let (tau_core::AgentMessageDirection::Inbound, Some(status)) =
                        (direction, watch_provider_status.as_ref())
                        && !status.initial
                    {
                        let sender_label = sender_session_id
                            .as_ref()
                            .map(|session_id| format!("{session_id}/{sender_id}"))
                            .unwrap_or_else(|| sender_id.to_string());
                        blocks.push(tau_proto::ContextBlock::UserInput(
                            tau_proto::UserInputBlock {
                                items: vec![ContextItem::Message(tau_proto::MessageItem {
                                    role: tau_proto::ContextRole::User,
                                    content: vec![tau_proto::ContentPart::Text {
                                        text: watch_provider_status_text(&sender_label, status),
                                    }],
                                    phase: None,
                                    responses_raw_json: None,
                                })],
                            },
                        ));
                    }
                }
                tau_proto::AgentMessageKind::WatchWorkStatus => {
                    if let (tau_core::AgentMessageDirection::Inbound, Some(status)) =
                        (direction, watch_work_status.as_ref())
                        && let Some(text) = watch_work_status_text(sender_id.as_ref(), status)
                    {
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
                tau_proto::AgentMessageKind::WatchLongWait => {
                    if let (tau_core::AgentMessageDirection::Inbound, Some(wait)) =
                        (direction, watch_long_wait.as_ref())
                    {
                        blocks.push(tau_proto::ContextBlock::UserInput(
                            tau_proto::UserInputBlock {
                                items: vec![ContextItem::Message(tau_proto::MessageItem {
                                    role: tau_proto::ContextRole::User,
                                    content: vec![tau_proto::ContentPart::Text {
                                        text: format!(
                                            "[tau-internal]: Watched agent {sender_id} has spent over {} minutes waiting.",
                                            wait.threshold_minutes
                                        ),
                                    }],
                                    phase: None,
                                    responses_raw_json: None,
                                })],
                            },
                        ));
                    }
                }
            },
            AgentEntry::MessageFact { item, .. } => {
                contains_exact_sentinel_envelope = true;
                blocks.push(tau_proto::ContextBlock::UserInput(
                    tau_proto::UserInputBlock {
                        items: vec![ContextItem::Message(*item.clone())],
                    },
                ));
            }
        }
    }

    AssembledPromptContext {
        context: tau_proto::PromptContext { blocks },
        contains_exact_sentinel_envelope,
    }
}

/// Apply the provider-only envelope selected by typed prompt provenance.
///
/// Prompt folding guarantees that a sourced entry contains exactly one
/// user-role message with one text part, so this preserves the one-item
/// provider shape while changing only that part's presentation text.
fn project_user_prompt_items(
    items: &[ContextItem],
    submission_source: Option<&tau_proto::PromptSubmissionSource>,
) -> Vec<ContextItem> {
    let mut projected = items.to_vec();
    if submission_source != Some(&tau_proto::PromptSubmissionSource::HumanUi) {
        return projected;
    }
    for item in &mut projected {
        if let ContextItem::Message(message) = item {
            for part in &mut message.content {
                let tau_proto::ContentPart::Text { text } = part;
                let body =
                    tau_proto::escape_exact_sentinel_close(text, USER_CLOSE, USER_CLOSE_VISIBLE);
                *text = format!("<user>{body}</user>");
            }
        }
    }
    projected
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
