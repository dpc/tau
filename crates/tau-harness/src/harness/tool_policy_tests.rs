//! Prompt capability and tool-policy behavior is specified by
//! `SPEC-tau-harness-prompt-dispatch`.

use std::collections::{BTreeSet, HashMap};
use std::os::unix::net::UnixStream;

use tau_config::settings::{AgentRole, ShellToolStyle, TauDirs, ToolPolicy};
use tau_core::ToolRegistration;
use tau_proto::{
    BackgroundSupport, Effort, ModelId, ModelName, ModelTag, ProviderModelInfo, ProviderName,
    ThinkingSummary, ToolGroup, ToolGroupName, ToolName, ToolSpec, ToolTag, ToolType, Verbosity,
};
use tempfile::TempDir;

use super::Harness;

const ROLE: &str = "test";

struct PolicyHarness {
    harness: Harness,
    _temp_dir: TempDir,
}

fn echo_runner(r: UnixStream, w: UnixStream) -> Result<(), String> {
    super::run_echo_provider(r, w).map_err(|error| error.to_string())
}

fn tagged_tool(name: &str, enabled_by_default: bool, tags: &[&str]) -> ToolSpec {
    ToolSpec {
        name: ToolName::new(name),
        model_visible_name: None,
        description: None,
        tool_type: ToolType::Function,
        parameters: None,
        format: None,
        tags: tags.iter().map(|tag| ToolTag::new(*tag)).collect(),
        enabled_by_default,
        background_support: Some(BackgroundSupport::Never),
        examples: Vec::new(),
    }
}

/// UI shell routing counts configured generic-shell owners exactly once,
/// independently of prefixes and unrelated tools.
#[test]
fn ui_shell_provider_discovery_distinguishes_zero_one_and_two_instances() {
    let mut registry = tau_core::ToolRegistry::new();
    assert!(super::ui_shell_provider_ids(&registry).is_empty());
    registry.register(
        &crate::test_connection_id("shell-a"),
        tagged_tool("shell", true, &["shell:exec:generic"]),
    );
    assert_eq!(super::ui_shell_provider_ids(&registry).len(), 1);
    registry.register(
        &crate::test_connection_id("shell-b"),
        tagged_tool("prod_shell", true, &["shell:exec:generic"]),
    );
    assert_eq!(super::ui_shell_provider_ids(&registry).len(), 2);
}

fn model_info(model: &ModelId, tags: &[&str]) -> ProviderModelInfo {
    ProviderModelInfo {
        id: model.clone(),
        display_name: None,
        tags: tags.iter().map(|tag| ModelTag::new(*tag)).collect(),
        supported_tool_types: vec![ToolType::Function],
        input_modalities: Vec::new(),
        tool_result_modalities: Vec::new(),
        supports_parallel_tool_calls: true,
        default_affinity: 0,
        context_window: tau_proto::TokenCount::new(128_000),
        efforts: vec![Effort::Off],
        verbosities: vec![Verbosity::Medium],
        thinking_summaries: vec![ThinkingSummary::Off],
        supports_compaction: false,
        supports_standalone_compaction: false,
        standalone_compaction_generation_negative: false,
        standalone_compaction_threshold: None,
        standalone_compaction_prefix_budget: None,
        cache_policy: None,
        est_uncached_input_cost_1m_usd: Default::default(),
        est_cached_input_cost_1m_usd: Default::default(),
        est_cache_write_input_cost_1m_usd: Default::default(),
        est_output_cost_1m_usd: Default::default(),
        est_cache_storage_cost_1m_token_hour_usd: None,
    }
}

fn policy_harness(model_tags: &[&str], role: AgentRole) -> PolicyHarness {
    policy_harness_for_model("model", model_tags, role)
}

fn policy_harness_for_model(
    model_name: &str,
    model_tags: &[&str],
    role: AgentRole,
) -> PolicyHarness {
    let temp_dir = TempDir::new().expect("temp dir");
    let state_dir = temp_dir.path().join("state");
    let dirs = TauDirs {
        config_dir: Some(temp_dir.path().join("config")),
        state_dir: Some(state_dir.join("runtime")),
    };
    let mut harness = Harness::new_with_provider(
        &state_dir,
        dirs,
        echo_runner,
        Vec::new(),
        "test-session",
        tau_proto::SessionStartReason::Initial,
        crate::HarnessStorageMode::Durable,
    )
    .expect("harness");
    harness.config.available_roles = HashMap::from([(ROLE.to_owned(), role)]);
    let model = ModelId::new(ProviderName::new("provider"), ModelName::new(model_name));
    harness.provider_runtime.model_info =
        HashMap::from([(model.clone(), model_info(&model, model_tags))]);
    harness.provider_runtime.model_routes =
        HashMap::from([(model.clone(), crate::test_connection_id("provider"))]);
    harness.config.selected_role = ROLE.to_owned();
    harness.config.selected_model = Some(model.clone());
    let group = ToolGroup {
        name: ToolGroupName::new("shell"),
        prompt_fragment: None,
    };
    let mut replace = tagged_tool("replace", false, &["shell:edit", "shell:edit:replace"]);
    replace.model_visible_name = Some(ToolName::new("edit"));
    for spec in [
        tagged_tool("edit", true, &["shell:edit", "shell:edit:line"]),
        replace,
        tagged_tool(
            "apply_patch",
            false,
            &["shell:edit", "shell:edit:apply_patch"],
        ),
        tagged_tool("shell", true, &["shell:exec:generic"]),
        tagged_tool("gpt_shell", false, &["shell:exec:shell_command"]),
        tagged_tool("read", true, &["shell:read"]),
        tagged_tool(
            "read_image",
            true,
            &["shell:read", "provider-content:image"],
        ),
        tagged_tool("workdir", true, &["shell:workdir"]),
        tagged_tool("dir_lock", true, &["shell:lock"]),
    ] {
        harness.tool_routing.registry.register_with_prompt_fragment(
            &crate::test_connection_id("tools"),
            ToolRegistration {
                tool: spec,
                tool_group: Some(group.clone()),
                prompt_fragment: None,
            },
        );
    }
    for (name, enabled_by_default, group, tags) in [
        (
            "compact",
            true,
            "compaction",
            &["harness:compaction", "harness:compaction:self"][..],
        ),
        (
            "agent_compact",
            false,
            "cross_agent_compaction",
            &[
                "harness:compaction",
                "harness:compaction:cross-agent",
                "harness:agent-control",
            ][..],
        ),
    ] {
        harness.tool_routing.registry.register_with_prompt_fragment(
            &crate::test_connection_id("harness"),
            ToolRegistration {
                tool: tagged_tool(name, enabled_by_default, tags),
                tool_group: Some(ToolGroup {
                    name: ToolGroupName::new(group),
                    prompt_fragment: None,
                }),
                prompt_fragment: None,
            },
        );
    }
    PolicyHarness {
        harness,
        _temp_dir: temp_dir,
    }
}

fn register_swarm_tools(harness: &mut Harness, prefix: Option<&str>) {
    let scoped_name =
        |name: &str| prefix.map_or_else(|| name.to_owned(), |prefix| format!("{prefix}_{name}"));
    let group = ToolGroup {
        name: ToolGroupName::new(scoped_name("swarm")),
        prompt_fragment: None,
    };
    for name in ["task_info", "task_update", "task_blocker"] {
        harness.tool_routing.registry.register_with_prompt_fragment(
            &crate::test_connection_id("swarm"),
            ToolRegistration {
                tool: tagged_tool(&scoped_name(name), false, &[]),
                tool_group: Some(group.clone()),
                prompt_fragment: None,
            },
        );
    }
}

fn register_rostra_tools(harness: &mut Harness) {
    let group = ToolGroup {
        name: ToolGroupName::new("rostra"),
        prompt_fragment: None,
    };
    for name in [
        "rostra_status",
        "rostra_list_posts",
        "rostra_read_post",
        "rostra_get_profile",
        "rostra_post",
        "rostra_react",
        "rostra_follow",
        "rostra_unfollow",
        "rostra_update_profile",
        "rostra_vote",
        "rostra_notifications",
    ] {
        harness.tool_routing.registry.register_with_prompt_fragment(
            &crate::test_connection_id("rostra"),
            ToolRegistration {
                tool: tagged_tool(name, true, &[]),
                tool_group: Some(group.clone()),
                prompt_fragment: None,
            },
        );
    }
}

/// Ensures a role cannot force-enable image-producing tools on a provider route
/// that did not explicitly publish both image-input and image-tool-result
/// support.
#[test]
fn image_tool_requires_exact_route_modalities() {
    let mut policy = policy_harness(&[], AgentRole::default());
    let model = policy
        .harness
        .config
        .selected_model
        .clone()
        .expect("selected model");
    let without_image = policy
        .harness
        .gather_effective_tool_specs_for_role_model(ROLE, Some(&model));
    assert!(!without_image.iter().any(|tool| tool.name == "read_image"));

    let model_info = policy
        .harness
        .provider_runtime
        .model_info
        .get_mut(&model)
        .expect("model metadata");
    model_info.input_modalities = vec![
        tau_proto::InputModality::Text,
        tau_proto::InputModality::Image,
    ];
    model_info.tool_result_modalities = vec![
        tau_proto::InputModality::Text,
        tau_proto::InputModality::Image,
    ];
    let with_image = policy
        .harness
        .gather_effective_tool_specs_for_role_model(ROLE, Some(&model));
    assert!(with_image.iter().any(|tool| tool.name == "read_image"));
}

fn effective_tool_names(harness: &Harness) -> Vec<String> {
    let relevant = [
        "edit",
        "replace",
        "apply_patch",
        "shell",
        "gpt_shell",
        "read",
        "workdir",
        "dir_lock",
        "compact",
        "agent_compact",
        "rostra_status",
        "rostra_list_posts",
        "rostra_read_post",
        "rostra_get_profile",
        "rostra_post",
        "rostra_react",
        "rostra_follow",
        "rostra_unfollow",
        "rostra_update_profile",
        "rostra_vote",
        "rostra_notifications",
    ];
    let model = harness.config.selected_model.as_ref();
    harness
        .gather_effective_tool_specs_for_role_model(ROLE, model)
        .into_iter()
        .map(|spec| spec.name.into_string())
        .filter(|name| relevant.contains(&name.as_str()))
        .collect()
}

/// Ensures untagged models select the exact-text implementation while explicit
/// tags still choose exactly one editing surface before role controls.
#[test]
fn shell_tool_style_selects_one_edit_surface() {
    let tagged = policy_harness(&["shell:tool-style:replace"], AgentRole::default());
    let tools = effective_tool_names(&tagged.harness);
    assert!(tools.contains(&"replace".to_owned()));
    assert!(!tools.contains(&"edit".to_owned()));
    assert!(!tools.contains(&"apply_patch".to_owned()));

    let ordinary = policy_harness_for_model("arbitrary-model", &[], AgentRole::default());
    let tools = effective_tool_names(&ordinary.harness);
    assert!(tools.contains(&"replace".to_owned()));
    assert!(!tools.contains(&"edit".to_owned()));
}

/// Ensures explicit model and global selectors retain their implementation
/// meanings while every untagged ordinary model gets exact-text editing.
#[test]
fn shell_tool_style_selectors_override_exact_text_default() {
    for model_name in ["small-local", "large-hosted", "future-model"] {
        let policy = policy_harness_for_model(model_name, &[], AgentRole::default());
        let tools = effective_tool_names(&policy.harness);
        assert!(
            tools.contains(&"replace".to_owned()),
            "{model_name} must expose replace"
        );
        assert!(
            !tools.contains(&"edit".to_owned()),
            "{model_name} must not expose edit"
        );
    }

    let provider_override = policy_harness_for_model(
        "arbitrary-model",
        &["shell:tool-style:edit"],
        AgentRole::default(),
    );
    let provider_tools = effective_tool_names(&provider_override.harness);
    assert!(provider_tools.contains(&"edit".to_owned()));
    assert!(!provider_tools.contains(&"replace".to_owned()));

    let mut global_override =
        policy_harness_for_model("arbitrary-model", &[], AgentRole::default());
    global_override.harness.config.tool_policy = ToolPolicy {
        default_shell_tool_style: Some(ShellToolStyle::Edit),
        ..ToolPolicy::default()
    };
    let global_tools = effective_tool_names(&global_override.harness);
    assert!(global_tools.contains(&"edit".to_owned()));
    assert!(!global_tools.contains(&"replace".to_owned()));
}

/// Ensures a configured global style wins over the model default while ordinary
/// role policy still runs after that base selection.
#[test]
fn configured_shell_tool_style_overrides_model_default() {
    let mut policy = policy_harness(&["shell:tool-style:replace"], AgentRole::default());
    policy.harness.config.tool_policy = ToolPolicy {
        default_shell_tool_style: Some(ShellToolStyle::Edit),
        ..ToolPolicy::default()
    };

    let tools = effective_tool_names(&policy.harness);
    assert!(tools.contains(&"edit".to_owned()));
    assert!(!tools.contains(&"replace".to_owned()));
}

/// Ensures forced Codex never silently loses its Custom patch tool when model
/// capability metadata is empty or Function-only, and rejects conflicting
/// explicit style tags even when a global style is configured.
#[test]
fn forced_codex_requires_custom_support_and_style_tags_cannot_conflict() {
    let mut forced = policy_harness(&["shell:tool-style:codex"], AgentRole::default());
    let model = forced.harness.config.selected_model.clone().expect("model");
    assert_eq!(
        forced.harness.shell_tool_style_error(Some(&model)),
        Some("Codex shell tool style requires Custom tool support".to_owned())
    );
    forced
        .harness
        .provider_runtime
        .model_info
        .get_mut(&model)
        .expect("model info")
        .supported_tool_types = vec![ToolType::Function];
    assert_eq!(
        forced.harness.shell_tool_style_error(Some(&model)),
        Some("Codex shell tool style requires Custom tool support".to_owned())
    );
    forced
        .harness
        .provider_runtime
        .model_info
        .get_mut(&model)
        .expect("model info")
        .supported_tool_types
        .push(ToolType::Custom);
    assert_eq!(forced.harness.shell_tool_style_error(Some(&model)), None);

    let mut conflicting = policy_harness(
        &["shell:tool-style:codex", "shell:tool-style:replace"],
        AgentRole::default(),
    );
    conflicting.harness.config.tool_policy = ToolPolicy {
        default_shell_tool_style: Some(ShellToolStyle::Edit),
        ..ToolPolicy::default()
    };
    let model = conflicting
        .harness
        .config
        .selected_model
        .as_ref()
        .expect("model");
    assert_eq!(
        conflicting.harness.shell_tool_style_error(Some(model)),
        Some("conflicting shell tool style tags".to_owned())
    );
}

/// Ensures repeated identical provider tags select their one intended surface
/// rather than being mistaken for conflicting style declarations.
#[test]
fn duplicate_identical_shell_style_tags_select_one_surface() {
    let policy = policy_harness(
        &["shell:tool-style:replace", "shell:tool-style:replace"],
        AgentRole::default(),
    );

    let tools = effective_tool_names(&policy.harness);

    assert!(tools.contains(&"replace".to_owned()));
    assert!(!tools.contains(&"edit".to_owned()));
    assert_eq!(
        policy
            .harness
            .shell_tool_style_error(policy.harness.config.selected_model.as_ref()),
        None
    );
}

/// Self-compaction is present by default, can be disabled independently, and
/// enabling cross-agent compaction does not alter either default.
#[test]
fn compaction_defaults_and_groups_are_independent() {
    let default = policy_harness(&[], AgentRole::default());
    let tools = effective_tool_names(&default.harness);
    assert!(tools.contains(&"compact".to_owned()));
    assert!(!tools.contains(&"agent_compact".to_owned()));

    let self_disabled = policy_harness(
        &[],
        AgentRole {
            disable_tool_groups: vec![ToolGroupName::new("compaction")],
            ..AgentRole::default()
        },
    );
    let tools = effective_tool_names(&self_disabled.harness);
    assert!(!tools.contains(&"compact".to_owned()));
    assert!(!tools.contains(&"agent_compact".to_owned()));

    let cross_only = policy_harness(
        &[],
        AgentRole {
            enable_tool_groups: vec![ToolGroupName::new("cross_agent_compaction")],
            ..AgentRole::default()
        },
    );
    let tools = effective_tool_names(&cross_only.harness);
    assert!(tools.contains(&"compact".to_owned()));
    assert!(tools.contains(&"agent_compact".to_owned()));
}

/// Ensures disabled-by-default Swarm tools stay absent unless a role enables
/// their group or one exact tool, while preserving exact-tool opt-in
/// precedence.
#[test]
fn swarm_tools_require_group_or_exact_role_opt_in() {
    let mut default = policy_harness(&[], AgentRole::default());
    register_swarm_tools(&mut default.harness, None);
    let default_tools = default.harness.gather_effective_tool_specs_for_role_model(
        ROLE,
        default.harness.config.selected_model.as_ref(),
    );
    assert!(!default_tools.iter().any(|tool| tool.name == "task_info"));
    assert!(!default_tools.iter().any(|tool| tool.name == "task_update"));
    assert!(!default_tools.iter().any(|tool| tool.name == "task_blocker"));

    let mut group_enabled = policy_harness(
        &[],
        AgentRole {
            enable_tool_groups: vec![ToolGroupName::new("swarm")],
            ..AgentRole::default()
        },
    );
    register_swarm_tools(&mut group_enabled.harness, None);
    let group_tools = group_enabled
        .harness
        .gather_effective_tool_specs_for_role_model(
            ROLE,
            group_enabled.harness.config.selected_model.as_ref(),
        );
    assert!(group_tools.iter().any(|tool| tool.name == "task_info"));
    assert!(group_tools.iter().any(|tool| tool.name == "task_update"));
    assert!(group_tools.iter().any(|tool| tool.name == "task_blocker"));
    assert!(!group_tools.iter().any(|tool| tool.name == "update"));
    assert!(!group_tools.iter().any(|tool| tool.name == "blocker"));

    let mut exact_enabled = policy_harness(
        &[],
        AgentRole {
            enable_tools: vec![ToolName::new("task_info")],
            ..AgentRole::default()
        },
    );
    register_swarm_tools(&mut exact_enabled.harness, None);
    let exact_tools = exact_enabled
        .harness
        .gather_effective_tool_specs_for_role_model(
            ROLE,
            exact_enabled.harness.config.selected_model.as_ref(),
        );
    assert!(exact_tools.iter().any(|tool| tool.name == "task_info"));
    assert!(!exact_tools.iter().any(|tool| tool.name == "task_update"));
    assert!(!exact_tools.iter().any(|tool| tool.name == "task_blocker"));

    let mut prefixed = policy_harness(
        &[],
        AgentRole {
            enable_tool_groups: vec![ToolGroupName::new("work_swarm")],
            ..AgentRole::default()
        },
    );
    register_swarm_tools(&mut prefixed.harness, Some("work"));
    let prefixed_tools = prefixed.harness.gather_effective_tool_specs_for_role_model(
        ROLE,
        prefixed.harness.config.selected_model.as_ref(),
    );
    assert!(
        prefixed_tools
            .iter()
            .any(|tool| tool.name == "work_task_info")
    );
    assert!(
        prefixed_tools
            .iter()
            .any(|tool| tool.name == "work_task_update")
    );
    assert!(
        prefixed_tools
            .iter()
            .any(|tool| tool.name == "work_task_blocker")
    );
}

/// Ensures the Rostra group enables every read, authenticated-write, and
/// notification tool, while an exact Rostra name remains a narrow opt-in.
#[test]
fn rostra_tools_support_group_and_exact_role_opt_in() {
    let mut group_enabled = policy_harness(
        &[],
        AgentRole {
            tools: Some(Vec::new()),
            enable_tool_groups: vec![ToolGroupName::new("rostra")],
            ..AgentRole::default()
        },
    );
    register_rostra_tools(&mut group_enabled.harness);
    let group_tools = effective_tool_names(&group_enabled.harness);
    let group_rostra_tools = group_tools
        .iter()
        .filter(|name| name.starts_with("rostra_"))
        .cloned()
        .collect::<BTreeSet<_>>();
    assert_eq!(
        group_rostra_tools,
        BTreeSet::from([
            "rostra_status".to_owned(),
            "rostra_list_posts".to_owned(),
            "rostra_read_post".to_owned(),
            "rostra_get_profile".to_owned(),
            "rostra_post".to_owned(),
            "rostra_react".to_owned(),
            "rostra_follow".to_owned(),
            "rostra_unfollow".to_owned(),
            "rostra_update_profile".to_owned(),
            "rostra_vote".to_owned(),
            "rostra_notifications".to_owned(),
        ])
    );

    let mut exact_enabled = policy_harness(
        &[],
        AgentRole {
            tools: Some(Vec::new()),
            enable_tools: vec![ToolName::new("rostra_post")],
            ..AgentRole::default()
        },
    );
    register_rostra_tools(&mut exact_enabled.harness);
    let exact_tools = effective_tool_names(&exact_enabled.harness);
    assert_eq!(
        exact_tools
            .iter()
            .filter(|name| name.starts_with("rostra_"))
            .cloned()
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["rostra_post".to_owned()])
    );
}

/// Exact tool and tag policy retains broad-to-specific precedence independently
/// for the self and cross-agent compaction capabilities.
#[test]
fn compaction_exact_and_tag_precedence_remains_independent() {
    let role = AgentRole {
        disable_tool_tags: serde_json::from_str(r#"["harness:compaction"]"#).expect("tag pattern"),
        enable_tools: vec![ToolName::new("agent_compact")],
        disable_tools: vec![ToolName::new("compact")],
        ..AgentRole::default()
    };
    let policy = policy_harness(&[], role);
    let tools = effective_tool_names(&policy.harness);
    assert!(!tools.contains(&"compact".to_owned()));
    assert!(tools.contains(&"agent_compact".to_owned()));
}

/// Prompt-owned compaction authority is immutable: later role changes cannot
/// authorize a tool absent from the originating snapshot or revoke one present.
#[test]
fn compaction_prompt_snapshot_survives_later_role_changes() {
    let mut policy = policy_harness(&[], AgentRole::default());
    let prompt_id: tau_proto::AgentPromptId = "prompt-compaction"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    policy
        .harness
        .prompt_coordination
        .prompt_runtime
        .tool_specs
        .insert(
            prompt_id.clone(),
            vec![tagged_tool(
                "compact",
                false,
                &["harness:compaction", "harness:compaction:self"],
            )],
        );
    policy.harness.config.available_roles.insert(
        ROLE.to_owned(),
        AgentRole {
            enable_tools: vec![ToolName::new("agent_compact")],
            disable_tools: vec![ToolName::new("compact")],
            ..AgentRole::default()
        },
    );
    assert!(
        policy
            .harness
            .resolve_enabled_tool_spec_for_prompt(&ToolName::new("compact"), &prompt_id)
            .is_some()
    );
    assert!(
        policy
            .harness
            .resolve_enabled_tool_spec_for_prompt(&ToolName::new("agent_compact"), &prompt_id)
            .is_none()
    );
}

/// Provider tool-type support is a final, non-overridable filter so capability
/// prose and prompt authorization cannot claim a tool the adapter will omit.
#[test]
fn provider_supported_tool_types_filter_effective_snapshot() {
    let mut policy = policy_harness(&[], AgentRole::default());
    let mut custom = tagged_tool("custom_text", true, &[]);
    custom.tool_type = ToolType::Custom;
    policy
        .harness
        .tool_routing
        .registry
        .register_with_prompt_fragment(
            &crate::test_connection_id("tools"),
            ToolRegistration {
                tool: custom,
                tool_group: None,
                prompt_fragment: None,
            },
        );
    let model = policy
        .harness
        .config
        .selected_model
        .clone()
        .expect("selected model");
    policy
        .harness
        .provider_runtime
        .model_info
        .get_mut(&model)
        .expect("model info")
        .supported_tool_types
        .clear();
    let specs = policy
        .harness
        .gather_effective_tool_specs_for_role_model(ROLE, Some(&model));
    assert!(
        specs.is_empty(),
        "an empty capability list must expose neither Function nor Custom tools"
    );

    policy
        .harness
        .provider_runtime
        .model_info
        .get_mut(&model)
        .expect("model info")
        .supported_tool_types = vec![ToolType::Function, ToolType::Custom];
    let specs = policy
        .harness
        .gather_effective_tool_specs_for_role_model(ROLE, Some(&model));
    assert!(specs.iter().any(|spec| spec.name.as_str() == "custom_text"));
}

/// Ensures untagged models use exact-text editing with generic shell defaults,
/// without implicitly promoting ChatGPT-oriented alternatives.
#[test]
fn generic_model_gets_generic_shell_alternatives() {
    let policy = policy_harness(&[], AgentRole::default());
    let tools = effective_tool_names(&policy.harness);

    assert!(tools.contains(&"replace".to_owned()));
    assert!(tools.contains(&"shell".to_owned()));
    assert!(tools.contains(&"dir_lock".to_owned()));
    assert!(!tools.contains(&"apply_patch".to_owned()));
    assert!(!tools.contains(&"gpt_shell".to_owned()));
}

/// Ensures a `shell:chatgpt` model gets only the declared shell exceptions
/// from the otherwise disabled shell tag family.
#[test]
fn chatgpt_model_gets_promoted_shell_alternatives() {
    let policy = policy_harness(&["shell:chatgpt"], AgentRole::default());
    let tools = effective_tool_names(&policy.harness);

    assert!(tools.contains(&"apply_patch".to_owned()));
    assert!(tools.contains(&"gpt_shell".to_owned()));
    assert!(tools.contains(&"workdir".to_owned()));
    assert!(tools.contains(&"dir_lock".to_owned()));
    assert!(!tools.contains(&"read".to_owned()));
    assert!(!tools.contains(&"edit".to_owned()));
    assert!(!tools.contains(&"shell".to_owned()));
}

/// Ensures an explicit `tools` allow-list is treated as the role-visible set
/// after global policy.
#[test]
fn explicit_tools_allowlist_overrides_global_policy_base() {
    let role = AgentRole {
        tools: Some(vec![ToolName::new("read")]),
        ..AgentRole::default()
    };
    let policy = policy_harness(&["shell:chatgpt"], role);
    let tools = effective_tool_names(&policy.harness);

    assert_eq!(tools, vec!["read".to_owned()]);
}

/// Ensures `enable_tools` can explicitly re-enable tools after tag policy.
#[test]
fn enable_tools_reenables_after_tag_policy() {
    let role = AgentRole {
        enable_tools: vec![ToolName::new("shell"), ToolName::new("apply_patch")],
        disable_tools: vec![ToolName::new("edit")],
        ..AgentRole::default()
    };
    let policy = policy_harness(&["shell:chatgpt"], role);
    let tools = effective_tool_names(&policy.harness);

    assert!(tools.contains(&"shell".to_owned()));
    assert!(tools.contains(&"apply_patch".to_owned()));
    assert!(!tools.contains(&"edit".to_owned()));
}

/// Ensures final per-tool enables can re-enable after per-tool disables because
/// role operations run broad-to-specific and disable-before-enable.
#[test]
fn enable_tools_runs_after_disable_tools() {
    let role = AgentRole {
        enable_tools: vec![ToolName::new("apply_patch")],
        disable_tools: vec![
            ToolName::new("edit"),
            ToolName::new("shell"),
            ToolName::new("apply_patch"),
        ],
        ..AgentRole::default()
    };
    let policy = policy_harness(&["shell:chatgpt"], role);
    let tools = effective_tool_names(&policy.harness);

    assert!(!tools.contains(&"edit".to_owned()));
    assert!(tools.contains(&"apply_patch".to_owned()));
    assert!(!tools.contains(&"shell".to_owned()));
    assert!(tools.contains(&"gpt_shell".to_owned()));
}

/// Ensures group disables run before individual tool enables.
#[test]
fn disable_tool_groups_runs_before_enable_tools() {
    let role = AgentRole {
        disable_tool_groups: vec![ToolGroupName::new("shell")],
        enable_tools: vec![ToolName::new("apply_patch")],
        ..AgentRole::default()
    };
    let policy = policy_harness(&["shell:chatgpt"], role);
    let tools = effective_tool_names(&policy.harness);

    assert_eq!(tools, vec!["apply_patch".to_owned(), "compact".to_owned()]);
}

/// Ensures a keyed user override can disable the built-in ChatGPT shell policy
/// without requiring tools or providers to publish model-specific tags.
#[test]
fn user_can_disable_builtin_chatgpt_shell_policy() {
    let mut policy = policy_harness(&["shell:chatgpt"], AgentRole::default());
    policy
        .harness
        .config
        .tool_policy
        .rules
        .entry("builtin.chatgpt-shell".to_owned())
        .or_default()
        .enable = false;

    let tools = effective_tool_names(&policy.harness);

    assert!(tools.contains(&"edit".to_owned()));
    assert!(tools.contains(&"shell".to_owned()));
    assert!(tools.contains(&"read".to_owned()));
    assert!(!tools.contains(&"apply_patch".to_owned()));
    assert!(!tools.contains(&"gpt_shell".to_owned()));
}

/// Ensures a custom policy rule can disable a broad tag prefix and then
/// re-enable a more specific tag prefix in the same evaluator path.
#[test]
fn custom_policy_rule_disables_and_enables_tool_tags() {
    let mut policy = policy_harness(&[], AgentRole::default());
    policy.harness.config.tool_policy = serde_json::from_str::<ToolPolicy>(
        r#"{
  "rules": {
    "custom.shell-cwd-only": {
      "disable_tool_tags": ["shell:*"],
      "enable_tool_tags": ["shell:workdir"]
    }
  }
}"#,
    )
    .expect("policy parses");

    let tools = effective_tool_names(&policy.harness);

    assert_eq!(tools, vec!["compact".to_owned(), "workdir".to_owned()]);
}

/// Ensures role-level tag operations run after global policy and before
/// group/tool name operations, preserving broad-to-specific overrides.
#[test]
fn role_tool_tags_run_after_policy_before_groups_and_tools() {
    let role = AgentRole {
        disable_tool_tags: serde_json::from_str(r#"["shell:*"]"#).expect("tag pattern parses"),
        enable_tool_tags: serde_json::from_str(r#"["shell:workdir"]"#).expect("tag pattern parses"),
        disable_tool_groups: vec![ToolGroupName::new("shell")],
        enable_tools: vec![ToolName::new("workdir")],
        ..AgentRole::default()
    };
    let policy = policy_harness(&["shell:chatgpt"], role);
    let tools = effective_tool_names(&policy.harness);

    assert_eq!(tools, vec!["compact".to_owned(), "workdir".to_owned()]);
}

/// Ensures prompt-owned tool lookup uses the advertised snapshot rather than
/// mutable current-role policy, while still accepting tools that were
/// advertised in the prompt snapshot. This is the authority boundary recorded
/// by `SPEC-tau-harness-prompt-dispatch`.
#[test]
fn prompt_snapshot_lookup_is_strict_and_survives_role_changes() {
    let mut policy = policy_harness(&["shell:chatgpt"], AgentRole::default());
    let prompt_id: tau_proto::AgentPromptId = "prompt-1"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    policy
        .harness
        .prompt_coordination
        .prompt_runtime
        .tool_specs
        .insert(
            prompt_id.clone(),
            vec![tagged_tool("edit", true, &["shell:edit:line"])],
        );
    policy.harness.config.available_roles.insert(
        ROLE.to_owned(),
        AgentRole {
            enable_tools: vec![ToolName::new("apply_patch")],
            ..AgentRole::default()
        },
    );

    assert!(
        policy
            .harness
            .resolve_enabled_tool_spec_for_prompt(&ToolName::new("edit"), &prompt_id)
            .is_some()
    );
    assert!(
        policy
            .harness
            .resolve_enabled_tool_spec_for_prompt(&ToolName::new("apply_patch"), &prompt_id)
            .is_none()
    );
}

/// Ensures prompt snapshot cleanup removes both the prompt-level spec snapshot
/// and all call-id backreferences for that prompt.
#[test]
fn prompt_snapshot_cleanup_removes_call_backreferences() {
    let mut policy = policy_harness(&[], AgentRole::default());
    let prompt_id: tau_proto::AgentPromptId = "prompt-cleanup"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    policy
        .harness
        .prompt_coordination
        .prompt_runtime
        .tool_specs
        .insert(
            prompt_id.clone(),
            vec![tagged_tool("edit", true, &["shell:edit:line"])],
        );
    policy
        .harness
        .prompt_coordination
        .prompt_runtime
        .tool_call_prompts
        .insert("call-1".into(), prompt_id.clone());

    policy.harness.clear_prompt_tool_snapshot(&prompt_id);

    assert!(
        !policy
            .harness
            .prompt_coordination
            .prompt_runtime
            .tool_specs
            .contains_key(&prompt_id)
    );
    assert!(
        policy
            .harness
            .prompt_coordination
            .prompt_runtime
            .tool_call_prompts
            .is_empty()
    );
}

/// Effective alias validation is snapshot-local: duplicates are diagnosed only
/// among tools simultaneously supplied to one prompt.
#[test]
fn effective_tool_surface_detects_visible_alias_collision() {
    let spec = |internal: &str| ToolSpec {
        name: ToolName::new(internal),
        model_visible_name: Some(ToolName::new("shared")),
        description: None,
        tool_type: ToolType::Function,
        parameters: None,
        format: None,
        tags: Vec::new(),
        enabled_by_default: true,
        background_support: None,
        examples: Vec::new(),
    };
    assert_eq!(
        super::duplicate_model_visible_tool_name(&[spec("provider_a"), spec("provider_b")]),
        Some(ToolName::new("shared"))
    );
    assert_eq!(
        super::duplicate_model_visible_tool_name(&[spec("provider_a")]),
        None
    );
}

/// Ensures enabling both shell editor implementations rejects their shared
/// provider-visible `edit` name before prompt construction.
#[test]
fn role_cannot_enable_both_shell_editors_with_shared_visible_name() {
    let policy = policy_harness(
        &[],
        AgentRole {
            enable_tools: vec![ToolName::new("edit"), ToolName::new("replace")],
            ..AgentRole::default()
        },
    );
    let model = policy
        .harness
        .config
        .selected_model
        .as_ref()
        .expect("model");
    let specs = policy
        .harness
        .gather_effective_tool_specs_for_role_model(ROLE, Some(model));
    assert!(specs.iter().any(|spec| spec.name == "edit"));
    assert!(specs.iter().any(|spec| spec.name == "replace"));
    let error = policy
        .harness
        .try_build_system_prompt_for_role_and_agent(ROLE, None, None, &specs, None, false)
        .expect_err("both editors must collide as visible edit");
    assert!(
        error
            .to_string()
            .contains("duplicate model-visible name `edit`")
    );
}

/// Role policy may keep duplicate aliases exclusive, while production prompt
/// construction rejects the same aliases when both become effective.
#[test]
fn policy_exclusive_alias_builds_and_routes_but_joint_surface_fails() {
    let mut policy = policy_harness(&[], AgentRole::default());
    let aliased = |internal: &str| {
        let mut spec = tagged_tool(internal, false, &[]);
        spec.model_visible_name = Some(ToolName::new("shared_alias"));
        spec
    };
    policy
        .harness
        .tool_routing
        .registry
        .register_internal(&crate::test_connection_id("harness"), aliased("internal_a"));
    policy
        .harness
        .tool_routing
        .registry
        .register_internal(&crate::test_connection_id("harness"), aliased("internal_b"));
    policy.harness.config.available_roles.insert(
        ROLE.to_owned(),
        AgentRole {
            enable_tools: vec![ToolName::new("internal_a")],
            ..AgentRole::default()
        },
    );
    let model = policy
        .harness
        .config
        .selected_model
        .as_ref()
        .expect("model");
    let exclusive = policy
        .harness
        .gather_effective_tool_specs_for_role_model(ROLE, Some(model));
    assert!(
        policy
            .harness
            .try_build_system_prompt_for_role_and_agent(ROLE, None, None, &exclusive, None, false)
            .is_ok()
    );
    let (internal, visible) = policy
        .harness
        .resolve_enabled_tool_name_for_role(&ToolName::new("shared_alias"), ROLE)
        .expect("exclusive alias resolves");
    assert_eq!(internal.as_str(), "internal_a");
    assert_eq!(visible.as_str(), "shared_alias");
    let route = policy
        .harness
        .tool_routing
        .registry
        .route_tool_request(tau_proto::ToolRequest {
            call_id: "alias-call".into(),
            tool_name: internal,
            tool_type: ToolType::Function,
            arguments: tau_proto::CborValue::Map(Vec::new()),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        })
        .expect("route exclusive alias owner");
    assert_eq!(route.target, tau_core::ToolRouteTarget::Internal);

    policy.harness.config.available_roles.insert(
        ROLE.to_owned(),
        AgentRole {
            enable_tools: vec![ToolName::new("internal_a"), ToolName::new("internal_b")],
            ..AgentRole::default()
        },
    );
    let joint = policy
        .harness
        .gather_effective_tool_specs_for_role_model(ROLE, Some(model));
    let error = policy
        .harness
        .try_build_system_prompt_for_role_and_agent(ROLE, None, None, &joint, None, false)
        .expect_err("joint alias surface must fail");
    assert!(error.to_string().contains("duplicate model-visible name"));
}

/// Prompt-owned model calls resolve only through advertised visible names, so
/// an alias cannot be shadowed by another tool's internal name.
#[test]
fn prompt_alias_resolution_does_not_prefer_an_internal_name() {
    let mut policy = policy_harness(&[], AgentRole::default());
    let mut aliased = tagged_tool("internal_a", true, &[]);
    aliased.model_visible_name = Some(ToolName::new("visible_b"));
    let mut other = tagged_tool("visible_b", true, &[]);
    other.model_visible_name = Some(ToolName::new("visible_c"));
    let prompt_id: tau_proto::AgentPromptId = "alias-resolution"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    policy
        .harness
        .prompt_coordination
        .prompt_runtime
        .tool_specs
        .insert(prompt_id.clone(), vec![aliased, other]);

    let resolved = policy
        .harness
        .resolve_enabled_tool_spec_for_prompt(&ToolName::new("visible_b"), &prompt_id)
        .expect("visible alias resolves");
    assert_eq!(resolved.name.as_str(), "internal_a");
}
