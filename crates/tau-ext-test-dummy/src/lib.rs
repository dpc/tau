//! Test-only Tau extension used by harness integration tests.
//!
//! The extension registers the [`RESTART_TEST_DUMMY_TOOL_NAME`] fixture tool
//! and an `agent.prompt_submitted` interceptor. It deliberately has no user
//! facing production role; its behavior exists to exercise extension
//! supervision, tool dispatch, replay suppression, and prompt interception.
//! See `ARCH-tau-ext-test-dummy` for the fixture boundary and invariants.

use std::error::Error;
use std::io::{Read, Write};
use std::marker::PhantomData;

use rand::Rng;
#[cfg(test)]
use rand::{SeedableRng, rngs::StdRng};
use tau_client::{
    ClientResult, ExtensionBuilder, InterceptDecision, TauExtension, TauExtensionRunner,
};
use tau_proto::{
    AgentPromptSubmitted, Event, EventSelector, HarnessNotice, InterceptionPriority, NoticeLevel,
    ToolError, ToolResult, ToolResultKind, ToolSpec,
};

/// Tool name registered by this fixture extension for restart-supervision
/// tests.
pub const RESTART_TEST_DUMMY_TOOL_NAME: &str = "restart_test_dummy";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum RestartMode {
    /// Preserve the historical random exit-or-error behavior.
    #[default]
    Random,
    /// Emit a successful tool result without restarting the extension.
    Success,
    /// Emit the same tool error as the historical failure branch.
    Error,
    /// Exit without replying to the tool invocation.
    Exit,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ExtConfig {
    /// Test-only deterministic behavior for `restart_test_dummy`.
    restart_mode: Option<RestartMode>,
}

/// Runtime state for the dummy extension.
struct DummyState<T> {
    /// Random source used by the historical restart fixture mode.
    rng: T,
    /// Active deterministic restart behavior selected by config.
    restart_mode: RestartMode,
}

/// Tau-client declaration for the dummy extension.
struct DummyExtension<T> {
    /// Marker carrying the state random-source type.
    _rng: PhantomData<fn() -> T>,
}

impl<T> Default for DummyExtension<T> {
    fn default() -> Self {
        Self { _rng: PhantomData }
    }
}

impl<T> TauExtension for DummyExtension<T>
where
    T: Rng,
{
    type State = DummyState<T>;

    fn name(&self) -> &'static str {
        "tau-ext-test-dummy"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder
            .configure::<ExtConfig>(|cx| {
                cx.state.restart_mode = cx.config().restart_mode.unwrap_or_default();
                Ok(())
            })
            .intercept(
                EventSelector::Exact(tau_proto::EventName::AGENT_PROMPT_SUBMITTED),
                InterceptionPriority::new(0),
                |cx| {
                    let replacement = intercepted_prompt_replacement(cx.event());
                    match replacement {
                        Some(event) => {
                            cx.emit_transient(correction_notice())?;
                            Ok(InterceptDecision::replace(event))
                        }
                        None => Ok(InterceptDecision::Pass),
                    }
                },
            )
            .tool_with_group_and_prompt_fragment(
                restart_tool_spec(),
                Some(tau_proto::ToolGroup {
                    name: tau_proto::ToolGroupName::new("test"),
                    prompt_fragment: None,
                }),
                None,
                |mut cx| handle_restart_invocation(&mut cx),
            )
            .ready_message("test dummy tools ready");
    }
}

/// Returns a copy of `text` with every case-insensitive "tao" word
/// rewritten to "tau", preserving the original casing letter-by-letter
/// (so `Tao` → `Tau`, `TAO` → `TAU`, `tAo` → `tAu`). Returns `None` if
/// no replacement happened so the caller can short-circuit and reply
/// with `Pass(None)` rather than re-publish an identical event.
///
/// Only ASCII letters form word boundaries for this test fixture. `"tao"` is
/// matched as a whole word, not as a free-floating substring — the `tao` inside
/// `taoism` is left alone.
fn correct_tao_to_tau(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut changed = false;
    while i < bytes.len() {
        let is_match = i + 3 <= bytes.len()
            && bytes[i].eq_ignore_ascii_case(&b't')
            && bytes[i + 1].eq_ignore_ascii_case(&b'a')
            && bytes[i + 2].eq_ignore_ascii_case(&b'o')
            && !preceded_by_letter(bytes, i)
            && !followed_by_letter(bytes, i + 3);
        if is_match {
            out.push(bytes[i] as char);
            out.push(bytes[i + 1] as char);
            // Replace 'o'/'O' → 'u'/'U' matching the original case.
            out.push(if bytes[i + 2].is_ascii_uppercase() {
                'U'
            } else {
                'u'
            });
            i += 3;
            changed = true;
        } else {
            // Cheap path for ASCII; fall back to a char step at the
            // current byte boundary to stay UTF-8-safe.
            let ch = text[i..].chars().next().expect("non-empty");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    changed.then_some(out)
}

fn preceded_by_letter(bytes: &[u8], i: usize) -> bool {
    i > 0 && bytes[i - 1].is_ascii_alphabetic()
}

fn followed_by_letter(bytes: &[u8], i: usize) -> bool {
    bytes.get(i).is_some_and(|b| b.is_ascii_alphabetic())
}

/// Runs the dummy extension on standard input and standard output.
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    run(std::io::stdin(), std::io::stdout())
}

/// Runs the dummy extension over the supplied harness protocol streams.
pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write + Send,
{
    run_with_rng(reader, writer, &mut rand::thread_rng())
}

fn run_with_rng<R, W, T>(reader: R, writer: W, rng: &mut T) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write + Send,
    T: Rng,
{
    let state = DummyState {
        rng,
        restart_mode: RestartMode::Random,
    };
    TauExtensionRunner::new(DummyExtension::<&mut T>::default()).run(reader, writer, state)?;
    Ok(())
}

fn restart_tool_spec() -> ToolSpec {
    ToolSpec {
        name: tau_proto::ToolName::new(RESTART_TEST_DUMMY_TOOL_NAME),
        model_visible_name: None,
        description: Some(
            "Test-only tool that restarts the dummy extension, returns an error, or follows configured restart_mode"
                .to_owned(),
        ),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        })),
        format: None,
        tags: Vec::new(),
        enabled_by_default: true,
        background_support: None,
        examples: Vec::new(),
    }
}

fn intercepted_prompt_replacement(event: &Event) -> Option<Event> {
    match event {
        Event::AgentPromptSubmitted(prompt) => correct_tao_to_tau(&prompt.text).map(|fixed| {
            Event::AgentPromptSubmitted(AgentPromptSubmitted {
                inference_activation: false,
                text: fixed,
                ..prompt.clone()
            })
        }),
        _ => None,
    }
}

fn correction_notice() -> Event {
    Event::HarnessNotice(HarnessNotice::new(
        tau_proto::notice_kind::EXTENSION_NOTICE,
        "did you mean \"Tau\"? — corrected for you",
        NoticeLevel::Info,
    ))
}

fn handle_restart_invocation<T>(
    cx: &mut tau_client::ToolContext<'_, DummyState<T>>,
) -> ClientResult<()>
where
    T: Rng,
{
    let invoke = cx.invoke().clone();
    match cx.state.restart_mode {
        RestartMode::Random if cx.state.rng.gen_bool(0.5) => {
            cx.request_stop();
            Ok(())
        }
        RestartMode::Exit => {
            cx.request_stop();
            Ok(())
        }
        RestartMode::Random | RestartMode::Error => cx.report_error(restart_error(invoke)),
        RestartMode::Success => cx.report_result(restart_success(invoke)),
    }
}

fn restart_success(invoke: tau_proto::ToolStarted) -> ToolResult {
    ToolResult {
        call_id: invoke.call_id,
        tool_name: invoke.tool_name,
        tool_type: tau_proto::ToolType::Function,
        result: tau_proto::CborValue::Text("restart succeeded".to_owned()),
        provider_content: Vec::new(),
        kind: ToolResultKind::Final,
        display: None,
        originator: invoke.originator,
    }
}

fn restart_error(invoke: tau_proto::ToolStarted) -> ToolError {
    ToolError {
        call_id: invoke.call_id,
        tool_name: invoke.tool_name,
        tool_type: tau_proto::ToolType::Function,
        message: "restarting failed".to_owned(),
        details: None,
        display: None,
        originator: invoke.originator,
    }
}

#[cfg(test)]
mod tests;
