use std::collections::HashMap;
use std::os::unix::net::UnixStream;

use tau_config::settings::{AgentRole, TauDirs};
use tau_proto::{
    BackgroundSupport, Effort, ModelId, ModelName, ModelTag, ProviderModelInfo, ProviderName,
    ThinkingSummary, ToolGroup, ToolGroupName, ToolName, ToolRegister, ToolSpec, ToolTag, ToolType,
    Verbosity,
};
use tempfile::TempDir;

use super::{Harness, tool_alternative_rank};

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
    }
}

fn model_info(model: &ModelId, tags: &[&str]) -> ProviderModelInfo {
    ProviderModelInfo {
        id: model.clone(),
        display_name: None,
        tags: tags.iter().map(|tag| ModelTag::new(*tag)).collect(),
        default_affinity: 0,
        context_window: 128_000,
        efforts: vec![Effort::Off],
        verbosities: vec![Verbosity::Medium],
        thinking_summaries: vec![ThinkingSummary::Off],
        supports_compaction: false,
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
        tagged_tool("apply_patch", false, &["shell:edit:patch"]),
        tagged_tool("shell", true, &["shell:exec:generic"]),
        tagged_tool("gpt_shell", false, &["shell:exec:command-text"]),
        tagged_tool("read", true, &["shell:read"]),
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
    PolicyHarness {
        harness,
        _temp_dir: temp_dir,
    }
}

fn effective_tool_names(harness: &Harness) -> Vec<String> {
    let relevant = ["edit", "apply_patch", "shell", "gpt_shell", "read"];
    let model = harness.selected_model.as_ref();
    harness
        .gather_effective_tool_specs_for_role_model(ROLE, model)
        .into_iter()
        .map(|spec| spec.name.into_string())
        .filter(|name| relevant.contains(&name.as_str()))
        .collect()
}

/// Ensures the built-in harness-owned policy ranks ChatGPT shell/edit
/// alternatives ahead of generic ones without putting model knowledge in tool
/// registrations.
#[test]
fn chatgpt_policy_prefers_patch_and_compatible_shell_alternatives() {
    let edit = tagged_tool("edit", true, &["shell:edit:line"]);
    let patch = tagged_tool("apply_patch", true, &["shell:edit:patch"]);
    let shell = tagged_tool("shell", true, &["shell:exec:generic"]);
    let gpt_shell = tagged_tool("gpt_shell", true, &["shell:exec:command-text"]);

    assert!(tool_alternative_rank(&patch, true) > tool_alternative_rank(&edit, true));
    assert!(tool_alternative_rank(&gpt_shell, true) > tool_alternative_rank(&shell, true));
    assert!(tool_alternative_rank(&edit, false) > tool_alternative_rank(&patch, false));
    assert!(tool_alternative_rank(&shell, false) > tool_alternative_rank(&gpt_shell, false));
}

/// Ensures untagged models keep generic edit/shell defaults and do not see the
/// ChatGPT-oriented alternatives by implicit promotion.
#[test]
fn generic_model_gets_generic_shell_alternatives() {
    let policy = policy_harness(&[], AgentRole::default());
    let tools = effective_tool_names(&policy.harness);

    assert!(tools.contains(&"edit".to_owned()));
    assert!(tools.contains(&"shell".to_owned()));
    assert!(!tools.contains(&"apply_patch".to_owned()));
    assert!(!tools.contains(&"gpt_shell".to_owned()));
}

/// Ensures a `shell:chatgpt` model gets patch/command-text alternatives while
/// ordinary read tools remain visible.
#[test]
fn chatgpt_model_gets_promoted_shell_alternatives() {
    let policy = policy_harness(&["shell:chatgpt"], AgentRole::default());
    let tools = effective_tool_names(&policy.harness);

    assert!(tools.contains(&"apply_patch".to_owned()));
    assert!(tools.contains(&"gpt_shell".to_owned()));
    assert!(tools.contains(&"read".to_owned()));
    assert!(!tools.contains(&"edit".to_owned()));
    assert!(!tools.contains(&"shell".to_owned()));
}

/// Ensures an explicit `tools` allow-list is treated as the base visible set
/// and does not receive implicit model-tag alternative promotion.
#[test]
fn explicit_tools_allowlist_prevents_implicit_promotion() {
    let role = AgentRole {
        tools: Some(vec![ToolName::new("read")]),
        ..AgentRole::default()
    };
    let policy = policy_harness(&["shell:chatgpt"], role);
    let tools = effective_tool_names(&policy.harness);

    assert_eq!(tools, vec!["read".to_owned()]);
}

/// Ensures `enable_tools` pins fallback tools or explicitly permitted exception
/// alternatives even when the model policy would otherwise prune them.
#[test]
fn enable_tools_pins_fallback_and_exception_tools() {
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

/// Ensures disabling a generic tool suppresses unpinned replacements in the
/// same alternative set, while direct enablement of the replacement remains
/// explicit.
#[test]
fn disable_tools_vetoes_unpinned_alternative_sets() {
    let role = AgentRole {
        disable_tools: vec![ToolName::new("edit"), ToolName::new("shell")],
        ..AgentRole::default()
    };
    let policy = policy_harness(&["shell:chatgpt"], role);
    let tools = effective_tool_names(&policy.harness);

    assert!(!tools.contains(&"edit".to_owned()));
    assert!(!tools.contains(&"apply_patch".to_owned()));
    assert!(!tools.contains(&"shell".to_owned()));
    assert!(!tools.contains(&"gpt_shell".to_owned()));
}

/// Ensures disabling the shell group suppresses unpinned alternative promotion,
/// but an individual `enable_tools` entry can still re-enable one tool.
#[test]
fn disable_tool_groups_suppresses_unpinned_alternatives() {
    let role = AgentRole {
        disable_tool_groups: vec![ToolGroupName::new("shell")],
        enable_tools: vec![ToolName::new("apply_patch")],
        ..AgentRole::default()
    };
    let policy = policy_harness(&["shell:chatgpt"], role);
    let tools = effective_tool_names(&policy.harness);

    assert_eq!(tools, vec!["apply_patch".to_owned()]);
}

/// Ensures prompt-owned tool lookup uses the advertised snapshot rather than
/// mutable current-role policy, while still accepting tools that were
/// advertised in the prompt snapshot.
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
