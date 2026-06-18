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
