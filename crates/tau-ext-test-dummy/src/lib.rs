//! Test-only Tau extension used by harness integration tests.
//!
//! The extension registers the [`RESTART_TEST_DUMMY_TOOL_NAME`] fixture tool
//! and an `agent.prompt_submitted` interceptor. It deliberately has no user
//! facing production role; its behavior exists to exercise extension
//! supervision, tool dispatch, replay suppression, and prompt interception.

use std::error::Error;
use std::io::{BufReader, BufWriter, Read, Write};

use rand::Rng;
#[cfg(test)]
use rand::{SeedableRng, rngs::StdRng};
use tau_proto::{
    AgentPromptSubmitted, ConfigError, Emit, Event, EventSelector, HarnessInputMessage,
    HarnessNotice, HarnessOutputMessage, InterceptAction, InterceptReply, InterceptionPriority,
    NoticeLevel, PeerInputReader, PeerOutputWriter, ToolError, ToolResult, ToolResultKind,
    ToolSpec,
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
    W: Write,
{
    run_with_rng(reader, writer, &mut rand::thread_rng())
}

fn run_with_rng<R, W, T>(reader: R, writer: W, rng: &mut T) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write,
    T: Rng + ?Sized,
{
    let mut reader = PeerInputReader::new(BufReader::new(reader));
    let mut writer = PeerOutputWriter::new(BufWriter::new(writer));

    // Subscribe only to fresh live invoke-start events. Extension
    // subscriptions can receive replayed durable catch-up, but `tool.started`
    // is a runtime-only event, so old invokes are not replayed.
    tau_extension::Handshake::tool("tau-ext-test-dummy")
        .subscribe([tau_proto::EventName::TOOL_STARTED])
        .intercept(
            EventSelector::Exact(tau_proto::EventName::AGENT_PROMPT_SUBMITTED),
            InterceptionPriority::new(0),
        )
        .register_tool_with_group_and_prompt_fragment(
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
            },
            Some(tau_proto::ToolGroup {
                name: tau_proto::ToolGroupName::new("test"),
                prompt_fragment: None,
            }),
            None,
        )
        .ready_message("test dummy tools ready")
        .run(&mut writer)?;

    let mut restart_mode = RestartMode::Random;

    while let Some(message) = reader.read_message()? {
        match message {
            HarnessOutputMessage::InterceptRequest(req) => {
                let mutated = match req.event.as_ref() {
                    Event::AgentPromptSubmitted(prompt) => {
                        correct_tao_to_tau(&prompt.text).map(|fixed| {
                            Event::AgentPromptSubmitted(AgentPromptSubmitted {
                                text: fixed,
                                ..prompt.clone()
                            })
                        })
                    }
                    _ => None,
                };
                if mutated.is_some() {
                    writer.write_message(&HarnessInputMessage::Emit(Emit {
                        event: Box::new(Event::HarnessNotice(HarnessNotice::new(
                            tau_proto::notice_kind::EXTENSION_NOTICE,
                            "did you mean \"Tau\"? — corrected for you",
                            NoticeLevel::Info,
                        ))),
                        transient: true,
                    }))?;
                }
                let action = match mutated {
                    Some(event) => InterceptAction::Pass(Some(Box::new(event))),
                    None => InterceptAction::Pass(None),
                };
                writer.write_message(&HarnessInputMessage::InterceptReply(InterceptReply {
                    action,
                }))?;
                writer.flush()?;
            }
            HarnessOutputMessage::Configure(msg) => {
                match tau_extension::parse_config::<ExtConfig>(&msg.config) {
                    Ok(config) => restart_mode = config.restart_mode.unwrap_or_default(),
                    Err(message) => {
                        writer.write_message(&HarnessInputMessage::ConfigError(ConfigError {
                            message,
                        }))?;
                        writer.flush()?;
                    }
                }
            }
            HarnessOutputMessage::Deliver(delivery) => {
                // Tool invocations are execution triggers; replay-marked
                // frames re-send history and must not re-run them.
                if delivery.is_replay() {
                    continue;
                }
                if let Event::ToolStarted(invoke) = delivery.into_event()
                    && invoke.tool_name == RESTART_TEST_DUMMY_TOOL_NAME
                {
                    match restart_mode {
                        RestartMode::Random if rng.gen_bool(0.5) => {
                            writer.flush()?;
                            return Ok(());
                        }
                        RestartMode::Random | RestartMode::Error => {
                            writer.write_message(&restart_error(invoke))?;
                            writer.flush()?;
                        }
                        RestartMode::Success => {
                            writer.write_message(&restart_success(invoke))?;
                            writer.flush()?;
                        }
                        RestartMode::Exit => {
                            writer.flush()?;
                            return Ok(());
                        }
                    }
                }
            }
            HarnessOutputMessage::Disconnect(_) => break,
            _ => {}
        }
    }

    Ok(())
}

fn restart_success(invoke: tau_proto::ToolStarted) -> HarnessInputMessage {
    HarnessInputMessage::emit(Event::ToolResult(ToolResult {
        call_id: invoke.call_id,
        tool_name: invoke.tool_name,
        tool_type: tau_proto::ToolType::Function,
        result: tau_proto::CborValue::Text("restart succeeded".to_owned()),
        kind: ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }))
}

fn restart_error(invoke: tau_proto::ToolStarted) -> HarnessInputMessage {
    HarnessInputMessage::emit(Event::ToolError(ToolError {
        call_id: invoke.call_id,
        tool_name: invoke.tool_name,
        tool_type: tau_proto::ToolType::Function,
        message: "restarting failed".to_owned(),
        details: None,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }))
}

#[cfg(test)]
mod tests;
