use super::*;

struct TestRecordedLineHandlers {
    dynamic_consumes: bool,
    outputs: Vec<String>,
}

impl TestRecordedLineHandlers {
    fn new(dynamic_consumes: bool) -> Self {
        Self {
            dynamic_consumes,
            outputs: Vec::new(),
        }
    }
}

impl RecordedLineHandlers for TestRecordedLineHandlers {
    fn handle_known_command(&mut self, _text: &str) -> Result<CommandOutcome, CliError> {
        Ok(CommandOutcome::NotHandled)
    }

    fn handle_dynamic_action(&mut self, text: &str) -> CommandOutcome {
        if self.dynamic_consumes {
            self.outputs.push(format!("dynamic:{text}"));
            CommandOutcome::Continue
        } else {
            CommandOutcome::NotHandled
        }
    }

    fn submit_prompt(&mut self, text: &str) -> Option<InputLoopExit> {
        self.outputs.push(format!("prompt:{text}"));
        None
    }

    fn system_info(&mut self, message: &str) {
        self.outputs.push(format!("notice:{message}"));
    }
}

fn route_line(line: &str, dynamic_consumes: bool) -> Vec<String> {
    let mut handlers = TestRecordedLineHandlers::new(dynamic_consumes);
    handle_recorded_line_with_handlers(line, &mut handlers).expect("line routes");
    handlers.outputs
}

struct TestTreeCommandHandlers {
    outputs: Vec<String>,
}

impl TestTreeCommandHandlers {
    fn new() -> Self {
        Self {
            outputs: Vec::new(),
        }
    }
}

impl RecordedLineHandlers for TestTreeCommandHandlers {
    fn handle_known_command(&mut self, text: &str) -> Result<CommandOutcome, CliError> {
        match tree_command_event("s1", None, text) {
            Ok(Some(event)) => {
                self.outputs.push(format_tree_event(&event));
                Ok(CommandOutcome::Continue)
            }
            Ok(None) => Ok(CommandOutcome::NotHandled),
            Err(message) => {
                self.system_info(message);
                Ok(CommandOutcome::Continue)
            }
        }
    }

    fn handle_dynamic_action(&mut self, _text: &str) -> CommandOutcome {
        CommandOutcome::NotHandled
    }

    fn submit_prompt(&mut self, text: &str) -> Option<InputLoopExit> {
        self.outputs.push(format!("prompt:{text}"));
        None
    }

    fn system_info(&mut self, message: &str) {
        self.outputs.push(format!("notice:{message}"));
    }
}

fn format_tree_event(event: &Event) -> String {
    match event {
        Event::UiTreeRequest(_) => "tree:request".to_owned(),
        Event::UiNavigateTree(req) => format!("tree:navigate:{:?}", req.target),
        other => panic!("expected tree event, got {other:?}"),
    }
}

fn route_tree_line(line: &str) -> Vec<String> {
    let mut handlers = TestTreeCommandHandlers::new();
    handle_recorded_line_with_handlers(line, &mut handlers).expect("line routes");
    handlers.outputs
}

/// Exercises the shared input-loop routing implementation, ensuring unknown
/// leading slash roots become local notices and are not sent to the harness
/// as prompts.
#[test]
fn unknown_leading_slash_action_emits_notice_without_prompt_submission() {
    assert_eq!(
        route_line("/typo arg", false),
        ["notice:unknown CLI action `/typo`"]
    );
}

/// Exercises the interactive input-loop routing for `/tree`: bare numeric
/// arguments are prompt anchors, root is explicit, and raw node ids require the
/// expert `node` keyword.
#[test]
fn tree_command_routes_anchors_root_and_raw_nodes() {
    assert_eq!(route_tree_line("/tree"), ["tree:request"]);
    assert_eq!(
        route_tree_line("/tree 42"),
        ["tree:navigate:PromptAnchor(42)"]
    );
    assert_eq!(route_tree_line("/tree 0"), ["tree:navigate:Root"]);
    assert_eq!(route_tree_line("/tree root"), ["tree:navigate:Root"]);
    assert_eq!(
        route_tree_line("/tree node 42"),
        ["tree:navigate:Node(NodeId(42))"]
    );
}

/// Invalid interactive `/tree` arguments are local validation notices. They
/// must not fall through to prompt submission or the unknown-slash fallback.
#[test]
fn invalid_tree_command_emits_notice_without_prompt_submission() {
    assert_eq!(
        route_tree_line("/tree nope"),
        ["notice:/tree: use a prompt anchor, `root`, or explicit `node <id>`"]
    );
}

/// Protects ordinary prompt text containing a non-leading slash from the
/// unknown-action fallback, because only a leading slash token is
/// command-like.
#[test]
fn non_leading_slash_text_still_submits_as_prompt() {
    assert_eq!(route_line("hello /typo", false), ["prompt:hello /typo"]);
}

/// Ensures dynamic extension-owned slash actions keep precedence over the
/// unknown fallback so known roots are dispatched instead of reported
/// locally.
#[test]
fn dynamic_actions_are_consumed_before_unknown_slash_fallback() {
    assert_eq!(
        route_line("/calendar list", true),
        ["dynamic:/calendar list"]
    );
}

/// Preserves harness-owned skill invocation: the CLI echoes/completes
/// `/skill`, but prompt submission must reach the harness so it can expand
/// the skill.
#[test]
fn skill_slash_actions_still_submit_to_harness_prompt_handling() {
    for line in ["/skill demo args", "/skill:demo args"] {
        assert_eq!(route_line(line, false), [format!("prompt:{line}")]);
    }
}

/// Guards the `/skill` exception from becoming too broad: similar-looking
/// unknown roots should still be local unknown-action notices.
#[test]
fn skillx_remains_unknown_slash_action() {
    assert_eq!(
        route_line("/skillx demo", false),
        ["notice:unknown CLI action `/skillx`"]
    );
}

/// Covers the new-agent half of `/model`: with no selected agent, the command
/// must not emit an agent-update event and must instead stage a one-shot
/// override for the next `UiCreateAgent`.
#[test]
fn model_selection_without_selected_agent_stages_one_shot_create_override() {
    let mut pending = PendingNewAgentModel::default();
    let model: tau_proto::ModelId = "test/staged".parse().expect("model id");

    let event = pending.apply_selection("s1", None, model.clone());

    assert_eq!(event, None);
    assert_eq!(pending.take(), Some(model));
    assert_eq!(pending.take(), None);
}

/// Covers the existing-agent half of `/model`: a selected agent still receives
/// a targeted `UiAgentModelSelect`, and no stale new-agent override is staged.
#[test]
fn model_selection_with_selected_agent_emits_targeted_update() {
    let mut pending = PendingNewAgentModel::default();
    let model: tau_proto::ModelId = "test/selected".parse().expect("model id");
    let agent_id = tau_proto::AgentId::parse("agent-1234567890abcdef").expect("agent id");

    let event = pending
        .apply_selection("s1", Some(agent_id.clone()), model.clone())
        .expect("selected agent event");

    match event {
        Event::UiAgentModelSelect(select) => {
            assert_eq!(select.session_id, "s1");
            assert_eq!(select.target_agent_id, Some(agent_id));
            assert_eq!(select.model, model);
        }
        other => panic!("expected model-select event, got {other:?}"),
    }
    assert_eq!(pending.take(), None);
}

/// Switching to an existing agent should discard a staged new-agent override so
/// an old `/new` + `/model` choice cannot unexpectedly affect a later prompt.
#[test]
fn pending_new_agent_model_clear_discards_staged_override() {
    let mut pending = PendingNewAgentModel::default();
    pending.stage("test/stale".parse().expect("model id"));

    pending.clear();

    assert_eq!(pending.take(), None);
}
