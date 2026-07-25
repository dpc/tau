//! Feature-gated causal quota-recovery provider fixture.

use std::collections::VecDeque;
use std::io::{BufReader, Cursor, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use tau_proto::{
    CborValue, ContextItem, Event, HarnessInputMessage, HarnessInputReader, HarnessOutputMessage,
    HarnessOutputWriter, ProviderResponseFinished, ProviderStopReason, ToolCallItem, ToolName,
    ToolType, UiRetryPrompt,
};
use tau_provider::retry_policy::RetryClass;

use super::*;

/// Runs a self-contained provider whose typed usage-window failure is manually
/// retried before its tool call is continued by the real harness.
///
/// This entrypoint exists only behind `quota-test-support`; production builds
/// neither expose nor compile the fixture.
pub fn run_quota_recovery_fixture(reader: UnixStream, writer: UnixStream) -> Result<(), String> {
    let input = FixtureInput::default();
    let pump_input = input.clone();
    let pump_shutdown = reader
        .try_clone()
        .map_err(|error| format!("clone fixture input: {error}"))?;
    let pump = thread::spawn(move || pump_harness_input(reader, pump_input));
    let mut pump = PumpGuard::new(pump_shutdown, pump);
    let observation = Arc::new(Mutex::new(FixtureObservation::default()));
    let executor: PromptExecutor = Arc::new(move |execution| {
        let prompt = &execution.job.prompt;
        let is_continuation = prompt
            .context
            .flatten()
            .iter()
            .any(|item| matches!(item, ContextItem::ToolResult(_)));
        let attempt = execution.job.retry_state.attempts;
        if attempt == 0 && !is_continuation {
            let PromptBackend::Responses(_config) = &execution.job.backend else {
                panic!("fixture requires Responses backend");
            };
            let decision = RetryDecision::new(RetryClass::UsageWindow)
                .with_retry_after(Some(Duration::from_secs(432_000)));
            assert_eq!(decision.class, RetryClass::UsageWindow);
            assert_eq!(decision.retry_after, Some(Duration::from_secs(432_000)));
            send_worker_message(
                &execution.output_tx,
                &execution.output_waker,
                WorkerMessage::Retry {
                    job: execution.job,
                    decision,
                },
            )
            .expect("park fixture probe");
            return;
        }

        let output_items = if is_continuation {
            vec![ContextItem::Message(tau_proto::MessageItem {
                role: tau_proto::ContextRole::Assistant,
                content: vec![tau_proto::ContentPart::Text {
                    text: "quota fixture complete".to_owned(),
                }],
                phase: None,
                responses_raw_json: None,
            })]
        } else {
            vec![ContextItem::ToolCall(ToolCallItem {
                call_id: "quota-fixture-call".into(),
                name: ToolName::new("echo"),
                tool_type: ToolType::Function,
                arguments: CborValue::Text("deterministic fixture result".to_owned()),
                raw_arguments_json: Some("\"deterministic fixture result\"".to_owned()),
                responses_envelope: None,
            })]
        };
        let finished = ProviderResponseFinished {
            estimated_api_cost_rates: None,
            estimated_api_cost_increment: None,

            agent_prompt_id: execution.job.agent_prompt_id.clone(),
            agent_id: prompt.agent_id.clone(),
            output_items,
            stop_reason: if is_continuation {
                ProviderStopReason::EndTurn
            } else {
                ProviderStopReason::ToolCalls
            },
            error: None,
            failure_kind: None,
            context_limit_telemetry: None,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            originator: prompt.originator.clone(),
            usage: None,
            compaction_original_input_tokens: None,
            compaction_compacted_input_tokens: None,
            backend: None,
            provider_response_id: None,
            ws_pool_delta: None,
        };
        let mut frame_writer = execution.frame_writer();
        frame_writer
            .write_message(&HarnessInputMessage::emit_transient(
                Event::ProviderResponseFinishedReported(finished),
            ))
            .expect("write fixture terminal");
        frame_writer.flush().expect("flush fixture terminal");
    });

    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        ProviderName::new(CHATGPT_PROVIDER_NAME),
        BuiltinProviderProfile::Chatgpt(ChatGptProfile {
            auth: OpenAiAuth {
                access_token: "fixture-access".to_owned(),
                refresh_token: String::new(),
                expires_at_ms: u64::MAX,
                account_id: Some("fixture-account".to_owned()),
            },
            responses_lite_compatibility: false,
        }),
    );
    let profiles = BuiltinProviderProfiles { providers };
    let reload_profiles = profiles.clone();
    let result = run_inner_with_prompt_executor(
        input.clone(),
        RetryInjectingWriter::new(writer, input, Arc::clone(&observation)),
        profiles,
        move || reload_profiles.clone(),
        2,
        executor,
    )
    .map_err(|error| error.to_string());
    let pump_result = pump.stop();
    result?;
    pump_result?;
    let observation = observation.lock().expect("fixture observation");
    if observation.parked != 1 || observation.injected != 1 || observation.accepted_retry != 1 {
        return Err(format!(
            "unexpected retry lifecycle: parked={} injected={} accepted={}",
            observation.parked, observation.injected, observation.accepted_retry
        ));
    }
    Ok(())
}

/// Exact retry lifecycle observed at the provider wire boundary.
#[derive(Default)]
struct FixtureObservation {
    /// Number of parked retry status frames.
    parked: usize,
    /// Number of manual controls injected.
    injected: usize,
    /// Number of matching accepted retry results.
    accepted_retry: usize,
}

/// Owns the harness-input pump and can unblock its Unix read on every exit.
struct PumpGuard {
    /// Clone used solely to shut down a blocked read.
    shutdown: UnixStream,
    /// Pump thread, consumed exactly once.
    handle: Option<thread::JoinHandle<Result<(), String>>>,
}

impl PumpGuard {
    /// Creates an owned input pump.
    fn new(shutdown: UnixStream, handle: thread::JoinHandle<Result<(), String>>) -> Self {
        Self {
            shutdown,
            handle: Some(handle),
        }
    }

    /// Unblocks and joins the pump.
    fn stop(&mut self) -> Result<(), String> {
        let _ = self.shutdown.shutdown(Shutdown::Both);
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        handle
            .join()
            .map_err(|_| "quota fixture input pump panicked".to_owned())?
    }
}

impl Drop for PumpGuard {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Shared byte queue combining harness traffic with one injected `:retry`.
#[derive(Clone, Default)]
struct FixtureInput {
    /// Buffered bytes, upstream EOF state, and reader wakeup.
    state: Arc<(Mutex<FixtureInputState>, Condvar)>,
}

/// Mutable input queue state.
#[derive(Default)]
struct FixtureInputState {
    /// Bytes waiting for the provider decoder.
    bytes: VecDeque<u8>,
    /// Whether the harness input stream reached EOF.
    closed: bool,
}

impl FixtureInput {
    /// Appends arbitrary raw bytes from harness traffic or an injected frame.
    fn push(&self, bytes: impl IntoIterator<Item = u8>) {
        let (lock, wake) = &*self.state;
        lock.lock().expect("fixture input lock").bytes.extend(bytes);
        wake.notify_all();
    }

    /// Marks the harness input stream closed.
    fn close(&self) {
        let (lock, wake) = &*self.state;
        lock.lock().expect("fixture input lock").closed = true;
        wake.notify_all();
    }
}

impl Read for FixtureInput {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let (lock, wake) = &*self.state;
        let mut state = lock.lock().expect("fixture input lock");
        loop {
            if !state.bytes.is_empty() {
                let count = buffer.len().min(state.bytes.len());
                for destination in &mut buffer[..count] {
                    *destination = state.bytes.pop_front().expect("queued byte");
                }
                return Ok(count);
            }
            if state.closed {
                return Ok(0);
            }
            state = wake.wait(state).expect("fixture input wait");
        }
    }
}

/// Copies raw harness output into the provider's multiplexed input.
fn pump_harness_input(mut reader: impl Read, input: FixtureInput) -> Result<(), String> {
    let mut buffer = [0_u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => {
                input.close();
                return Ok(());
            }
            Ok(count) => input.push(buffer[..count].iter().copied()),
            Err(error) => {
                input.close();
                return Err(error.to_string());
            }
        }
    }
}

/// Provider writer that injects one retry after observing parked status.
struct RetryInjectingWriter<W> {
    /// Real harness-bound provider writer.
    inner: W,
    /// Provider bytes retained until complete frames can be decoded.
    observed: Vec<u8>,
    /// Multiplexed input used for the manual control event.
    input: FixtureInput,
    /// Whether the one allowed manual retry was injected.
    injected: bool,
    /// Shared exact lifecycle counters.
    observation: Arc<Mutex<FixtureObservation>>,
}

impl<W> RetryInjectingWriter<W> {
    /// Wraps the real provider output.
    fn new(inner: W, input: FixtureInput, observation: Arc<Mutex<FixtureObservation>>) -> Self {
        Self {
            inner,
            observed: Vec::new(),
            input,
            injected: false,
            observation,
        }
    }
}

impl<W: Write> Write for RetryInjectingWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.inner.write_all(bytes)?;
        self.observed.extend_from_slice(bytes);
        let frames = decoded_provider_frames(&self.observed);
        if let Some(frames) = &frames {
            let parked = frames
                .iter()
                .filter(|frame| matches!(
                    frame,
                    HarnessInputMessage::Emit(emit)
                        if matches!(
                            emit.event.as_ref(),
                            Event::ProviderResponseUpdatedReported(update)
                                if update.status.as_ref().is_some_and(|status| status.retry.is_some())
                        )
                ))
                .count();
            let accepted_retry = frames
                .iter()
                .filter(|frame| {
                    matches!(
                        frame,
                        HarnessInputMessage::Emit(emit)
                            if matches!(
                                emit.event.as_ref(),
                                Event::ProviderRetryPromptResultReported(result)
                                    if result.request_id.as_str() == "quota-fixture-retry"
                                        && result.status == tau_proto::RetryPromptStatus::Accepted
                            )
                    )
                })
                .count();
            let mut observation = self.observation.lock().expect("fixture observation");
            observation.parked = parked;
            observation.accepted_retry = accepted_retry;
        }
        let parked_prompt = (!self.injected)
            .then_some(frames)
            .flatten()
            .and_then(|frames| {
                frames.into_iter().find_map(|frame| match frame {
                    HarnessInputMessage::Emit(emit) => match emit.event.as_ref() {
                        Event::ProviderResponseUpdatedReported(update)
                            if update
                                .status
                                .as_ref()
                                .is_some_and(|status| status.retry.is_some()) =>
                        {
                            Some(update.agent_prompt_id.clone())
                        }
                        _ => None,
                    },
                    _ => None,
                })
            });
        if let Some(agent_prompt_id) = parked_prompt {
            self.injected = true;
            self.observation
                .lock()
                .expect("fixture observation")
                .injected += 1;
            self.input
                .push(encode_harness_output(&HarnessOutputMessage::deliver_live(
                    tau_proto::UnixMicros::new(1),
                    Event::UiRetryPrompt(UiRetryPrompt {
                        request_id: tau_proto::RetryPromptRequestId::parse("quota-fixture-retry")
                            .expect("fixture retry id"),
                        session_id: "s1".into(),
                        target_agent_id: None,
                        agent_prompt_id: Some(agent_prompt_id),
                    }),
                )));
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Decodes complete provider frames, returning `None` while a write is partial.
fn decoded_provider_frames(bytes: &[u8]) -> Option<Vec<HarnessInputMessage>> {
    let mut reader = HarnessInputReader::new(BufReader::new(Cursor::new(bytes)));
    let mut frames = Vec::new();
    loop {
        match reader.read_message() {
            Ok(Some(frame)) => frames.push(frame),
            Ok(None) => return Some(frames),
            Err(_) => return None,
        }
    }
}

/// Encodes one harness-to-provider frame for multiplexed injection.
fn encode_harness_output(frame: &HarnessOutputMessage) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut writer = HarnessOutputWriter::new(&mut bytes);
    writer.write_message(frame).expect("encode fixture retry");
    writer.flush().expect("flush fixture retry");
    bytes
}
