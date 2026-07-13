//! Prompt capability and tool-policy behavior is specified by
//! `SPEC-tau-harness-prompt-dispatch`.

use std::collections::HashMap;
use std::os::unix::net::UnixStream;

use tau_config::settings::{AgentRole, TauDirs, ToolPolicy};
use tau_proto::{
    BackgroundSupport, Effort, ModelId, ModelName, ModelTag, ProviderModelInfo, ProviderName,
    ThinkingSummary, ToolGroup, ToolGroupName, ToolName, ToolRegister, ToolSpec, ToolTag, ToolType,
    Verbosity,
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

fn model_info(model: &ModelId, tags: &[&str]) -> ProviderModelInfo {
    ProviderModelInfo {
        id: model.clone(),
        display_name: None,
        tags: tags.iter().map(|tag| ModelTag::new(*tag)).collect(),
        supported_tool_types: vec![],
        input_modalities: Vec::new(),
        tool_result_modalities: Vec::new(),
        default_affinity: 0,
        context_window: 128_000,
        efforts: vec![Effort::Off],
        verbosities: vec![Verbosity::Medium],
        thinking_summaries: vec![ThinkingSummary::Off],
        supports_compaction: false,
        supports_standalone_compaction: false,
        standalone_compaction_threshold: None,
    }
}

fn policy_harness(model_tags: &[&str], role: AgentRole) -> PolicyHarness {
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
        tau_core::SessionPersistenceMode::Durable,
    )
    .expect("harness");
    harness.available_roles = HashMap::from([(ROLE.to_owned(), role)]);
    let model = ModelId::new(ProviderName::new("provider"), ModelName::new("model"));
    harness.provider_model_info = HashMap::from([(model.clone(), model_info(&model, model_tags))]);
    harness.provider_model_routes = HashMap::from([(model.clone(), "provider".into())]);
    harness.selected_role = ROLE.to_owned();
    harness.selected_model = Some(model.clone());
    let group = ToolGroup {
        name: ToolGroupName::new("shell"),
        prompt_fragment: None,
    };
    for spec in [
        tagged_tool("edit", true, &["shell:edit:line"]),
        tagged_tool("apply_patch", false, &["shell:edit:apply_patch"]),
        tagged_tool("shell", true, &["shell:exec:generic"]),
        tagged_tool("gpt_shell", false, &["shell:exec:shell_command"]),
        tagged_tool("read", true, &["shell:read"]),
        tagged_tool(
            "read_image",
            true,
            &["shell:read", "provider-content:image"],
        ),
        tagged_tool("cd", true, &["shell:cd"]),
        tagged_tool("dir_lock", true, &["shell:lock"]),
    ] {
        harness.registry.register_with_prompt_fragment(
            "tools",
            ToolRegister {
                tool: spec,
                tool_group: Some(group.clone()),
                prompt_fragment: None,
            },
        );
    }
    for (name, group, tags) in [
        (
            "compact",
            "compaction",
            &["harness:compaction", "harness:compaction:self"][..],
        ),
        (
            "agent_compact",
            "cross_agent_compaction",
            &[
                "harness:compaction",
                "harness:compaction:cross-agent",
                "harness:agent-control",
            ][..],
        ),
    ] {
        harness.registry.register_with_prompt_fragment(
            "harness",
            ToolRegister {
                tool: tagged_tool(name, false, tags),
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

/// Ensures a role cannot force-enable image-producing tools on a provider route
/// that did not explicitly publish both image-input and image-tool-result
/// support.
#[test]
fn image_tool_requires_exact_route_modalities() {
    let mut policy = policy_harness(&[], AgentRole::default());
    let model = policy
        .harness
        .selected_model
        .clone()
        .expect("selected model");
    let without_image = policy
        .harness
        .gather_effective_tool_specs_for_role_model(ROLE, Some(&model));
    assert!(!without_image.iter().any(|tool| tool.name == "read_image"));

    let model_info = policy
        .harness
        .provider_model_info
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
        "apply_patch",
        "shell",
        "gpt_shell",
        "read",
        "cd",
        "dir_lock",
        "compact",
        "agent_compact",
    ];
    let model = harness.selected_model.as_ref();
    harness
        .gather_effective_tool_specs_for_role_model(ROLE, model)
        .into_iter()
        .map(|spec| spec.name.into_string())
        .filter(|name| relevant.contains(&name.as_str()))
        .collect()
}

/// Compaction capabilities are absent by default and each independent role
/// group exposes only its own tool.
#[test]
fn compaction_groups_are_disabled_and_independent() {
    let default = policy_harness(&[], AgentRole::default());
    let tools = effective_tool_names(&default.harness);
    assert!(!tools.contains(&"compact".to_owned()));
    assert!(!tools.contains(&"agent_compact".to_owned()));

    let self_only = policy_harness(
        &[],
        AgentRole {
            enable_tool_groups: vec![ToolGroupName::new("compaction")],
            ..AgentRole::default()
        },
    );
    let tools = effective_tool_names(&self_only.harness);
    assert!(tools.contains(&"compact".to_owned()));
    assert!(!tools.contains(&"agent_compact".to_owned()));

    let cross_only = policy_harness(
        &[],
        AgentRole {
            enable_tool_groups: vec![ToolGroupName::new("cross_agent_compaction")],
            ..AgentRole::default()
        },
    );
    let tools = effective_tool_names(&cross_only.harness);
    assert!(!tools.contains(&"compact".to_owned()));
    assert!(tools.contains(&"agent_compact".to_owned()));
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
    let prompt_id: tau_proto::AgentPromptId = "prompt-compaction".into();
    policy.harness.prompt_tool_specs.insert(
        prompt_id.clone(),
        vec![tagged_tool(
            "compact",
            false,
            &["harness:compaction", "harness:compaction:self"],
        )],
    );
    policy.harness.available_roles.insert(
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
    policy.harness.registry.register_with_prompt_fragment(
        "tools",
        ToolRegister {
            tool: custom,
            tool_group: None,
            prompt_fragment: None,
        },
    );
    let model = policy
        .harness
        .selected_model
        .clone()
        .expect("selected model");
    let specs = policy
        .harness
        .gather_effective_tool_specs_for_role_model(ROLE, Some(&model));
    assert!(!specs.iter().any(|spec| spec.name.as_str() == "custom_text"));

    policy
        .harness
        .provider_model_info
        .get_mut(&model)
        .expect("model info")
        .supported_tool_types = vec![ToolType::Function, ToolType::Custom];
    let specs = policy
        .harness
        .gather_effective_tool_specs_for_role_model(ROLE, Some(&model));
    assert!(specs.iter().any(|spec| spec.name.as_str() == "custom_text"));
}

/// Ensures untagged models keep generic edit/shell defaults and do not see the
/// ChatGPT-oriented alternatives by implicit promotion.
#[test]
fn generic_model_gets_generic_shell_alternatives() {
    let policy = policy_harness(&[], AgentRole::default());
    let tools = effective_tool_names(&policy.harness);

    assert!(tools.contains(&"edit".to_owned()));
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
    assert!(tools.contains(&"cd".to_owned()));
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

    assert_eq!(tools, vec!["apply_patch".to_owned()]);
}

/// Ensures a keyed user override can disable the built-in ChatGPT shell policy
/// without requiring tools or providers to publish model-specific tags.
#[test]
fn user_can_disable_builtin_chatgpt_shell_policy() {
    let mut policy = policy_harness(&["shell:chatgpt"], AgentRole::default());
    policy
        .harness
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
    policy.harness.tool_policy = serde_json::from_str::<ToolPolicy>(
        r#"{
  "rules": {
    "custom.shell-cwd-only": {
      "disable_tool_tags": ["shell:*"],
      "enable_tool_tags": ["shell:cd"]
    }
  }
}"#,
    )
    .expect("policy parses");

    let tools = effective_tool_names(&policy.harness);

    assert_eq!(tools, vec!["cd".to_owned()]);
}

/// Ensures role-level tag operations run after global policy and before
/// group/tool name operations, preserving broad-to-specific overrides.
#[test]
fn role_tool_tags_run_after_policy_before_groups_and_tools() {
    let role = AgentRole {
        disable_tool_tags: serde_json::from_str(r#"["shell:*"]"#).expect("tag pattern parses"),
        enable_tool_tags: serde_json::from_str(r#"["shell:cd"]"#).expect("tag pattern parses"),
        disable_tool_groups: vec![ToolGroupName::new("shell")],
        enable_tools: vec![ToolName::new("cd")],
        ..AgentRole::default()
    };
    let policy = policy_harness(&["shell:chatgpt"], role);
    let tools = effective_tool_names(&policy.harness);

    assert_eq!(tools, vec!["cd".to_owned()]);
}

/// Ensures prompt-owned tool lookup uses the advertised snapshot rather than
/// mutable current-role policy, while still accepting tools that were
/// advertised in the prompt snapshot. This is the authority boundary recorded
/// by `DESIGN-tau-harness-tool-prompt-snapshots`.
#[test]
fn prompt_snapshot_lookup_is_strict_and_survives_role_changes() {
    let mut policy = policy_harness(&["shell:chatgpt"], AgentRole::default());
    let prompt_id: tau_proto::AgentPromptId = "prompt-1".into();
    policy.harness.prompt_tool_specs.insert(
        prompt_id.clone(),
        vec![tagged_tool("edit", true, &["shell:edit:line"])],
    );
    policy.harness.available_roles.insert(
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
    let prompt_id: tau_proto::AgentPromptId = "prompt-cleanup".into();
    policy.harness.prompt_tool_specs.insert(
        prompt_id.clone(),
        vec![tagged_tool("edit", true, &["shell:edit:line"])],
    );
    policy
        .harness
        .prompt_tool_call_prompts
        .insert("call-1".into(), prompt_id.clone());

    policy.harness.clear_prompt_tool_snapshot(&prompt_id);

    assert!(!policy.harness.prompt_tool_specs.contains_key(&prompt_id));
    assert!(policy.harness.prompt_tool_call_prompts.is_empty());
}
