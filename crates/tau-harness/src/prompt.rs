//! Building blocks for the per-turn prompt: the system prompt body, the
//! AGENTS.md context message, and the conversation assembly that turns a
//! [`tau_core::AgentTree`] into item-based prompt context.

use std::collections::hash_map::DefaultHasher as PathStdDefaultHasher;
use std::{
    cell as path_std_cell, cmp as path_std_cmp, collections as path_std_collections,
    hash as path_std_hash, time as path_std_time,
};

use handlebars::Renderable as _;
use tau_core::AgentEntry;
use tau_proto::{ContextItem, PromptFragment, ToolName};

#[cfg(test)]
thread_local! {
    static PROMPT_CONTEXT_CONSTRUCTION_COUNT: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
    static PROMPT_PREFLIGHT_ENTRY_VISIT_COUNT: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
    static PROMPT_TEMPLATE_PARSE_COUNT: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
    static PROMPT_TEMPLATE_RENDER_COUNT: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
    static PROMPT_MEASUREMENT_JSON_SERIALIZATION_COUNT: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
    static PROMPT_MEASURED_BLOCK_CLONE_COUNT: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
    static PROMPT_MEASUREMENT_BLOCK_COUNT_SNAPSHOT_COUNT: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

/// Reset test-only counters for prompt construction and preflight work.
#[cfg(test)]
pub(crate) fn reset_prompt_preflight_test_counters() {
    PROMPT_CONTEXT_CONSTRUCTION_COUNT.set(0);
    PROMPT_PREFLIGHT_ENTRY_VISIT_COUNT.set(0);
}

/// Return the test-only complete prompt-context construction count.
#[cfg(test)]
pub(crate) fn prompt_context_construction_count() -> usize {
    PROMPT_CONTEXT_CONSTRUCTION_COUNT.get()
}

/// Return the test-only canonical-entry preflight visit count.
#[cfg(test)]
pub(crate) fn prompt_preflight_entry_visit_count() -> usize {
    PROMPT_PREFLIGHT_ENTRY_VISIT_COUNT.get()
}

/// Reset test-only counters for prompt-context measurement work.
#[cfg(test)]
pub(crate) fn reset_prompt_measurement_test_counters() {
    PROMPT_MEASUREMENT_JSON_SERIALIZATION_COUNT.set(0);
    PROMPT_MEASURED_BLOCK_CLONE_COUNT.set(0);
    PROMPT_MEASUREMENT_BLOCK_COUNT_SNAPSHOT_COUNT.set(0);
}

/// Return test-only JSON serialization, measured-block clone, and block-count
/// snapshot counts.
#[cfg(test)]
pub(crate) fn prompt_measurement_test_counts() -> (usize, usize, usize) {
    (
        PROMPT_MEASUREMENT_JSON_SERIALIZATION_COUNT.get(),
        PROMPT_MEASURED_BLOCK_CLONE_COUNT.get(),
        PROMPT_MEASUREMENT_BLOCK_COUNT_SNAPSHOT_COUNT.get(),
    )
}

/// Reset deterministic template parse and render work counters.
#[cfg(test)]
pub(crate) fn reset_prompt_template_test_counters() {
    PROMPT_TEMPLATE_PARSE_COUNT.set(0);
    PROMPT_TEMPLATE_RENDER_COUNT.set(0);
}

/// Return the number of immutable template sources parsed by the current test.
#[cfg(test)]
pub(crate) fn prompt_template_parse_count() -> usize {
    PROMPT_TEMPLATE_PARSE_COUNT.get()
}

/// Return the number of dynamic template renders performed by the current test.
#[cfg(test)]
pub(crate) fn prompt_template_render_count() -> usize {
    PROMPT_TEMPLATE_RENDER_COUNT.get()
}

use crate::discovery as path_crate_discovery;
use crate::discovery::{DiscoveredAgentsFile, DiscoveredSkill};
pub(crate) const BUILT_IN_SYSTEM_TEMPLATE_NAME: &str = "built-in";
const BUILT_IN_SYSTEM_PROMPT_TEMPLATE: &str = include_str!("../prompts/system.hbs");
const BIG_SYSTEM_TEMPLATE_NAME: &str = "big";
const BIG_SYSTEM_PROMPT_TEMPLATE: &str = include_str!("../prompts/big.hbs");
const MESSAGE_CLOSE: &str = "</message>";
const MESSAGE_CLOSE_VISIBLE: &str = "&lt;/message&gt;";
const PEER_MESSAGE_CLOSE: &str = "</tau_peer_message>";
const PEER_MESSAGE_CLOSE_VISIBLE: &str = "&lt;/tau_peer_message&gt;";
const WATCH_RESPONSE_CLOSE: &str = "</response>";
const WATCH_RESPONSE_CLOSE_VISIBLE: &str = "&lt;/response&gt;";
const WATCH_PROMPT_CLOSE: &str = "</prompt>";
const WATCH_PROMPT_CLOSE_VISIBLE: &str = "&lt;/prompt&gt;";

/// Reusable strict Handlebars registry and exact immutable-source parse cache.
///
/// The cache stores parsed templates only. Every invocation still renders
/// against current dynamic data, so agent context, secrets, capabilities, and
/// other per-dispatch values can never be reused as final output.
pub(crate) struct PromptTemplateEngine {
    /// Registry configured once with Tau's escaping policy and helper set.
    registry: handlebars::Handlebars<'static>,
    /// Parsed sources in collision-safe generation/content hash buckets.
    cache: path_std_cell::RefCell<PromptTemplateCache>,
}

/// Active exact source generation and its bounded parsed-template cache.
#[derive(Default)]
struct PromptTemplateCache {
    /// Monotonic process-local source generation.
    generation: u64,
    /// Exact ordered source snapshot owning this generation.
    sources: Vec<String>,
    /// Parsed sources in collision-safe generation/content hash buckets.
    templates: std::collections::HashMap<u64, Vec<CachedPromptTemplate>>,
}

/// One collision-safe immutable source cache entry.
struct CachedPromptTemplate {
    /// Generation that owns this source.
    generation: u64,
    /// Exact source bytes used to reject hash collisions.
    source: String,
    /// Parsed immutable Handlebars template.
    template: handlebars::Template,
}

impl Default for PromptTemplateEngine {
    fn default() -> Self {
        Self {
            registry: prompt_template_renderer(),
            cache: path_std_cell::RefCell::new(PromptTemplateCache::default()),
        }
    }
}

impl PromptTemplateEngine {
    /// Return the number of parsed templates retained for the active
    /// generation.
    #[cfg(test)]
    fn cached_template_count(&self) -> usize {
        self.cache.borrow().templates.values().map(Vec::len).sum()
    }

    /// Select the exact ordered source snapshot for one render generation.
    ///
    /// A changed system, ordinary-fragment, or tool-fragment source advances
    /// the generation and discards all parsed entries from the old snapshot.
    fn activate_source_snapshot(
        &self,
        system_template: &str,
        prompt_fragments: &[PromptFragment],
        tool_prompt_fragments: &[ToolPromptFragment],
    ) -> u64 {
        let sources = std::iter::once(system_template)
            .chain(
                prompt_fragments
                    .iter()
                    .map(|fragment| fragment.template.as_str()),
            )
            .chain(
                tool_prompt_fragments
                    .iter()
                    .map(|item| item.fragment.template.as_str()),
            );
        let mut cache = self.cache.borrow_mut();
        let unchanged = cache.sources.len()
            == 1 + prompt_fragments.len() + tool_prompt_fragments.len()
            && cache.sources.iter().map(String::as_str).eq(sources.clone());
        if !unchanged {
            cache.generation = cache.generation.wrapping_add(1);
            cache.sources = sources.map(str::to_owned).collect();
            cache.templates.clear();
        }
        cache.generation
    }

    /// Parse an immutable source at most once per exact generation/content key
    /// and render it against the supplied current data.
    fn render(
        &self,
        generation: u64,
        source: &str,
        data: &serde_json::Value,
    ) -> Result<String, handlebars::RenderError> {
        #[cfg(test)]
        PROMPT_TEMPLATE_RENDER_COUNT.set(PROMPT_TEMPLATE_RENDER_COUNT.get() + 1);

        let mut hasher = PathStdDefaultHasher::new();
        path_std_hash::Hash::hash(&generation, &mut hasher);
        path_std_hash::Hash::hash(source, &mut hasher);
        let key = path_std_hash::Hasher::finish(&hasher);
        let matches =
            |entry: &CachedPromptTemplate| entry.generation == generation && entry.source == source;
        if !self
            .cache
            .borrow()
            .templates
            .get(&key)
            .is_some_and(|bucket| bucket.iter().any(matches))
        {
            #[cfg(test)]
            PROMPT_TEMPLATE_PARSE_COUNT.set(PROMPT_TEMPLATE_PARSE_COUNT.get() + 1);
            let template =
                handlebars::Template::compile(source).map_err(handlebars::RenderError::from)?;
            self.cache
                .borrow_mut()
                .templates
                .entry(key)
                .or_default()
                .push(CachedPromptTemplate {
                    generation,
                    source: source.to_owned(),
                    template,
                });
        }
        let cache = self.cache.borrow();
        let template = &cache
            .templates
            .get(&key)
            .and_then(|bucket| bucket.iter().find(|entry| matches(entry)))
            .expect("parsed prompt template cache entry must exist")
            .template;
        let context = handlebars::Context::wraps(data)?;
        let mut render_context = handlebars::RenderContext::new(None);
        template.renders(&self.registry, &context, &mut render_context)
    }
}

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
    /// Canonical current directory captured when the harness process started.
    ///
    /// This session-wide path remains separate from per-agent shell workdir
    /// context, which extensions publish under `agent_context`.
    pub(crate) session_cwd: Option<&'a std::path::Path>,
    /// Optional model-visible provenance notice for selected payload envelopes.
    pub(crate) payload_envelope_provenance_notice: Option<&'a str>,
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
            session_cwd: None,
            payload_envelope_provenance_notice: None,
        }
    }

    /// Build template context for a concrete agent prompt render.
    pub(crate) fn for_agent(role_name: &'a str, agent_id: &'a tau_proto::AgentId) -> Self {
        Self {
            role_name,
            role_group: role_name,
            agent_id: Some(agent_id),
            session_cwd: None,
            payload_envelope_provenance_notice: None,
        }
    }

    /// Supply the configured group containing this role.
    pub(crate) fn with_role_group(mut self, role_group: &'a str) -> Self {
        self.role_group = role_group;
        self
    }

    /// Supply the canonical current directory captured for this harness
    /// session.
    pub(crate) fn with_session_cwd(mut self, session_cwd: &'a std::path::Path) -> Self {
        self.session_cwd = Some(session_cwd);
        self
    }

    /// Supply the optional model-visible payload-envelope provenance notice.
    pub(crate) fn with_payload_envelope_provenance_notice(
        mut self,
        notice: Option<&'a str>,
    ) -> Self {
        self.payload_envelope_provenance_notice = notice;
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
#[cfg(test)]
pub(crate) fn try_build_system_prompt_with_tool_template_context(
    system_template: &str,
    skills: &std::collections::HashMap<tau_proto::SkillName, DiscoveredSkill>,
    prompt_fragments: &[PromptFragment],
    tool_prompt_fragments: &[ToolPromptFragment],
    agent_context: serde_json::Value,
    template_context: RolePromptTemplateContext<'_>,
    capabilities: PromptCapabilities,
) -> Result<String, handlebars::RenderError> {
    let engine = PromptTemplateEngine::default();
    // This test-only convenience API accepts arbitrary fragment order. The
    // production harness passes its already source/name/priority-sorted slices
    // directly to `try_build_system_prompt_with_engine`.
    let mut prompt_fragments = prompt_fragments.to_vec();
    prompt_fragments.sort_by_key(|fragment| fragment.priority);
    let mut tool_prompt_fragments = tool_prompt_fragments.to_vec();
    tool_prompt_fragments.sort_by_key(|item| item.fragment.priority);
    try_build_system_prompt_with_engine(
        &engine,
        system_template,
        skills,
        &prompt_fragments,
        &tool_prompt_fragments,
        agent_context,
        template_context,
        capabilities,
    )
}

/// Render one system prompt with a reusable registry and immutable-source
/// cache.
#[allow(clippy::too_many_arguments)]
pub(crate) fn try_build_system_prompt_with_engine(
    engine: &PromptTemplateEngine,
    system_template: &str,
    skills: &std::collections::HashMap<tau_proto::SkillName, DiscoveredSkill>,
    prompt_fragments: &[PromptFragment],
    tool_prompt_fragments: &[ToolPromptFragment],
    agent_context: serde_json::Value,
    template_context: RolePromptTemplateContext<'_>,
    capabilities: PromptCapabilities,
) -> Result<String, handlebars::RenderError> {
    let template_generation =
        engine.activate_source_snapshot(system_template, prompt_fragments, tool_prompt_fragments);
    render_system_prompt_template(
        engine,
        template_generation,
        system_template,
        template_context,
        skills,
        prompt_fragments,
        tool_prompt_fragments,
        agent_context,
        capabilities,
    )
}

#[allow(clippy::too_many_arguments)]
fn render_system_prompt_template(
    engine: &PromptTemplateEngine,
    template_generation: u64,
    system_template: &str,
    context: RolePromptTemplateContext<'_>,
    skills: &std::collections::HashMap<tau_proto::SkillName, DiscoveredSkill>,
    prompt_fragments: &[PromptFragment],
    tool_prompt_fragments: &[ToolPromptFragment],
    agent_context: serde_json::Value,
    capabilities: PromptCapabilities,
) -> Result<String, handlebars::RenderError> {
    let data = system_prompt_template_data(
        engine,
        template_generation,
        context,
        skills,
        prompt_fragments,
        tool_prompt_fragments,
        agent_context,
        capabilities,
    )?;
    engine.render(template_generation, system_template, &data)
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
        "session": {
            "cwd": context.session_cwd.map(|path| path.display().to_string()),
        },
        "agent_id": context.agent_id.map(ToString::to_string),
        "skills": prompt_template_skills(skills),
        "agent_context": agent_context,
        "capabilities": capabilities,
    })
}

#[allow(clippy::too_many_arguments)]
fn system_prompt_template_data(
    engine: &PromptTemplateEngine,
    template_generation: u64,
    context: RolePromptTemplateContext<'_>,
    skills: &std::collections::HashMap<tau_proto::SkillName, DiscoveredSkill>,
    prompt_fragments: &[PromptFragment],
    tool_prompt_fragments: &[ToolPromptFragment],
    agent_context: serde_json::Value,
    capabilities: PromptCapabilities,
) -> Result<serde_json::Value, handlebars::RenderError> {
    let payload_envelope_provenance_notice = context.payload_envelope_provenance_notice;
    let mut data = prompt_template_data(context, skills, agent_context, capabilities);
    let rendered_fragments = rendered_prompt_fragment_template_parts(
        engine,
        template_generation,
        prompt_fragments,
        &data,
    )?;
    let rendered_tool_fragments = rendered_tool_prompt_fragment_template_parts(
        engine,
        template_generation,
        tool_prompt_fragments,
        &data,
    )?;
    let object = data
        .as_object_mut()
        .expect("system prompt template data is an object");
    object.insert("prompt_fragments".to_owned(), rendered_fragments);
    object.insert("tool_prompt_fragments".to_owned(), rendered_tool_fragments);
    object.insert(
        "payload_envelope_provenance_notice".to_owned(),
        serde_json::to_value(payload_envelope_provenance_notice)
            .expect("optional payload-envelope provenance notice serializes"),
    );
    Ok(data)
}

fn rendered_prompt_fragment_template_parts(
    engine: &PromptTemplateEngine,
    template_generation: u64,
    fragments: &[PromptFragment],
    data: &serde_json::Value,
) -> Result<serde_json::Value, handlebars::RenderError> {
    Ok(serde_json::Value::Array(
        fragments
            .iter()
            .filter_map(|fragment| {
                if fragment.template.is_empty() {
                    return None;
                }
                let content =
                    match engine.render(template_generation, fragment.template.as_str(), data) {
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
    engine: &PromptTemplateEngine,
    template_generation: u64,
    fragments: &[ToolPromptFragment],
    data: &serde_json::Value,
) -> Result<serde_json::Value, handlebars::RenderError> {
    Ok(serde_json::Value::Array(
        fragments
            .iter()
            .filter_map(|item| {
                let fragment = &item.fragment;
                if fragment.template.is_empty() {
                    return None;
                }
                let rendered =
                    match engine.render(template_generation, fragment.template.as_str(), data) {
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
    handlebars.register_helper("xml_escape_lax", Box::new(XmlEscapeLaxHelper));
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

/// Render template content with lax XML closing-tag escaping.
///
/// The helper replaces each literal `</` with `&lt;/` and preserves every other
/// byte. It deliberately does not parse XML: escaping every possible
/// closing-tag prefix prevents trusted wrapper confusion without escaping
/// ordinary text.
struct XmlEscapeLaxHelper;

impl handlebars::HelperDef for XmlEscapeLaxHelper {
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
            xml_escape_lax(&value.value().render()),
        )))
    }
}

/// Escape only possible XML closing-tag prefixes in model-visible text.
fn xml_escape_lax(text: &str) -> String {
    text.replace("</", "&lt;/")
}

/// Render template content with full XML escaping for attribute-safe custom
/// templates.
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
        tau_proto::AgentWorkStatusPhase::Waiting => "waiting",
        tau_proto::AgentWorkStatusPhase::Unknown => "unknown",
    };
    Some(crate::internal_envelope::frame(&format!(
        "Watched agent {sender_label} status: {state} on {title}"
    )))
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
            crate::internal_envelope::frame(&format!(
                "Watched agent {sender_label} provider status: retrying ({category}, attempt {attempt}, next retry about {delay})",
                category = category.as_str(),
            ))
        }
        tau_proto::AgentWatchProviderState::RecoveringContext { .. } => {
            crate::internal_envelope::frame(&format!(
                "Watched agent {sender_label} provider status: recovering_context (context_window)"
            ))
        }
        tau_proto::AgentWatchProviderState::Blocked { category } => {
            crate::internal_envelope::frame(&format!(
                "Watched agent {sender_label} provider status: blocked ({})",
                category.as_str()
            ))
        }
        tau_proto::AgentWatchProviderState::DispatchUncertain { category } => {
            crate::internal_envelope::frame(&format!(
                "Watched agent {sender_label} provider status: dispatch_uncertain ({})",
                category.as_str()
            ))
        }
        tau_proto::AgentWatchProviderState::TerminalError { failure_kind, .. } => {
            crate::internal_envelope::frame(&format!(
                "Watched agent {sender_label} provider status: terminal error ({})",
                failure_kind.as_str()
            ))
        }
        tau_proto::AgentWatchProviderState::TerminalIncomplete { category, .. } => {
            crate::internal_envelope::frame(&format!(
                "Watched agent {sender_label} provider status: terminal incomplete ({})",
                category.as_str()
            ))
        }
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
        tau_proto::AgentWatchProviderState::TerminalIncomplete { category, .. } => {
            format!("terminal incomplete ({})", category.as_str())
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
    /// Whether selected context projects a payload envelope that needs the
    /// model-visible provenance notice.
    pub(crate) contains_payload_envelope_provenance_projection: bool,
}

fn is_payload_envelope_provenance_projection(text: &str) -> bool {
    tau_proto::registered_payload_envelopes()
        .iter()
        .filter(|family| family.name != tau_proto::TAU_INTERNAL_HEADER_NAME)
        .any(|family| family.matches_whole(text))
        || [
            ("<message>", MESSAGE_CLOSE),
            ("<tau_peer_message", PEER_MESSAGE_CLOSE),
            ("<response>", WATCH_RESPONSE_CLOSE),
            ("<prompt>", WATCH_PROMPT_CLOSE),
        ]
        .into_iter()
        .any(|(open, close)| text.ends_with(close) && text.starts_with(open))
}

fn context_items_contain_payload_envelope_provenance_projection(items: &[ContextItem]) -> bool {
    items.iter().any(|item| match item {
        ContextItem::Message(message) => message.content.iter().any(|part| match part {
            tau_proto::ContentPart::SyntheticCompactionSummary { .. } => true,
            tau_proto::ContentPart::Text { text }
            | tau_proto::ContentPart::HarnessInternalText { text } => {
                is_payload_envelope_provenance_projection(text)
            }
            tau_proto::ContentPart::UrlCitation { .. }
            | tau_proto::ContentPart::CitationMetadataInvalid => false,
        }),
        ContextItem::ToolResult(result) => {
            result.presentation == tau_proto::ToolResultPresentation::HarnessDedupPointer
                || is_payload_envelope_provenance_projection(&result.output.body)
        }
        _ => false,
    })
}

fn tool_results_contain_payload_envelope_provenance_projection(
    items: &[tau_proto::ToolResultItem],
) -> bool {
    items.iter().any(|item| {
        item.presentation == tau_proto::ToolResultPresentation::HarnessDedupPointer
            || is_payload_envelope_provenance_projection(&item.output.body)
    })
}

/// Returns whether the selected provider window needs the model-visible
/// exact-envelope provenance notice without materializing provider context.
pub(crate) fn active_prompt_context_contains_payload_envelope_provenance_projection(
    tree: &tau_core::AgentTree,
    head: Option<tau_core::NodeId>,
) -> bool {
    let active_window = tree.active_provider_window(head);
    let mut contains_projection = active_window
        .replacement
        .is_some_and(context_items_contain_payload_envelope_provenance_projection);

    for (_, entry) in active_window.transcript {
        #[cfg(test)]
        PROMPT_PREFLIGHT_ENTRY_VISIT_COUNT
            .set(PROMPT_PREFLIGHT_ENTRY_VISIT_COUNT.get().saturating_add(1));
        match entry {
            AgentEntry::Compaction {
                replacement_window, ..
            } => {
                contains_projection = context_items_contain_payload_envelope_provenance_projection(
                    replacement_window,
                );
            }
            AgentEntry::UserInput {
                items,
                submission_source,
                ..
            } => {
                contains_projection |= submission_source.as_ref()
                    == Some(&tau_proto::PromptSubmissionSource::HumanUi)
                    || items.iter().any(|item| {
                        matches!(
                            item,
                            ContextItem::Message(message)
                                if message.content.iter().any(|part| matches!(
                                    part,
                                    tau_proto::ContentPart::HarnessInternalText { .. }
                                ))
                        )
                    });
            }
            AgentEntry::ToolResults { items } => {
                contains_projection |=
                    tool_results_contain_payload_envelope_provenance_projection(items);
            }
            AgentEntry::AgentMessage {
                direction, kind, ..
            } => {
                contains_projection |= *direction == tau_core::AgentMessageDirection::Inbound
                    && matches!(
                        kind,
                        tau_proto::AgentMessageKind::Message
                            | tau_proto::AgentMessageKind::WatchResponse
                            | tau_proto::AgentMessageKind::WatchPrompt
                    );
            }
            AgentEntry::MessageFact { .. } => contains_projection = true,
            AgentEntry::AssistantResponse { .. } | AgentEntry::CompactionTrigger { .. } => {}
        }
    }

    contains_projection
}

/// Assembles provider context from the selected transcript branch.
pub(crate) fn assemble_prompt_context_from(
    tree: &tau_core::AgentTree,
    head: Option<tau_core::NodeId>,
) -> AssembledPromptContext {
    assemble_prompt_context_window(tree, head, None, None)
}

/// Returns whether one durable agent-message entry contributes model-visible
/// input when prompt assembly lowers the active provider window.
pub(crate) fn agent_message_is_provider_visible(entry: &AgentEntry) -> bool {
    let AgentEntry::AgentMessage {
        direction,
        sender_id,
        kind,
        watch_provider_status,
        watch_work_status,
        watch_long_wait,
        watch_lifecycle,
        ..
    } = entry
    else {
        return false;
    };
    if *direction != tau_core::AgentMessageDirection::Inbound {
        return false;
    }
    match kind {
        tau_proto::AgentMessageKind::Message
        | tau_proto::AgentMessageKind::WatchResponse
        | tau_proto::AgentMessageKind::WatchPrompt => true,
        tau_proto::AgentMessageKind::WatchProviderStatus => watch_provider_status
            .as_ref()
            .is_some_and(|status| !status.initial),
        tau_proto::AgentMessageKind::WatchWorkStatus => watch_work_status
            .as_deref()
            .is_some_and(|status| watch_work_status_text(sender_id.as_str(), status).is_some()),
        tau_proto::AgentMessageKind::WatchLongWait => watch_long_wait.is_some(),
        tau_proto::AgentMessageKind::WatchLifecycle => watch_lifecycle.is_some(),
    }
}

/// Assemble a logical active-window prefix ending at `cut`.
///
/// Unlike physical ancestry assembly, this retains the latest replacement and
/// addresses nodes in its preserved suffix, allowing a later rolling
/// compaction pass to compact `replacement + prefix(suffix)`.
pub(crate) fn assemble_prompt_context_prefix_from(
    tree: &tau_core::AgentTree,
    active_head: Option<tau_core::NodeId>,
    cut: tau_proto::AgentHead,
) -> Option<AssembledPromptContext> {
    let window = tree.active_provider_window(active_head);
    let cut_exists = match cut {
        tau_proto::AgentHead::Root => window.replacement.is_none(),
        tau_proto::AgentHead::Node(node_id) => {
            window.replacement_boundary == Some(node_id)
                || window
                    .transcript
                    .iter()
                    .any(|(candidate, _)| *candidate == node_id)
        }
    };
    cut_exists.then(|| assemble_prompt_context_window(tree, active_head, Some(cut), None))
}

/// Assemble the complete logical window once and return its exact canonical
/// JSON byte length after every surviving transcript occurrence.
pub(crate) fn active_prompt_prefix_json_measurements(
    tree: &tau_core::AgentTree,
    active_head: Option<tau_core::NodeId>,
) -> Option<Vec<(tau_core::NodeId, tau_proto::ByteCount)>> {
    let mut measurements = Vec::new();
    let _ = assemble_prompt_context_window(tree, active_head, None, Some(&mut measurements));
    Some(measurements)
}

/// Builds the synthetic user block that prompt materialization prepends from
/// the agent's frozen initialization context.
pub(crate) fn initialization_agents_context_block(
    tree: &tau_core::AgentTree,
) -> Option<tau_proto::ContextBlock> {
    let agents_message = tree.initialization_context()?.agents_message.as_ref()?;
    Some(tau_proto::ContextBlock::UserInput(
        tau_proto::UserInputBlock {
            items: vec![ContextItem::Message(tau_proto::MessageItem {
                role: tau_proto::ContextRole::User,
                content: vec![tau_proto::ContentPart::Text {
                    text: agents_message.clone(),
                }],
                phase: None,
                responses_raw_json: None,
            })],
        },
    ))
}

fn assemble_prompt_context_window(
    tree: &tau_core::AgentTree,
    head: Option<tau_core::NodeId>,
    prefix_through: Option<tau_proto::AgentHead>,
    measurements: Option<&mut Vec<(tau_core::NodeId, tau_proto::ByteCount)>>,
) -> AssembledPromptContext {
    #[cfg(test)]
    PROMPT_CONTEXT_CONSTRUCTION_COUNT
        .set(PROMPT_CONTEXT_CONSTRUCTION_COUNT.get().saturating_add(1));
    let mut blocks: Vec<tau_proto::ContextBlock> = Vec::new();
    let mut contains_payload_envelope_provenance_projection = false;
    let mut active_window = tree.active_provider_window(head);
    if let Some(prefix_through) = prefix_through {
        match prefix_through {
            tau_proto::AgentHead::Root => {
                active_window.replacement = None;
                active_window.replacement_boundary = None;
                active_window.transcript.clear();
            }
            tau_proto::AgentHead::Node(node_id)
                if active_window.replacement_boundary == Some(node_id) =>
            {
                // A physical head at the replacement boundary names the whole
                // logical active window, including its preserved suffix.
            }
            tau_proto::AgentHead::Node(node_id) => {
                if let Some(index) = active_window
                    .transcript
                    .iter()
                    .position(|(candidate, _)| *candidate == node_id)
                {
                    active_window.transcript.truncate(index.saturating_add(1));
                }
            }
        }
    }
    if let Some(replacement_window) = active_window.replacement {
        contains_payload_envelope_provenance_projection =
            context_items_contain_payload_envelope_provenance_projection(replacement_window);
        blocks.push(tau_proto::ContextBlock::UserInput(
            tau_proto::UserInputBlock {
                items: replacement_window.to_vec(),
            },
        ));
    }
    let mut measurement_state = measurements
        .map(|measurements| PromptContextMeasurementState::new(tree, &blocks, measurements));

    for (node_id, entry) in active_window.transcript {
        if matches!(entry, AgentEntry::AgentMessage { .. })
            && !agent_message_is_provider_visible(entry)
        {
            if let Some(measurement_state) = measurement_state.as_mut() {
                measurement_state.record(node_id);
            }
            continue;
        }
        let blocks_before = measurement_state
            .as_ref()
            .map(|_| measurement_block_count(&blocks));
        match entry {
            AgentEntry::Compaction {
                replacement_window, ..
            } => {
                blocks.clear();
                contains_payload_envelope_provenance_projection =
                    context_items_contain_payload_envelope_provenance_projection(
                        replacement_window,
                    );
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
                contains_payload_envelope_provenance_projection |= submission_source.as_ref()
                    == Some(&tau_proto::PromptSubmissionSource::HumanUi)
                    || items.iter().any(|item| {
                        matches!(
                            item,
                            ContextItem::Message(message)
                                if message.content.iter().any(|part| matches!(
                                    part,
                                    tau_proto::ContentPart::HarnessInternalText { .. }
                                ))
                        )
                    });
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
                let items = project_tool_result_items(items);
                contains_payload_envelope_provenance_projection |=
                    tool_results_contain_payload_envelope_provenance_projection(&items);
                blocks.push(tau_proto::ContextBlock::ToolResults(
                    tau_proto::ToolResultsBlock { items },
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
                watch_lifecycle,
                message,
            } => match kind {
                tau_proto::AgentMessageKind::Message => {
                    if *direction == tau_core::AgentMessageDirection::Outbound {
                        // The original tool call/result already records the sender turn.
                        // Replaying this routing fact would fabricate assistant output.
                        if let Some(measurement_state) = measurement_state.as_mut() {
                            measurement_state.record(node_id);
                        }
                        continue;
                    }
                    contains_payload_envelope_provenance_projection = true;
                    let message_text = match sender_session_id {
                        Some(sender_session_id) => {
                            let body = tau_proto::escape_exact_sentinel_close(
                                message,
                                PEER_MESSAGE_CLOSE,
                                PEER_MESSAGE_CLOSE_VISIBLE,
                            );
                            crate::internal_envelope::frame(&format!(
                                "Authenticated peer message\n\n<tau_peer_message sender_session=\"{}\" sender_agent=\"{}\">\n{}\n</tau_peer_message>",
                                xml_escape(sender_session_id.as_str()),
                                xml_escape(sender_id.as_str()),
                                body
                            ))
                        }
                        None => {
                            let body = tau_proto::escape_exact_sentinel_close(
                                message,
                                MESSAGE_CLOSE,
                                MESSAGE_CLOSE_VISIBLE,
                            );
                            crate::internal_envelope::frame(&format!(
                                "You have received a message from {sender_id}\n\n<message>\n{body}\n</message>"
                            ))
                        }
                    };
                    blocks.push(tau_proto::ContextBlock::UserInput(
                        tau_proto::UserInputBlock {
                            items: vec![ContextItem::Message(tau_proto::MessageItem {
                                role: tau_proto::ContextRole::User,
                                content: vec![tau_proto::ContentPart::Text { text: message_text }],
                                phase: None,
                                responses_raw_json: None,
                            })],
                        },
                    ));
                }
                tau_proto::AgentMessageKind::WatchResponse => {
                    if *direction == tau_core::AgentMessageDirection::Inbound {
                        contains_payload_envelope_provenance_projection = true;
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
                                            crate::internal_envelope::frame(&format!(
                                                "Watched agent {sender_label} emitted a response\n\n<response>\n{body}\n</response>"
                                            ))
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
                        contains_payload_envelope_provenance_projection = true;
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
                                            crate::internal_envelope::frame(&format!(
                                                "Watched agent {sender_label} received a user prompt\n\n<prompt>\n{body}\n</prompt>"
                                            ))
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
                                        text: crate::internal_envelope::frame(&format!(
                                            "Watched agent {sender_id} has spent over {} minutes waiting.",
                                            wait.threshold_minutes
                                        )),
                                    }],
                                    phase: None,
                                    responses_raw_json: None,
                                })],
                            },
                        ));
                    }
                }
                tau_proto::AgentMessageKind::WatchLifecycle => {
                    if let (tau_core::AgentMessageDirection::Inbound, Some(lifecycle)) =
                        (direction, watch_lifecycle.as_ref())
                    {
                        let reason = match lifecycle.reason {
                            tau_proto::AgentWatchLifecycleReason::RestoredDelegationRouteLost => {
                                "restored delegation lost its completion route"
                            }
                            tau_proto::AgentWatchLifecycleReason::UnexpectedUnload => {
                                "unexpected unload"
                            }
                        };
                        blocks.push(tau_proto::ContextBlock::UserInput(
                            tau_proto::UserInputBlock {
                                items: vec![ContextItem::Message(tau_proto::MessageItem {
                                    role: tau_proto::ContextRole::User,
                                    content: vec![tau_proto::ContentPart::Text {
                                        text: crate::internal_envelope::frame(&format!(
                                            "Watched agent {sender_id} stopped: {reason}"
                                        )),
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
                contains_payload_envelope_provenance_projection = true;
                blocks.push(tau_proto::ContextBlock::UserInput(
                    tau_proto::UserInputBlock {
                        items: vec![ContextItem::Message(*item.clone())],
                    },
                ));
            }
        }
        if let Some(measurement_state) = measurement_state.as_mut() {
            let blocks_before = blocks_before.expect("measurement snapshot must exist");
            measurement_state.append_blocks(&blocks[blocks_before..]);
            measurement_state.record(node_id);
        }
    }

    AssembledPromptContext {
        context: tau_proto::PromptContext { blocks },
        contains_payload_envelope_provenance_projection,
    }
}

/// Exact serialized-prefix accounting used only by rolling-compaction
/// admission.
struct PromptContextMeasurementState<'a> {
    /// Caller-owned per-node measurement output.
    measurements: &'a mut Vec<(tau_core::NodeId, tau_proto::ByteCount)>,
    /// Exact JSON byte count for the prefix through the current node.
    serialized_bytes: tau_proto::ByteCount,
    /// Number of blocks already represented in `serialized_bytes`.
    serialized_block_count: usize,
}

impl<'a> PromptContextMeasurementState<'a> {
    /// Initialize accounting from the replacement window and initialization
    /// block.
    fn new(
        tree: &tau_core::AgentTree,
        blocks: &[tau_proto::ContextBlock],
        measurements: &'a mut Vec<(tau_core::NodeId, tau_proto::ByteCount)>,
    ) -> Self {
        // Prefix admission measures the exact historical context that prompt
        // materialization will send. Keep the initialization block out of the
        // assembled transcript itself so materialization remains its sole owner.
        let measurement_prefix = initialization_agents_context_block(tree);
        let measured_blocks = measurement_prefix
            .iter()
            .map(clone_measured_block)
            .chain(blocks.iter().map(clone_measured_block))
            .collect();
        let serialized_bytes = measurement_json_bytes(&tau_proto::PromptContext {
            blocks: measured_blocks,
        });
        let serialized_block_count = blocks.len() + usize::from(measurement_prefix.is_some());
        Self {
            measurements,
            serialized_bytes,
            serialized_block_count,
        }
    }

    /// Extend the exact JSON byte count with newly appended context blocks.
    fn append_blocks(&mut self, blocks: &[tau_proto::ContextBlock]) {
        for block in blocks {
            let block_bytes = measurement_json_bytes(block);
            self.serialized_bytes = self
                .serialized_bytes
                .checked_add(block_bytes)
                .and_then(|bytes| {
                    bytes.checked_add(tau_proto::ByteCount::new(u64::from(
                        self.serialized_block_count != 0,
                    )))
                })
                .unwrap_or(tau_proto::ByteCount::MAX);
            self.serialized_block_count = self.serialized_block_count.saturating_add(1);
        }
    }

    /// Record the current prefix size for one canonical transcript node.
    fn record(&mut self, node_id: tau_core::NodeId) {
        self.measurements.push((node_id, self.serialized_bytes));
    }
}

/// Clone one context block solely for the initial measurement prefix.
fn clone_measured_block(block: &tau_proto::ContextBlock) -> tau_proto::ContextBlock {
    #[cfg(test)]
    PROMPT_MEASURED_BLOCK_CLONE_COUNT
        .set(PROMPT_MEASURED_BLOCK_CLONE_COUNT.get().saturating_add(1));
    block.clone()
}

/// Serialize one measurement-only JSON value and return its exact byte count.
fn measurement_json_bytes(value: &impl serde::Serialize) -> tau_proto::ByteCount {
    #[cfg(test)]
    PROMPT_MEASUREMENT_JSON_SERIALIZATION_COUNT.set(
        PROMPT_MEASUREMENT_JSON_SERIALIZATION_COUNT
            .get()
            .saturating_add(1),
    );
    serde_json::to_vec(value)
        .ok()
        .and_then(|encoded| u64::try_from(encoded.len()).ok())
        .map(tau_proto::ByteCount::new)
        .unwrap_or(tau_proto::ByteCount::MAX)
}

/// Snapshot the current output block count solely for prefix measurement.
fn measurement_block_count(blocks: &[tau_proto::ContextBlock]) -> usize {
    #[cfg(test)]
    PROMPT_MEASUREMENT_BLOCK_COUNT_SNAPSHOT_COUNT.set(
        PROMPT_MEASUREMENT_BLOCK_COUNT_SNAPSHOT_COUNT
            .get()
            .saturating_add(1),
    );
    blocks.len()
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
    let human_ui = submission_source == Some(&tau_proto::PromptSubmissionSource::HumanUi);
    for item in &mut projected {
        if let ContextItem::Message(message) = item {
            for part in &mut message.content {
                match part {
                    tau_proto::ContentPart::HarnessInternalText { text } => {
                        *part = tau_proto::ContentPart::Text {
                            text: crate::internal_envelope::frame(text),
                        };
                    }
                    tau_proto::ContentPart::SyntheticCompactionSummary { .. } => {}
                    tau_proto::ContentPart::UrlCitation { .. }
                    | tau_proto::ContentPart::CitationMetadataInvalid => {}
                    tau_proto::ContentPart::Text { text } => {
                        let body = tau_proto::escape_exact_sentinel_close(
                            text,
                            crate::internal_envelope::TAU_INTERNAL_CLOSE,
                            crate::internal_envelope::TAU_INTERNAL_CLOSE_VISIBLE,
                        );
                        if human_ui {
                            let body = tau_proto::USER_PAYLOAD_ENVELOPE.escape_body(&body);
                            *text = format!("<user>{body}</user>");
                        } else {
                            *text = body.into_owned();
                        }
                    }
                }
            }
        }
    }
    projected
}

/// Project durable tool terminals without treating producer-controlled text as
/// presentation authority. Only the harness-stamped dedup discriminator gets
/// the internal envelope; every ordinary tool payload has an exact close
/// neutralized before it reaches a provider.
pub(crate) fn project_tool_result_items(
    items: &[tau_proto::ToolResultItem],
) -> Vec<tau_proto::ToolResultItem> {
    items
        .iter()
        .cloned()
        .map(|mut item| {
            item.output.body = match item.presentation {
                tau_proto::ToolResultPresentation::ToolPayload => {
                    crate::internal_envelope::escape_untrusted_close(&item.output.body).into_owned()
                }
                tau_proto::ToolResultPresentation::HarnessDedupPointer => {
                    crate::internal_envelope::frame(&item.output.body)
                }
            };
            match &mut item.status {
                tau_proto::ToolResultStatus::Error { message } => {
                    *message = match item.presentation {
                        tau_proto::ToolResultPresentation::ToolPayload => {
                            crate::internal_envelope::escape_untrusted_close(message).into_owned()
                        }
                        tau_proto::ToolResultPresentation::HarnessDedupPointer => {
                            crate::internal_envelope::frame(message)
                        }
                    };
                }
                tau_proto::ToolResultStatus::Cancelled { reason } => {
                    *reason = crate::internal_envelope::escape_untrusted_close(reason).into_owned();
                }
                tau_proto::ToolResultStatus::Success => {}
            }
            item
        })
        .collect()
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
