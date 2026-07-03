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

struct TestEphemeralCommandHandlers {
    pending: PendingNewAgentOptions,
    has_selected_agent: bool,
    outputs: Vec<String>,
}

impl TestEphemeralCommandHandlers {
    fn new(has_selected_agent: bool) -> Self {
        Self {
            pending: PendingNewAgentOptions::default(),
            has_selected_agent,
            outputs: Vec::new(),
        }
    }
}

impl RecordedLineHandlers for TestEphemeralCommandHandlers {
    fn handle_known_command(&mut self, text: &str) -> Result<CommandOutcome, CliError> {
        let handled = apply_ephemeral_staging_command(
            text,
            self.has_selected_agent,
            &mut self.pending,
            |message| self.outputs.push(format!("notice:{message}")),
        );
        if handled {
            Ok(CommandOutcome::Continue)
        } else {
            Ok(CommandOutcome::NotHandled)
        }
    }

    fn handle_dynamic_action(&mut self, _text: &str) -> CommandOutcome {
        CommandOutcome::NotHandled
    }

    fn submit_prompt(&mut self, text: &str) -> Option<InputLoopExit> {
        let model_override = self.pending.take_model();
        let ephemeral = self.pending.take_ephemeral();
        let event = create_user_agent_prompt(
            "s1",
            "senior-engineer",
            text,
            CreateUserAgentPromptOptions {
                model_override,
                ephemeral,
            },
        );
        match event {
            Event::UiCreateAgent(req) => self.outputs.push(format!(
                "create:ephemeral={} model={:?} prompt={}",
                req.ephemeral,
                req.model_override,
                req.initial_prompt.unwrap_or_default()
            )),
            other => panic!("expected create-agent prompt, got {other:?}"),
        }
        None
    }

    fn system_info(&mut self, message: &str) {
        self.outputs.push(format!("notice:{message}"));
    }
}

fn route_ephemeral_lines(
    lines: &[&str],
    has_selected_agent: bool,
    setup: impl FnOnce(&mut PendingNewAgentOptions),
) -> Vec<String> {
    let mut handlers = TestEphemeralCommandHandlers::new(has_selected_agent);
    setup(&mut handlers.pending);
    for line in lines {
        handle_recorded_line_with_handlers(line, &mut handlers).expect("line routes");
    }
    handlers.outputs
}

struct TestNewRoleCommandHandlers {
    pending: PendingNewAgentOptions,
    selected_agent: Option<String>,
    current_role: String,
    outputs: Vec<String>,
}

impl TestNewRoleCommandHandlers {
    fn new() -> Self {
        Self {
            pending: PendingNewAgentOptions::default(),
            selected_agent: Some("agent-1".to_owned()),
            current_role: "senior-engineer".to_owned(),
            outputs: Vec::new(),
        }
    }
}

impl RecordedLineHandlers for TestNewRoleCommandHandlers {
    fn handle_known_command(&mut self, text: &str) -> Result<CommandOutcome, CliError> {
        if text == "/new" || text.starts_with("/new ") {
            match new_alias_command_effect(text) {
                NewAliasCommandEffect::StartNewAgent { role } => {
                    if let Some(role) = role {
                        self.pending.stage_role(role);
                        self.outputs.push(format!("role-select:{role}"));
                    } else {
                        self.pending.clear_role();
                    }
                    self.selected_agent = None;
                    self.outputs.push("clear-selected".to_owned());
                }
                NewAliasCommandEffect::Usage(usage) => self.outputs.push(format!("notice:{usage}")),
            }
            return Ok(CommandOutcome::Continue);
        }
        if text == "/role" || text.starts_with("/role ") {
            let rest = text.strip_prefix("/role").unwrap_or("").trim();
            match crate::ui_commands::parse_role_command(rest) {
                Ok(Some(event @ Event::UiRoleSelect(_))) => {
                    if let Event::UiRoleSelect(select) = &event {
                        self.current_role = select.role.clone();
                        self.outputs.push(format!("role-select:{}", select.role));
                    }
                    stage_role_selection_for_new_agent(
                        &mut self.pending,
                        self.selected_agent.is_some(),
                        &event,
                    );
                }
                Ok(Some(_)) => self.outputs.push("role-update".to_owned()),
                Ok(None) => self.outputs.push("notice:/role <role>".to_owned()),
                Err(error) => self.outputs.push(format!("notice:{error}")),
            }
            return Ok(CommandOutcome::Continue);
        }
        if text == "/agent new" || text.starts_with("/agent new ") {
            let rest = text.strip_prefix("/agent new").unwrap_or("").trim();
            if rest.is_empty() {
                self.pending.clear_role();
                self.selected_agent = None;
                self.outputs.push("clear-selected".to_owned());
            } else {
                self.outputs.push("notice:/agent new".to_owned());
            }
            return Ok(CommandOutcome::Continue);
        }
        Ok(CommandOutcome::NotHandled)
    }

    fn handle_dynamic_action(&mut self, _text: &str) -> CommandOutcome {
        CommandOutcome::NotHandled
    }

    fn submit_prompt(&mut self, text: &str) -> Option<InputLoopExit> {
        if let Some(agent_id) = &self.selected_agent {
            self.outputs.push(format!("prompt:{agent_id}:{text}"));
            return None;
        }
        let role = take_new_agent_role(&mut self.pending, self.current_role.clone());
        let model_override = self.pending.take_model();
        let ephemeral = self.pending.take_ephemeral();
        let event = create_user_agent_prompt(
            "s1",
            role,
            text,
            CreateUserAgentPromptOptions {
                model_override,
                ephemeral,
            },
        );
        match event {
            Event::UiCreateAgent(req) => self.outputs.push(format!(
                "create:role={} ephemeral={} model={:?} prompt={}",
                req.role,
                req.ephemeral,
                req.model_override.as_ref().map(ToString::to_string),
                req.initial_prompt.unwrap_or_default()
            )),
            other => panic!("expected create-agent prompt, got {other:?}"),
        }
        None
    }

    fn system_info(&mut self, message: &str) {
        self.outputs.push(format!("notice:{message}"));
    }
}

fn route_new_role_lines(
    lines: &[&str],
    setup: impl FnOnce(&mut TestNewRoleCommandHandlers),
) -> TestNewRoleCommandHandlers {
    let mut handlers = TestNewRoleCommandHandlers::new();
    setup(&mut handlers);
    for line in lines {
        handle_recorded_line_with_handlers(line, &mut handlers).expect("line routes");
    }
    handlers
}

/// `/new <role>` is intentionally a single optional role token, not a prompt
/// rest string, so malformed invocations must produce usage before the input
/// loop clears the selected agent or stages a role.
#[test]
fn new_alias_parses_optional_single_role() {
    assert_eq!(new_alias_role("/new").expect("bare new parses"), None);
    assert_eq!(
        new_alias_role("/new reviewer").expect("role new parses"),
        Some("reviewer")
    );
    assert_eq!(new_alias_role("/new reviewer extra"), Err("/new [role]"));
}

/// The new-agent role override is one-shot so `/new reviewer` can select the
/// requested role for an immediate prompt without leaking stale role choices
/// into later agents after that prompt has been submitted.
#[test]
fn pending_new_agent_role_is_one_shot() {
    let mut pending = PendingNewAgentOptions::default();

    pending.stage_role("reviewer");

    assert_eq!(pending.take_role().as_deref(), Some("reviewer"));
    assert_eq!(pending.take_role(), None);

    pending.stage_role("unknown");
    pending.clear_role();
    assert_eq!(pending.take_role(), None);
}

/// The routed `/new <role>` path must both switch the UI into no-agent mode and
/// carry the requested role into the immediate next create-agent prompt, rather
/// than only exercising the small parser and pending-storage helpers.
#[test]
fn new_role_command_selects_role_clears_agent_and_creates_with_role() {
    let handlers = route_new_role_lines(&["/new reviewer", "please review"], |_| {});

    assert_eq!(
        handlers.outputs,
        [
            "role-select:reviewer",
            "clear-selected",
            "create:role=reviewer ephemeral=false model=None prompt=please review",
        ]
    );
}

/// A later no-agent `/role <role>` command is a stronger expression of intent
/// than the latency-bridging role staged by `/new <role>`, so the next prompt
/// must use the later selection.
#[test]
fn role_command_after_new_role_supersedes_pending_new_role() {
    let handlers = route_new_role_lines(
        &["/new reviewer", "/role engineer", "please implement"],
        |_| {},
    );

    assert_eq!(
        handlers.outputs,
        [
            "role-select:reviewer",
            "clear-selected",
            "role-select:engineer",
            "create:role=engineer ephemeral=false model=None prompt=please implement",
        ]
    );
}

/// Role cycling is also a no-agent role selection path. The selected cycled
/// role should replace the `/new <role>` bridge so an immediate prompt cannot
/// race the asynchronous harness role-selected echo.
#[test]
fn role_cycle_after_new_role_supersedes_pending_new_role() {
    let writer = Arc::new(Mutex::new(UiWriter::new(Vec::new(), UiIoMeter::default())));
    let current_role_state = Arc::new(Mutex::new(Some("reviewer".to_owned())));
    let roles_available = Arc::new(Mutex::new(vec![
        "reviewer".to_owned(),
        "engineer".to_owned(),
    ]));
    let mut pending = PendingNewAgentOptions::default();
    pending.stage_role("reviewer");

    if let Some(role) = cycle_role(&writer, &current_role_state, &roles_available, &|message| {
        panic!("unexpected cycle notice: {message}")
    }) {
        pending.stage_role(role);
    }
    let event = create_user_agent_prompt(
        "s1",
        pending
            .take_role()
            .unwrap_or_else(|| "senior-engineer".to_owned()),
        "please implement",
        CreateUserAgentPromptOptions::default(),
    );

    let Event::UiCreateAgent(req) = event else {
        panic!("expected create-agent prompt");
    };
    assert_eq!(req.role, "engineer");
}

/// Invalid `/new` arguments should be a harmless usage notice: they must not
/// clear the selected agent and must not mutate already staged one-shot
/// options.
#[test]
fn invalid_new_role_command_does_not_clear_or_mutate_pending_options() {
    let mut handlers = route_new_role_lines(&["/new reviewer extra", "hello selected"], |h| {
        h.pending.stage_role("staged");
        h.pending
            .stage_model("test/model".parse().expect("model id"));
        h.pending.set_ephemeral(true);
    });

    assert_eq!(
        handlers.outputs,
        ["notice:/new [role]", "prompt:agent-1:hello selected"]
    );
    assert_eq!(handlers.selected_agent.as_deref(), Some("agent-1"));
    assert_eq!(handlers.pending.take_role().as_deref(), Some("staged"));
    assert_eq!(
        handlers.pending.take_model().map(|model| model.to_string()),
        Some("test/model".to_owned())
    );
    assert!(handlers.pending.take_ephemeral());
}

/// The optional role argument belongs to the short `/new [role]` command. Keep
/// `/agent new <arg>` rejected unless the long form gains matching completion
/// and documentation.
#[test]
fn agent_new_with_role_argument_stays_rejected() {
    let mut handlers = route_new_role_lines(&["/agent new reviewer", "hello selected"], |h| {
        h.pending.stage_role("staged");
    });

    assert_eq!(
        handlers.outputs,
        ["notice:/agent new", "prompt:agent-1:hello selected"]
    );
    assert_eq!(handlers.selected_agent.as_deref(), Some("agent-1"));
    assert_eq!(handlers.pending.take_role().as_deref(), Some("staged"));
}

/// Bare `/new` intentionally drops only a stale pending role. Model and
/// ephemeral staging remain available for users who set them around the
/// no-agent transition.
#[test]
fn bare_new_clears_pending_role_but_preserves_model_and_ephemeral() {
    let handlers = route_new_role_lines(&["/new", "secret prompt"], |h| {
        h.pending.stage_role("stale-reviewer");
        h.pending
            .stage_model("test/model".parse().expect("model id"));
        h.pending.set_ephemeral(true);
        h.current_role = "engineer".to_owned();
    });

    assert_eq!(
        handlers.outputs,
        [
            "clear-selected",
            "create:role=engineer ephemeral=true model=Some(\"test/model\") prompt=secret prompt",
        ]
    );
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
    let mut pending = PendingNewAgentOptions::default();
    let model: tau_proto::ModelId = "test/staged".parse().expect("model id");

    let event = pending.apply_model_selection("s1", None, model.clone());

    assert_eq!(event, None);
    assert_eq!(pending.take_model(), Some(model));
    assert_eq!(pending.take_model(), None);
}

/// Covers the existing-agent half of `/model`: a selected agent still receives
/// a targeted `UiAgentModelSelect`, and no stale new-agent override is staged.
#[test]
fn model_selection_with_selected_agent_emits_targeted_update() {
    let mut pending = PendingNewAgentOptions::default();
    let model: tau_proto::ModelId = "test/selected".parse().expect("model id");
    let agent_id = tau_proto::AgentId::parse("agent-1234567890abcdef").expect("agent id");

    let event = pending
        .apply_model_selection("s1", Some(agent_id.clone()), model.clone())
        .expect("selected agent event");

    match event {
        Event::UiAgentModelSelect(select) => {
            assert_eq!(select.session_id, "s1");
            assert_eq!(select.target_agent_id, Some(agent_id));
            assert_eq!(select.model, model);
        }
        other => panic!("expected model-select event, got {other:?}"),
    }
    assert_eq!(pending.take_model(), None);
}

/// Exercises the selected-agent `/name` command route: it must resolve the
/// current agent, keep the display name as a rest string, and emit the same
/// display-name event shape as the explicit `/agent name` path.
#[test]
fn name_alias_with_selected_agent_emits_display_name_update() {
    let request = name_alias_request("/name Current worker", Some("worker-1".to_owned()), |id| {
        id == "worker-1"
    })
    .expect("selected agent request");

    let event = request.event("session-1");

    let Event::UiSetAgentDisplayName(update) = event else {
        panic!("expected display name event");
    };
    assert_eq!(update.session_id.as_str(), "session-1");
    assert_eq!(update.agent_id.as_str(), "worker-1");
    assert_eq!(update.display_name, "Current worker");
}

/// Covers the no-current-agent `/name` validation branch so users get a local
/// notice with both available recovery paths instead of a silently dropped or
/// misrouted prompt.
#[test]
fn name_alias_without_selected_agent_reports_fallbacks() {
    let error = name_alias_request("/name Current worker", None, |_| true)
        .expect_err("missing selected agent should be rejected");

    assert_eq!(
        error,
        "/name requires a selected agent; use /agent switch <agent_id> or /agent name <agent_id> <display_name>"
    );
}

/// `/ephemeral` is a local staging command for the next new agent, so the
/// generic slash fallback must not submit it to the harness as prompt text.
#[test]
fn ephemeral_slash_command_is_local() {
    assert!(is_local_slash_command("/ephemeral"));
    assert!(is_local_slash_command("/ephemeral on"));
    assert!(!is_local_slash_command("/ephemeralx"));
}

/// Exercises the recorded-line command path for `/new` + `/ephemeral on`: the
/// local staging command is consumed, and the next prompt carries a one-shot
/// memory-only create-agent request.
#[test]
fn ephemeral_command_stages_one_shot_new_agent_option() {
    let outputs = route_ephemeral_lines(
        &["/ephemeral on", "secret prompt", "next prompt"],
        false,
        |_| {},
    );

    assert!(
        outputs
            .iter()
            .any(|line| line.contains("next agent will be ephemeral")),
        "expected local staging notice, got {outputs:?}"
    );
    assert!(
        outputs
            .iter()
            .any(|line| line.contains("create:ephemeral=true")
                && line.contains("prompt=secret prompt")),
        "first prompt should create an ephemeral agent, got {outputs:?}"
    );
    assert!(
        outputs
            .iter()
            .any(|line| line.contains("create:ephemeral=false")
                && line.contains("prompt=next prompt")),
        "ephemeral staging should be consumed after one prompt, got {outputs:?}"
    );
}

/// `/model` and `/ephemeral` stage independent fields for the same next
/// `ui.create_agent`; consuming one prompt clears both one-shot options.
#[test]
fn ephemeral_and_model_staging_compose_for_next_agent() {
    let model: tau_proto::ModelId = "test/composed".parse().expect("model id");
    let outputs = route_ephemeral_lines(&["/ephemeral on", "with model"], false, |pending| {
        assert_eq!(pending.apply_model_selection("s1", None, model), None);
    });

    assert!(
        outputs
            .iter()
            .any(|line| line.contains("create:ephemeral=true")
                && line.contains("ProviderName(\"test\")")
                && line.contains("ModelName(\"composed\")")
                && line.contains("prompt=with model")),
        "next create-agent should include both staged fields, got {outputs:?}"
    );
}

/// `/ephemeral off` explicitly clears a staged memory-only flag instead of
/// forcing callers to rely on the bare-toggle form.
#[test]
fn ephemeral_off_clears_staged_new_agent_option() {
    let outputs = route_ephemeral_lines(&["/ephemeral off", "durable prompt"], false, |pending| {
        pending.set_ephemeral(true);
    });

    assert!(
        outputs
            .iter()
            .any(|line| line.contains("create:ephemeral=false")
                && line.contains("prompt=durable prompt")),
        "prompt should create a durable agent after /ephemeral off, got {outputs:?}"
    );
}

/// `/ephemeral` is only a new-agent staging command. When an existing agent is
/// selected, the command must be rejected rather than converting it in place.
#[test]
fn ephemeral_command_rejects_existing_agent_selection() {
    let outputs = route_ephemeral_lines(&["/ephemeral on"], true, |_| {});

    assert_eq!(
        outputs,
        ["notice:Use /new first; /ephemeral controls only the next new agent."]
    );
}

/// Prompt-history routing treats staged ephemeral creation and selected
/// ephemeral agents (including shell shortcuts) as memory-only prompt lines.
#[test]
fn prompt_history_routing_skips_ephemeral_agent_lines() {
    assert!(prompt_line_targets_ephemeral_agent_state(
        "create secret agent",
        false,
        false,
        true,
        false,
    ));
    assert!(prompt_line_targets_ephemeral_agent_state(
        "!!", true, true, false, false,
    ));
    assert!(prompt_line_targets_ephemeral_agent_state(
        "!pwd", true, true, false, false,
    ));
    assert!(!prompt_line_targets_ephemeral_agent_state(
        "/ephemeral on",
        false,
        false,
        true,
        true,
    ));
}

/// Switching to an existing agent should discard a staged new-agent override so
/// an old `/new` + `/model` choice cannot unexpectedly affect a later prompt.
#[test]
fn pending_new_agent_model_clear_discards_staged_override() {
    let mut pending = PendingNewAgentOptions::default();
    pending.stage_model("test/stale".parse().expect("model id"));

    pending.clear();

    assert_eq!(pending.take_model(), None);
}
