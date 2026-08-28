use std::collections::VecDeque;
use std::io::{BufReader, Cursor, Read, Write};
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};
use std::{
    collections as path_std_collections, io as path_std_io, net as path_std_net,
    path as path_std_path, sync as path_std_sync, thread,
};

use tau_proto::{
    Effort, HarnessInputMessage, HarnessInputReader, HarnessOutputMessage, HarnessOutputWriter,
    Verbosity,
};

use super::*;
use crate::tests::SharedTraceWriter;

/// Shared byte sink used by tests that run tau-client's writer thread.
#[derive(Clone, Default)]
struct SharedWriter {
    /// Shared byte buffer and notification for runtime output observers.
    bytes: Arc<(Mutex<Vec<u8>>, Condvar)>,
}

impl SharedWriter {
    /// Returns a snapshot of bytes written so far.
    fn bytes(&self) -> Vec<u8> {
        self.bytes.0.lock().expect("lock shared writer").clone()
    }

    /// Waits until the runtime appends bytes or the supplied deadline expires.
    fn wait_for_change(&self, previous_len: usize, deadline: Instant) {
        let (lock, cv) = &*self.bytes;
        let bytes = lock.lock().expect("lock shared writer");
        if bytes.len() != previous_len {
            return;
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return;
        };
        let _ = cv
            .wait_timeout_while(bytes, remaining, |bytes| bytes.len() == previous_len)
            .expect("wait for shared writer");
    }
}

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let (lock, cv) = &*self.bytes;
        lock.lock().expect("lock shared writer").extend(buf);
        cv.notify_all();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Blocking byte source used by tests that need to delay harness EOF.
#[derive(Clone, Default)]
struct BlockingInput {
    /// Shared input bytes, EOF flag, and reader-waiting observation.
    state: Arc<(Mutex<BlockingInputState>, Condvar)>,
}

/// Mutable state protected by [`BlockingInput`].
#[derive(Default)]
struct BlockingInputState {
    /// Bytes still available to the runtime reader.
    bytes: VecDeque<u8>,
    /// True once reads should return EOF after buffered bytes are consumed.
    closed: bool,
    /// True after the runtime reader blocks waiting for more bytes.
    waiting_for_more: bool,
}

impl BlockingInput {
    /// Appends encoded harness bytes for the runtime reader.
    fn push(&self, bytes: impl IntoIterator<Item = u8>) {
        let (lock, cv) = &*self.state;
        let mut state = lock.lock().expect("blocking input lock");
        state.bytes.extend(bytes);
        state.waiting_for_more = false;
        cv.notify_all();
    }

    /// Makes the reader return EOF after currently buffered bytes are consumed.
    fn close(&self) {
        let (lock, cv) = &*self.state;
        lock.lock().expect("blocking input lock").closed = true;
        cv.notify_all();
    }

    /// Waits until the runtime reader is parked for more harness input.
    fn wait_for_reader_waiting(&self, timeout: Duration) {
        let (lock, cv) = &*self.state;
        let deadline = Instant::now() + timeout;
        let mut state = lock.lock().expect("blocking input lock");
        while !state.waiting_for_more {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                panic!("runtime reader did not block for more input before timeout");
            };
            let (next, wait) = cv
                .wait_timeout(state, remaining)
                .expect("wait for input read");
            state = next;
            if wait.timed_out() && !state.waiting_for_more {
                panic!("runtime reader did not block for more input before timeout");
            }
        }
    }
}

impl Read for BlockingInput {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let (lock, cv) = &*self.state;
        let mut state = lock.lock().expect("blocking input lock");
        loop {
            if !state.bytes.is_empty() {
                let mut read = 0;
                while read < buf.len() {
                    let Some(byte) = state.bytes.pop_front() else {
                        break;
                    };
                    buf[read] = byte;
                    read += 1;
                }
                state.waiting_for_more = false;
                return Ok(read);
            }
            if state.closed {
                return Ok(0);
            }
            state.waiting_for_more = true;
            cv.notify_all();
            state = cv.wait(state).expect("wait for blocking input");
        }
    }
}

/// Manually advanced monotonic clock used by retry runtime acceptance tests.
struct VirtualRetryClock {
    /// Current virtual instant.
    now: Mutex<Instant>,
    /// Scheduler command sender attached when the actor starts.
    scheduler: Mutex<Option<std::sync::Weak<SyncSender<SchedulerCommand>>>>,
}

impl VirtualRetryClock {
    /// Creates a clock at a fixed monotonic epoch.
    fn new(now: Instant) -> Self {
        Self {
            now: Mutex::new(now),
            scheduler: Mutex::new(None),
        }
    }

    /// Advances time and synchronously interrupts any far-future actor wait.
    fn advance(&self, duration: Duration) {
        let mut now = self.now.lock().expect("virtual retry clock");
        *now = now.checked_add(duration).expect("virtual time overflow");
        drop(now);
        if let Some(scheduler) = self
            .scheduler
            .lock()
            .expect("scheduler sender")
            .as_ref()
            .and_then(path_std_sync::Weak::upgrade)
        {
            let (acknowledged, ack) = mpsc::sync_channel(0);
            scheduler
                .send(SchedulerCommand::Wake {
                    acknowledged: Some(acknowledged),
                })
                .expect("wake virtual scheduler");
            ack.recv_timeout(Duration::from_secs(1))
                .expect("virtual scheduler observed advanced time");
        }
    }
}

impl RetryClock for VirtualRetryClock {
    fn now(&self) -> Instant {
        *self.now.lock().expect("virtual retry clock")
    }

    fn attach_scheduler(&self, commands: std::sync::Weak<SyncSender<SchedulerCommand>>) {
        *self.scheduler.lock().expect("scheduler sender") = Some(commands);
    }
}

fn chatgpt_auth() -> OpenAiAuth {
    OpenAiAuth {
        access_token: "access".to_owned(),
        refresh_token: "refresh".to_owned(),
        expires_at_ms: u64::MAX,
        account_id: Some("account".to_owned()),
    }
}

fn model_ids(models: &[ProviderModelInfo]) -> Vec<String> {
    models.iter().map(|model| model.id.to_string()).collect()
}

fn decode_frames(bytes: &[u8]) -> Vec<HarnessInputMessage> {
    let mut reader = HarnessInputReader::new(BufReader::new(bytes));
    let mut frames = Vec::new();
    while let Some(frame) = reader.read_message().expect("decode frame") {
        frames.push(frame);
    }
    frames
}

/// Decodes a concurrent writer snapshot, returning `None` when it ends midway
/// through a frame and the caller should retry after the next synchronization.
fn try_decode_frames(bytes: &[u8]) -> Option<Vec<HarnessInputMessage>> {
    let mut reader = HarnessInputReader::new(BufReader::new(bytes));
    let mut frames = Vec::new();
    loop {
        match reader.read_message() {
            Ok(Some(frame)) => frames.push(frame),
            Ok(None) => return Some(frames),
            Err(tau_proto::DecodeError::Io(error))
                if error.kind() == path_std_io::ErrorKind::UnexpectedEof =>
            {
                return None;
            }
            Err(error) => panic!("decode concurrent runtime frame: {error}"),
        }
    }
}

fn encode_frames(frames: &[HarnessOutputMessage]) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut writer = HarnessOutputWriter::new(&mut bytes);
        if !matches!(frames.first(), Some(HarnessOutputMessage::Configure(_))) {
            writer
                .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
                    tool_prefix: None,
                    config: tau_proto::CborValue::Map(Vec::new()),
                    instance_name: tau_proto::ExtensionName::parse("test-extension")
                        .expect("test extension name must satisfy the identifier grammar"),
                    state_dir: None,
                    secrets: path_std_collections::BTreeMap::new(),
                    settings_files: Default::default(),
                }))
                .expect("encode initial configure");
        }
        for frame in frames {
            writer.write_message(frame).expect("encode frame");
        }
        writer.flush().expect("flush frames");
    }
    bytes
}

/// Waits for a complete runtime output snapshot satisfying `predicate`.
///
/// Runtime tests use this only to observe a protocol boundary; worker and
/// scheduler ordering itself is controlled by channels and input frame order.
fn wait_for_runtime_frames(
    output: &SharedWriter,
    predicate: impl Fn(&[HarnessInputMessage]) -> bool,
) -> Vec<HarnessInputMessage> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let bytes = output.bytes();
        if let Some(frames) = try_decode_frames(&bytes)
            && predicate(&frames)
        {
            return frames;
        }
        assert!(
            Instant::now() < deadline,
            "runtime output boundary not reached"
        );
        output.wait_for_change(bytes.len(), deadline);
    }
}

fn live_event(recorded_at: u64, event: Event) -> HarnessOutputMessage {
    HarnessOutputMessage::deliver_live(tau_proto::UnixMicros::new(recorded_at), event)
}

fn replay_event(recorded_at: u64, event: Event) -> HarnessOutputMessage {
    HarnessOutputMessage::deliver_replay(tau_proto::UnixMicros::new(recorded_at), event)
}

fn input_event(message: &HarnessInputMessage) -> Option<&Event> {
    match message {
        HarnessInputMessage::Emit(emit) => Some(emit.event.as_ref()),
        _ => None,
    }
}

fn session_dir(status: tau_proto::SessionDirStatus) -> tau_proto::HarnessSessionDir {
    tau_proto::HarnessSessionDir {
        session_id: "session-1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        path: path_std_path::PathBuf::from("/tmp/tau-test-session-1"),
        status,
    }
}

/// Retry status must clear the response without publishing a user-visible
/// message delta, so transient failure text never enters assistant output.
#[test]
fn retry_banner_emits_status_not_message_delta() {
    let mut bytes = Vec::new();
    {
        let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
        emit_retry_banner(
            &tau_proto::AgentPromptId::parse("sp-retry").expect("test prompt id"),
            &tau_proto::AgentId::parse("main").expect("agent id"),
            &tau_proto::PromptOriginator::User,
            &mut writer,
            "HTTP 500: temporary",
            Duration::from_secs(1),
            1,
        );
    }

    let frames = decode_frames(&bytes);
    let Some(Event::ProviderResponseUpdatedReported(update)) = frames.first().and_then(input_event)
    else {
        panic!("expected provider response update frame: {frames:?}");
    };
    assert!(update.deltas.is_empty());
    assert!(matches!(
        update.status.as_ref(),
        Some(tau_proto::ProviderResponseStatusUpdate {
            text,
            clear_response: true,
            retry: None,
        }) if text.contains("provider error")
    ));
}

fn model_id(provider: &str, model: &str) -> ModelId {
    ModelId::new(ProviderName::new(provider), ModelName::new(model))
}

pub(super) fn prompt() -> tau_proto::AgentPromptCreated {
    tau_proto::AgentPromptCreated {
        agent_prompt_id: "sp-1"
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
        agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
        session_id: "session-1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        system_prompt: String::new(),
        context: tau_proto::PromptContext {
            blocks: vec![tau_proto::ContextBlock::UserInput(
                tau_proto::UserInputBlock {
                    items: vec![ContextItem::Message(tau_proto::MessageItem {
                        role: tau_proto::ContextRole::User,
                        content: vec![tau_proto::ContentPart::Text {
                            text: "hello".to_owned(),
                        }],
                        phase: None,
                        responses_raw_json: None,
                    })],
                },
            )],
        },
        tools: Vec::new(),
        tools_ref: None,
        model: model_id(CHATGPT_PROVIDER_NAME, "gpt-5.6-sol"),
        model_params: Default::default(),
        tool_choice: tau_proto::ToolChoice::Auto,
        originator: tau_proto::PromptOriginator::User,
        share_user_cache_key: false,
        ctx_id: None,
        compaction: None,
        operation: tau_proto::PromptOperation::Inference,
    }
}

/// Builds the cache-only prefix corresponding to [`prompt`].
fn prewarm() -> tau_proto::AgentPromptPrewarmRequested {
    let prompt = prompt();
    tau_proto::AgentPromptPrewarmRequested {
        agent_id: prompt.agent_id,
        session_id: prompt.session_id,
        system_prompt: prompt.system_prompt,
        context: prompt.context,
        tools: prompt.tools,
        model: Some(prompt.model),
        model_params: prompt.model_params,
        tool_choice: prompt.tool_choice,
        originator: prompt.originator,
        share_user_cache_key: prompt.share_user_cache_key,
    }
}

/// A silent prewarm and a duplicate request must remain off the provider loop;
/// dispatching the real prompt cancels the warm work and completes normally.
#[test]
fn silent_duplicate_prewarm_does_not_block_real_prompt() {
    let input = BlockingInput::default();
    input.push(encode_frames(&[
        live_event(11, Event::AgentPromptPrewarmRequested(prewarm())),
        live_event(12, Event::AgentPromptPrewarmRequested(prewarm())),
    ]));
    let (started_tx, started_rx) = mpsc::channel();
    let executions = Arc::new(AtomicUsize::new(0));
    let executor_count = Arc::clone(&executions);
    let prewarm_executor: PrewarmExecutor = Arc::new(move |mut execution| {
        executor_count.fetch_add(1, Ordering::SeqCst);
        let (wake_tx, wake_rx) = mpsc::channel();
        let _guard = execution.abort.register_waker(Arc::new(move || {
            let _ = wake_tx.send(());
        }));
        started_tx.send(()).expect("announce prewarm start");
        wake_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("real prompt cancels silent prewarm");
        assert!(
            execution.abort.is_aborted(),
            "wake must represent owned cancellation"
        );
        tau_proto::ProviderCacheRefreshStatus::Cancelled
    });
    let prompt_executor: PromptExecutor = Arc::new(|execution| {
        let mut writer = execution.frame_writer();
        writer
            .write_message(&HarnessInputMessage::emit_transient(
                Event::ProviderResponseFinishedReported(simple_finished(
                    execution.job.agent_prompt_id,
                    execution.job.prompt.agent_id,
                    execution.job.prompt.originator,
                    "done",
                )),
            ))
            .expect("finished");
        writer.flush().expect("flush fake response");
    });
    let profiles = profiles_with_chatgpt_auth(chatgpt_auth());
    let prompt_profiles = profiles.clone();
    let output = SharedWriter::default();
    let runtime_output = output.clone();
    let runtime_input = input.clone();
    let runtime = thread::spawn(move || {
        run_inner_with_executors(
            runtime_input,
            runtime_output,
            profiles,
            move || prompt_profiles.clone(),
            1,
            prompt_executor,
            prewarm_executor,
        )
        .expect("run provider");
    });
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("prewarm worker starts");
    input.push(encode_frames(&[live_event(
        13,
        Event::AgentPromptCreated(prompt()),
    )]));
    wait_for_runtime_frames(&output, |frames| {
        frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseFinishedReported(finished))
                    if finished.agent_prompt_id.as_str() == "sp-1"
            )
        })
    });
    input.close();
    runtime.join().expect("provider exits");
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "duplicate prewarm must not start a second worker"
    );
}

/// A lifecycle-aware refresh produces exactly one content-free terminal report.
#[test]
fn cache_refresh_reports_correlated_terminal() {
    let refresh_id = tau_proto::ProviderCacheRefreshId::parse("pcr-test").expect("refresh id");
    let input = BlockingInput::default();
    input.push(encode_frames(&[live_event(
        11,
        Event::AgentCacheRefreshRequested(tau_proto::AgentCacheRefreshRequested {
            refresh_id: refresh_id.clone(),
            prompt: prewarm(),
            stop_after_millis: NonZeroU32::new(1_000).expect("nonzero"),
        }),
    )]));
    let prewarm_executor: PrewarmExecutor =
        Arc::new(|_| tau_proto::ProviderCacheRefreshStatus::Succeeded);
    let profiles = profiles_with_chatgpt_auth(chatgpt_auth());
    let prompt_profiles = profiles.clone();
    let prompt_executor: PromptExecutor = Arc::new(|execution| {
        let mut writer = execution.frame_writer();
        writer
            .write_message(&HarnessInputMessage::emit_transient(
                Event::ProviderResponseFinishedReported(simple_finished(
                    execution.job.agent_prompt_id,
                    execution.job.prompt.agent_id,
                    execution.job.prompt.originator,
                    "done",
                )),
            ))
            .expect("finished");
        writer.flush().expect("flush fake response");
    });
    let output = SharedWriter::default();
    let runtime_output = output.clone();
    let runtime_input = input.clone();
    let runtime = thread::spawn(move || {
        run_inner_with_executors(
            runtime_input,
            runtime_output,
            profiles,
            move || prompt_profiles.clone(),
            1,
            prompt_executor,
            prewarm_executor,
        )
        .expect("run provider");
    });
    wait_for_runtime_frames(&output, |frames| {
        frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderCacheRefreshFinishedReported(finished))
                    if finished.refresh_id == refresh_id
                        && finished.status
                            == tau_proto::ProviderCacheRefreshStatus::Succeeded
            )
        })
    });
    input.close();
    runtime.join().expect("provider exits");
}

/// Directed cancellation is consumed before the following real prompt on the
/// Provider FIFO and never delays that prompt.
#[test]
fn cache_refresh_cancel_precedes_real_prompt() {
    let refresh_id =
        tau_proto::ProviderCacheRefreshId::parse("pcr-cancel-order").expect("refresh id");
    let input = BlockingInput::default();
    input.push(encode_frames(&[live_event(
        11,
        Event::AgentCacheRefreshRequested(tau_proto::AgentCacheRefreshRequested {
            refresh_id: refresh_id.clone(),
            prompt: prewarm(),
            stop_after_millis: NonZeroU32::new(1_000).expect("nonzero"),
        }),
    )]));
    let (started_tx, started_rx) = mpsc::channel();
    let prewarm_executor: PrewarmExecutor = Arc::new(move |mut execution| {
        let (wake_tx, wake_rx) = mpsc::channel();
        let _guard = execution.abort.register_waker(Arc::new(move || {
            let _ = wake_tx.send(());
        }));
        started_tx.send(()).expect("started");
        wake_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("directed cancel wakes refresh");
        tau_proto::ProviderCacheRefreshStatus::Cancelled
    });
    let profiles = profiles_with_chatgpt_auth(chatgpt_auth());
    let prompt_profiles = profiles.clone();
    let prompt_executor: PromptExecutor = Arc::new(|execution| {
        let mut writer = execution.frame_writer();
        writer
            .write_message(&HarnessInputMessage::emit_transient(
                Event::ProviderResponseFinishedReported(simple_finished(
                    execution.job.agent_prompt_id,
                    execution.job.prompt.agent_id,
                    execution.job.prompt.originator,
                    "done",
                )),
            ))
            .expect("finished");
        writer.flush().expect("flush fake response");
    });
    let output = SharedWriter::default();
    let runtime_output = output.clone();
    let runtime_input = input.clone();
    let runtime = thread::spawn(move || {
        run_inner_with_executors(
            runtime_input,
            runtime_output,
            profiles,
            move || prompt_profiles.clone(),
            1,
            prompt_executor,
            prewarm_executor,
        )
        .expect("run provider");
    });
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("refresh starts");
    input.push(encode_frames(&[
        live_event(
            12,
            Event::AgentCacheRefreshCancelRequested(tau_proto::AgentCacheRefreshCancelRequested {
                refresh_id: refresh_id.clone(),
                reason: tau_proto::ProviderCacheRefreshCancelReason::RealPrompt,
            }),
        ),
        live_event(13, Event::AgentPromptCreated(prompt())),
    ]));
    wait_for_runtime_frames(&output, |frames| {
        let cancelled = frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderCacheRefreshFinishedReported(finished))
                    if finished.refresh_id == refresh_id
                        && finished.status == tau_proto::ProviderCacheRefreshStatus::Cancelled
            )
        });
        let prompt_finished = frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseFinishedReported(finished))
                    if finished.agent_prompt_id.as_str() == "sp-1"
            )
        });
        cancelled && prompt_finished
    });
    input.close();
    runtime.join().expect("provider exits");
}

/// Session shutdown must wake silent prewarm transport work and retain worker
/// ownership until its exact completion reaches the provider loop.
#[test]
fn session_shutdown_cancels_and_joins_silent_prewarm() {
    let input = BlockingInput::default();
    input.push(encode_frames(&[live_event(
        11,
        Event::AgentPromptPrewarmRequested(prewarm()),
    )]));
    let (started_tx, started_rx) = mpsc::channel();
    let (canceled_tx, canceled_rx) = mpsc::channel();
    let prewarm_executor: PrewarmExecutor = Arc::new(move |mut execution| {
        let (wake_tx, wake_rx) = mpsc::channel();
        let _guard = execution.abort.register_waker(Arc::new(move || {
            let _ = wake_tx.send(());
        }));
        started_tx.send(()).expect("announce prewarm start");
        wake_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("shutdown cancels silent prewarm");
        assert!(execution.abort.is_aborted());
        canceled_tx.send(()).expect("announce cancellation");
        tau_proto::ProviderCacheRefreshStatus::Cancelled
    });
    let profiles = profiles_with_chatgpt_auth(chatgpt_auth());
    let prompt_profiles = profiles.clone();
    let output = SharedWriter::default();
    let runtime_input = input.clone();
    let runtime = thread::spawn(move || {
        run_inner_with_executors(
            runtime_input,
            output,
            profiles,
            move || prompt_profiles.clone(),
            1,
            production_prompt_executor(),
            prewarm_executor,
        )
        .expect("run provider");
    });
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("prewarm worker starts");
    input.push(encode_frames(&[live_event(
        12,
        Event::SessionShutdown(tau_proto::SessionShutdown {
            session_id: "session-1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
        }),
    )]));
    canceled_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("shutdown reaches prewarm");
    input.close();
    runtime.join().expect("provider joins canceled prewarm");
}

/// Mutable credential rotation observed by unrelated prompt resolution must
/// cancel old-profile prewarm work rather than waiting for another prewarm.
#[test]
fn profile_rotation_cancels_active_prewarm() {
    let input = BlockingInput::default();
    input.push(encode_frames(&[live_event(
        11,
        Event::AgentPromptPrewarmRequested(prewarm()),
    )]));
    let (started_tx, started_rx) = mpsc::channel();
    let (canceled_tx, canceled_rx) = mpsc::channel();
    let prewarm_executor: PrewarmExecutor = Arc::new(move |mut execution| {
        let (wake_tx, wake_rx) = mpsc::channel();
        let _guard = execution.abort.register_waker(Arc::new(move || {
            let _ = wake_tx.send(());
        }));
        started_tx.send(()).expect("announce prewarm start");
        wake_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("profile rotation cancels prewarm");
        assert!(execution.abort.is_aborted());
        canceled_tx.send(()).expect("announce cancellation");
        tau_proto::ProviderCacheRefreshStatus::Cancelled
    });
    let prompt_executor: PromptExecutor = Arc::new(|execution| {
        let mut writer = execution.frame_writer();
        writer
            .write_message(&HarnessInputMessage::emit_transient(
                Event::ProviderResponseFinishedReported(simple_finished(
                    execution.job.agent_prompt_id,
                    execution.job.prompt.agent_id,
                    execution.job.prompt.originator,
                    "done",
                )),
            ))
            .expect("finished");
        writer.flush().expect("flush fake response");
    });
    let profiles = Arc::new(Mutex::new(profiles_with_chatgpt_auth(chatgpt_auth())));
    let startup_profiles = profiles.lock().expect("profiles").clone();
    let load_profiles = Arc::clone(&profiles);
    let output = SharedWriter::default();
    let runtime_input = input.clone();
    let runtime = thread::spawn(move || {
        run_inner_with_executors(
            runtime_input,
            output,
            startup_profiles,
            move || load_profiles.lock().expect("profiles").clone(),
            1,
            prompt_executor,
            prewarm_executor,
        )
        .expect("run provider");
    });
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("prewarm worker starts");
    {
        let mut profiles = profiles.lock().expect("profiles");
        let BuiltinProviderProfile::Chatgpt(profile) = profiles
            .providers
            .get_mut(&ProviderName::new("chatgpt"))
            .expect("chatgpt profile")
        else {
            panic!("expected ChatGPT profile");
        };
        profile.auth.access_token = "rotated-access".to_owned();
    }
    let mut rotated_prompt = prompt();
    rotated_prompt.agent_id = tau_proto::AgentId::parse("agent-2").expect("agent id");
    input.push(encode_frames(&[live_event(
        12,
        Event::AgentPromptCreated(rotated_prompt),
    )]));
    canceled_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("profile reconciliation reaches active prewarm");
    input.close();
    runtime.join().expect("provider exits");
}

/// Ensures credential identity rotation invalidates the old profile cooldown
/// and releases its parked work without relying on quota display telemetry.
#[test]
fn profile_identity_rotation_releases_old_shared_cooldown() {
    let clock = Arc::new(VirtualRetryClock::new(Instant::now()));
    let input = BlockingInput::default();
    input.push(encode_frames(&[live_event(
        11,
        Event::AgentPromptCreated(prompt()),
    )]));
    let (completed_tx, completed_rx) = mpsc::channel();
    let executor: PromptExecutor = Arc::new(move |execution| {
        if execution.job.agent_prompt_id.as_str() == "sp-1"
            && execution.job.retry_state.attempts == 0
        {
            send_worker_message(
                &execution.output_tx,
                &execution.output_waker,
                WorkerMessage::Retry {
                    job: execution.job,
                    decision: RetryDecision::new(RetryClass::UsageWindow)
                        .with_retry_after(Some(Duration::from_secs(86_400))),
                    live_detail: None,
                    canonical_unauthorized: false,
                    terminal_backend: None,
                },
            )
            .expect("park old-profile prompt");
            return;
        }
        let id = execution.job.agent_prompt_id.clone();
        let mut finished = simple_finished(
            id.clone(),
            execution.job.prompt.agent_id.clone(),
            execution.job.prompt.originator.clone(),
            "replace error",
        );
        finished.error = None;
        finished.stop_reason = tau_proto::ProviderStopReason::EndTurn;
        let mut writer = execution.frame_writer();
        writer
            .write_message(&HarnessInputMessage::emit_transient(
                Event::ProviderResponseFinishedReported(finished),
            ))
            .expect("successful terminal");
        writer.flush().expect("flush terminal");
        completed_tx
            .send(id.to_string())
            .expect("report completion");
    });
    let profiles = Arc::new(Mutex::new(profiles_with_chatgpt_auth(chatgpt_auth())));
    let startup_profiles = profiles.lock().expect("profiles").clone();
    let load_profiles = Arc::clone(&profiles);
    let output = SharedWriter::default();
    let runtime_output = output.clone();
    let runtime_input = input.clone();
    let runtime_clock: Arc<dyn RetryClock> = clock.clone();
    let (runtime_done_tx, runtime_done_rx) = mpsc::sync_channel(0);
    let runtime = thread::spawn(move || {
        run_inner_with_executors_and_clock(
            runtime_input,
            runtime_output,
            startup_profiles,
            move || load_profiles.lock().expect("profiles").clone(),
            2,
            RuntimeExecutors {
                prompt: executor,
                prewarm: production_prewarm_executor(),
                retry_clock: runtime_clock,
            },
        )
        .expect("run provider");
        runtime_done_tx.send(()).expect("report provider shutdown");
    });
    wait_for_runtime_frames(&output, |frames| {
        frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseUpdatedReported(update))
                    if update.agent_prompt_id.as_str() == "sp-1"
                        && update.status.as_ref().is_some_and(|status| status.retry.is_some())
            )
        })
    });

    {
        let mut profiles = profiles.lock().expect("profiles");
        let BuiltinProviderProfile::Chatgpt(profile) = profiles
            .providers
            .get_mut(&ProviderName::new("chatgpt"))
            .expect("chatgpt profile")
        else {
            panic!("expected ChatGPT profile");
        };
        profile.auth.access_token = "rotated-access".to_owned();
    }
    let mut rotated = prompt();
    rotated.agent_prompt_id = "rotated"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    input.push(encode_frames(&[live_event(
        12,
        Event::AgentPromptCreated(rotated),
    )]));

    assert_eq!(
        completed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("rotated-profile prompt finishes"),
        "rotated"
    );
    clock.advance(Duration::from_secs(RESET_BOUNDARY_JITTER_MAX.as_secs() + 1));
    let completed = path_std_collections::BTreeSet::from([
        "rotated".to_owned(),
        completed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("virtual-time old-profile prompt finishes"),
    ]);
    assert_eq!(
        completed,
        std::collections::BTreeSet::from(["rotated".to_owned(), "sp-1".to_owned()])
    );
    input.close();
    runtime_done_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("profile-rotation runtime and scheduler shut down");
    runtime.join().expect("provider exits");
}

/// Ensures shared retry evidence returned by an old active attempt after
/// credential rotation cannot reinstall a cooldown for the new identity.
#[test]
fn stale_old_identity_retry_cannot_park_new_profile_work() {
    let clock = Arc::new(VirtualRetryClock::new(Instant::now()));
    let input = BlockingInput::default();
    input.push(encode_frames(&[live_event(
        11,
        Event::AgentPromptCreated(prompt()),
    )]));
    let (old_started_tx, old_started_rx) = mpsc::sync_channel(1);
    let (release_old_tx, release_old_rx) = mpsc::sync_channel(0);
    let release_old_rx = Mutex::new(release_old_rx);
    let (completed_tx, completed_rx) = mpsc::channel();
    let executor: PromptExecutor = Arc::new(move |execution| {
        let id = execution.job.agent_prompt_id.to_string();
        if id == "sp-1" && execution.job.retry_state.attempts == 0 {
            old_started_tx.send(()).expect("report old attempt");
            release_old_rx
                .lock()
                .expect("old release receiver")
                .recv_timeout(Duration::from_secs(1))
                .expect("release stale old attempt");
            send_worker_message(
                &execution.output_tx,
                &execution.output_waker,
                WorkerMessage::Retry {
                    job: execution.job,
                    decision: RetryDecision::new(RetryClass::UsageWindow),
                    live_detail: None,
                    canonical_unauthorized: false,
                    terminal_backend: None,
                },
            )
            .expect("return stale shared evidence");
            return;
        }
        let mut finished = simple_finished(
            execution.job.agent_prompt_id.clone(),
            execution.job.prompt.agent_id.clone(),
            execution.job.prompt.originator.clone(),
            "identity-race success",
        );
        finished.error = None;
        let mut writer = execution.frame_writer();
        writer
            .write_message(&HarnessInputMessage::emit_transient(
                Event::ProviderResponseFinishedReported(finished),
            ))
            .expect("write identity-race terminal");
        writer.flush().expect("flush identity-race terminal");
        completed_tx.send(id).expect("report identity-race finish");
    });
    let profiles = Arc::new(Mutex::new(profiles_with_chatgpt_auth(chatgpt_auth())));
    let startup_profiles = profiles.lock().expect("profiles").clone();
    let load_profiles = Arc::clone(&profiles);
    let output = SharedWriter::default();
    let runtime_output = output.clone();
    let runtime_input = input.clone();
    let runtime_clock: Arc<dyn RetryClock> = clock.clone();
    let (runtime_done_tx, runtime_done_rx) = mpsc::sync_channel(1);
    let runtime = thread::spawn(move || {
        let result = run_inner_with_executors_and_clock(
            runtime_input,
            runtime_output,
            startup_profiles,
            move || load_profiles.lock().expect("profiles").clone(),
            3,
            RuntimeExecutors {
                prompt: executor,
                prewarm: production_prewarm_executor(),
                retry_clock: runtime_clock,
            },
        );
        runtime_done_tx.send(()).expect("report identity-race exit");
        result.expect("run identity-race provider");
    });
    old_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("old-profile attempt starts");
    {
        let mut profiles = profiles.lock().expect("profiles");
        let BuiltinProviderProfile::Chatgpt(profile) = profiles
            .providers
            .get_mut(&ProviderName::new("chatgpt"))
            .expect("chatgpt profile")
        else {
            panic!("expected ChatGPT profile");
        };
        profile.auth.access_token = "new-identity".to_owned();
    }
    let mut rotated = prompt();
    rotated.agent_prompt_id = "rotated-first"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    input.push(encode_frames(&[live_event(
        12,
        Event::AgentPromptCreated(rotated),
    )]));
    assert_eq!(
        completed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("new identity prompt finishes"),
        "rotated-first"
    );
    release_old_tx.send(()).expect("release old evidence");
    wait_for_runtime_frames(&output, |frames| {
        frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseUpdatedReported(update))
                    if update.agent_prompt_id.as_str() == "sp-1"
                        && update.status.as_ref().is_some_and(|status| status.retry.is_some())
            )
        })
    });
    let mut peer = prompt();
    peer.agent_prompt_id = "new-peer"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    input.push(encode_frames(&[live_event(
        13,
        Event::AgentPromptCreated(peer),
    )]));
    assert_eq!(
        completed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("stale evidence must not park new identity peer"),
        "new-peer"
    );
    clock.advance(Duration::from_secs(2));
    assert_eq!(
        completed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("old prompt eventually retries locally"),
        "sp-1"
    );
    input.close();
    runtime_done_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("identity-race runtime exits");
    runtime.join().expect("identity-race runtime joins");
}

/// Builds scheduler-owned logical state without starting a provider worker.
pub(super) fn scheduled_job(prompt_id: &str, provider: &str) -> PromptJob {
    let mut prompt = prompt();
    prompt.agent_prompt_id = prompt_id
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    prompt.model.provider = ProviderName::new(provider);
    PromptJob {
        agent_prompt_id: prompt.agent_prompt_id.clone(),
        debug_provider_requests: false,
        prompt,
        backend: PromptBackend::Unavailable {
            login_required: None,
        },
        pinned_chatgpt_identity: None,
        profile_identity: None,
        retry_state: PromptRetryState::default(),
        cancel_generation: 0,
        manual_cooldown_bypass: false,
        cooldown_probe: None,
    }
}

/// Minimal configured Chat Completions model for runtime routing fixtures.
fn chat_model(id: &str) -> ChatCompletionsModel {
    ChatCompletionsModel {
        id: ModelName::new(id),
        display_name: None,
        context_window: 128_000,
        compat: None,
        tags: Vec::new(),
        input_modalities: Vec::new(),
        tool_result_modalities: Vec::new(),
        supports_parallel_tool_calls: true,
        local_summary_compaction: None,
        cache_contract: None,
        est_uncached_input_cost_1m_usd: Default::default(),
        est_cached_input_cost_1m_usd: Default::default(),
        est_cache_write_input_cost_1m_usd: Default::default(),
        est_output_cost_1m_usd: Default::default(),
        est_cache_storage_cost_1m_token_hour_usd: None,
    }
}

/// Verifies shared cooldown extension, anti-herd jitter, scope isolation, and
/// fake-clock eligibility in the exact queue used by the scheduler thread.
#[test]
fn retry_schedule_queue_enforces_shared_cooldown_without_cross_provider_herd() {
    let epoch = Instant::now();
    let initial_due = epoch + Duration::from_secs(10);
    let extended_boundary = epoch + Duration::from_secs(60);
    let unaffected_due = epoch + Duration::from_secs(12);
    let mut queue = RetryScheduleQueue::default();
    for id in ["same-1", "same-2", "same-3", "same-4"] {
        queue
            .schedule(initial_due, None, scheduled_job(id, "limited"))
            .unwrap_or_else(|_| panic!("unique parked prompt"));
    }
    queue
        .schedule(unaffected_due, None, scheduled_job("peer", "healthy"))
        .unwrap_or_else(|_| panic!("unique parked prompt"));

    queue.extend_cooldown(&ProviderName::new("limited"), extended_boundary, 1);

    let deadlines = queue.deadlines();
    assert_eq!(queue.len(), 5);
    let limited = deadlines
        .iter()
        .filter(|(_, provider, _)| provider.as_str() == "limited")
        .map(|(_, _, due)| *due)
        .collect::<Vec<_>>();
    assert_eq!(limited.len(), 4);
    assert!(
        limited.iter().all(|due| *due > extended_boundary),
        "same-scope prompts need positive jitter after the common boundary: {limited:?}"
    );
    assert_eq!(
        limited
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        limited.len(),
        "stable prompt-local jitter must prevent an exact reset-boundary herd"
    );
    assert!(
        deadlines
            .iter()
            .any(|(id, provider, due)| id.as_str() == "peer"
                && provider.as_str() == "healthy"
                && *due == unaffected_due)
    );

    assert!(
        queue.pop_due(epoch + Duration::from_secs(11)).is_none(),
        "fake time before all deadlines must not execute an attempt"
    );
    assert_eq!(
        queue
            .pop_due(unaffected_due)
            .expect("different provider is independently due")
            .agent_prompt_id
            .as_str(),
        "peer"
    );
    assert_eq!(queue.len(), 4);
}

/// Ensures a successful probe advances only its provider's parked prompts,
/// retaining positive distinct stable jitter and all scheduler ownership.
#[test]
fn retry_schedule_queue_release_is_provider_scoped_and_jittered() {
    let epoch = Instant::now();
    let far_due = epoch + Duration::from_secs(86_400);
    let unrelated_due = epoch + Duration::from_secs(90);
    let mut queue = RetryScheduleQueue::default();
    for id in ["limited-1", "limited-2", "limited-3"] {
        queue
            .schedule(
                epoch,
                Some(CooldownConstraint {
                    generation: 1,
                    boundary: far_due,
                }),
                scheduled_job(id, "limited"),
            )
            .unwrap_or_else(|_| panic!("unique parked prompt"));
    }
    let independent_due = epoch + Duration::from_secs(45);
    queue
        .schedule(
            independent_due,
            None,
            scheduled_job("independent", "limited"),
        )
        .unwrap_or_else(|_| panic!("unique independent backoff"));
    queue
        .schedule(
            epoch,
            Some(CooldownConstraint {
                generation: 2,
                boundary: far_due,
            }),
            scheduled_job("newer-generation", "limited"),
        )
        .unwrap_or_else(|_| panic!("unique newer-generation cooldown"));
    queue
        .schedule(unrelated_due, None, scheduled_job("unrelated", "healthy"))
        .unwrap_or_else(|_| panic!("unique parked prompt"));

    queue.release_cooldown(&ProviderName::new("limited"), 1, epoch);

    let deadlines = queue.deadlines();
    let released = deadlines
        .iter()
        .filter(|(id, _, _)| id.as_str().starts_with("limited-"))
        .map(|(_, _, due)| *due)
        .collect::<Vec<_>>();
    assert_eq!(
        queue.len(),
        6,
        "release must not transfer or duplicate ownership"
    );
    assert!(released.iter().all(|due| *due > epoch && *due < far_due));
    assert_eq!(
        released
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        released.len(),
        "stable anti-herd jitter must remain distinct"
    );
    assert!(deadlines.iter().any(|(id, provider, due)| {
        id.as_str() == "unrelated" && provider.as_str() == "healthy" && *due == unrelated_due
    }));
    assert!(deadlines.iter().any(|(id, provider, due)| {
        id.as_str() == "independent" && provider.as_str() == "limited" && *due == independent_due
    }));
    assert!(
        deadlines
            .iter()
            .any(|(id, _, due)| { id.as_str() == "newer-generation" && *due > far_due })
    );
}

/// Ensures the actor's synchronous mutation seam preserves exact-generation
/// authority and independent deadlines rather than hiding policy in timer I/O.
#[test]
fn retry_scheduler_state_release_is_generation_scoped_and_deadline_safe() {
    let epoch = Instant::now();
    let independent_due = epoch + Duration::from_secs(30);
    let boundary = epoch + Duration::from_secs(5 * 86_400);
    let mut state = RetrySchedulerState::default();
    for (id, generation) in [("current", 7), ("newer", 8)] {
        assert!(
            state
                .step(SchedulerCommand::Schedule {
                    independent_due,
                    cooldown: Some(CooldownConstraint {
                        generation,
                        boundary,
                    }),
                    job: Box::new(scheduled_job(id, "limited")),
                })
                .is_empty()
        );
    }
    assert!(
        state
            .step(SchedulerCommand::ReleaseCooldown {
                provider: ProviderName::new("limited"),
                generation: 7,
                now: epoch,
            })
            .is_empty()
    );

    assert!(
        state
            .advance(independent_due - Duration::from_nanos(1))
            .is_empty(),
        "release cannot discard the prompt-local deadline"
    );
    let due = state.advance(
        independent_due
            .checked_add(RESET_BOUNDARY_JITTER_MAX)
            .expect("bounded virtual time"),
    );
    assert_eq!(due.len(), 1);
    assert!(matches!(
        &due[0],
        RetrySchedulerAction::Due(job) if job.agent_prompt_id.as_str() == "current"
    ));
    assert_eq!(state.queue.len(), 1, "newer evidence remains parked");
}

/// Ensures newer shared evidence always advances authority generation but can
/// never shorten the already observed provider boundary.
#[test]
fn newer_shorter_shared_evidence_preserves_long_boundary() {
    let provider = ProviderName::new("limited");
    let now = Instant::now();
    let long = now + Duration::from_secs(86_400);
    let short = now + Duration::from_secs(60);
    let mut cooldowns = BTreeMap::new();
    let mut generation = 0;
    let first = install_shared_cooldown(
        &mut cooldowns,
        &mut generation,
        provider.clone(),
        long,
        RetryClass::UsageWindow,
    );
    let second = install_shared_cooldown(
        &mut cooldowns,
        &mut generation,
        provider,
        short,
        RetryClass::Throttle,
    );
    assert_eq!(first.generation, 1);
    assert_eq!(second.generation, 2, "new evidence advances authority");
    assert_eq!(second.not_before, long, "new evidence cannot shorten");
    assert_eq!(
        second.class,
        RetryClass::UsageWindow,
        "the visible reason continues to describe the controlling boundary"
    );
    let authoritative = cooldowns
        .get(&ProviderName::new("limited"))
        .expect("cooldown");
    assert_eq!(authoritative.generation, 2);
    assert_eq!(authoritative.not_before, long);
    assert_eq!(authoritative.class, RetryClass::UsageWindow);
}

/// Ensures a syntactically valid maximum trusted hint cannot overflow into an
/// immediate retry and instead falls back to bounded generated cadence.
#[test]
fn overflowing_trusted_hint_falls_back_to_policy_due() {
    let now = Instant::now();
    let policy = Duration::from_secs(17);
    assert_eq!(
        retry_common_due(now, policy, Duration::from_secs(u64::MAX)),
        now + policy
    );
}

/// Usage-window reset estimates can become stale after provider- or user-driven
/// recovery, so even a multi-day estimate must retain periodic policy probes.
#[test]
fn usage_window_reset_hint_does_not_suppress_policy_retries() {
    let distant_reset = Some(Duration::from_secs(419_322));
    assert_eq!(
        scheduler_retry_hint(RetryClass::UsageWindow, distant_reset),
        None
    );
    assert_eq!(
        scheduler_retry_hint(RetryClass::Throttle, distant_reset),
        distant_reset,
        "other trusted retry hints retain their lower-bound behavior"
    );

    let now = Instant::now();
    let policy_delay = Duration::from_secs(17);
    let hint_delay =
        scheduler_retry_hint(RetryClass::UsageWindow, distant_reset).unwrap_or(Duration::ZERO);
    assert_eq!(
        retry_common_due(now, policy_delay, hint_delay),
        now + policy_delay
    );
}

/// Proves configured-to-removed-to-re-added reconciliation clears only the old
/// provider's shared generation and leaves unrelated cooldown state intact.
#[test]
fn removed_and_readded_profile_does_not_inherit_shared_cooldown() {
    let limited = ProviderName::new("limited");
    let healthy = ProviderName::new("healthy");
    let boundary = Instant::now() + Duration::from_secs(86_400);
    let old = SharedCooldown {
        not_before: boundary,
        class: RetryClass::UsageWindow,
        generation: 7,
    };
    let unrelated = SharedCooldown {
        not_before: boundary,
        class: RetryClass::Throttle,
        generation: 3,
    };
    let mut identities = BTreeMap::from([
        (
            limited.clone(),
            Some(BackendProfileIdentity::from_test_value(10)),
        ),
        (
            healthy.clone(),
            Some(BackendProfileIdentity::from_test_value(20)),
        ),
    ]);
    let mut cooldowns = BTreeMap::from([(limited.clone(), old), (healthy.clone(), unrelated)]);

    let (removed, old_cooldown) =
        reconcile_inference_identity(&mut identities, &mut cooldowns, &limited, None);
    assert!(removed);
    assert_eq!(old_cooldown.expect("old shared state").generation, 7);
    assert!(!cooldowns.contains_key(&limited));
    assert_eq!(
        cooldowns.get(&healthy).map(|state| state.generation),
        Some(3),
        "profile removal is provider scoped"
    );
    cooldowns.insert(
        limited.clone(),
        SharedCooldown {
            not_before: boundary,
            class: RetryClass::Auth,
            generation: 8,
        },
    );

    let (readded, inherited) = reconcile_inference_identity(
        &mut identities,
        &mut cooldowns,
        &limited,
        Some(BackendProfileIdentity::from_test_value(11)),
    );
    assert!(readded);
    assert_eq!(
        inherited
            .expect("unavailable-profile shared state")
            .generation,
        8
    );
    assert!(!cooldowns.contains_key(&limited));
    assert_eq!(
        identities.get(&limited),
        Some(&Some(BackendProfileIdentity::from_test_value(11)))
    );
    assert_eq!(
        cooldowns.get(&healthy).map(|state| state.generation),
        Some(3)
    );
}

/// Proves only a non-error terminal from the exact captured generation may
/// invalidate a shared cooldown; stale, error, and canceled terminals may not.
#[test]
fn successful_probe_requires_current_generation_and_successful_terminal() {
    let provider = ProviderName::new("limited");
    let prompt_id: tau_proto::AgentPromptId = "probe"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    let probe = CooldownProbe {
        provider: provider.clone(),
        generation: 7,
    };
    let cooldowns = BTreeMap::from([(
        provider,
        SharedCooldown {
            not_before: Instant::now() + Duration::from_secs(60),
            class: RetryClass::UsageWindow,
            generation: 7,
        },
    )]);
    let agent_id = tau_proto::AgentId::parse("agent-1").expect("agent id");
    let mut successful = simple_finished(
        prompt_id.clone(),
        agent_id.clone(),
        tau_proto::PromptOriginator::User,
        "replace error",
    );
    successful.error = None;
    successful.stop_reason = tau_proto::ProviderStopReason::EndTurn;
    for stop_reason in [
        tau_proto::ProviderStopReason::EndTurn,
        tau_proto::ProviderStopReason::ToolCalls,
        tau_proto::ProviderStopReason::Length,
    ] {
        successful.stop_reason = stop_reason;
        let success_message = HarnessInputMessage::emit_transient(
            Event::ProviderResponseFinishedReported(successful.clone()),
        );
        assert!(
            successful_probe_matches(&success_message, &prompt_id, &probe, &cooldowns),
            "{stop_reason:?} is authoritative after commit validation"
        );
    }
    successful.stop_reason = tau_proto::ProviderStopReason::EndTurn;
    let success_message = HarnessInputMessage::emit_transient(
        Event::ProviderResponseFinishedReported(successful.clone()),
    );
    assert!(!successful_probe_matches(
        &success_message,
        &tau_proto::AgentPromptId::parse("different")
            .expect("known-safe AgentPromptId must be valid"),
        &probe,
        &cooldowns
    ));

    let stale_probe = CooldownProbe {
        generation: 6,
        ..probe.clone()
    };
    assert!(!successful_probe_matches(
        &success_message,
        &prompt_id,
        &stale_probe,
        &cooldowns
    ));

    let error_message = HarnessInputMessage::emit_transient(
        Event::ProviderResponseFinishedReported(simple_finished(
            prompt_id.clone(),
            agent_id,
            tau_proto::PromptOriginator::User,
            "provider error",
        )),
    );
    assert!(!successful_probe_matches(
        &error_message,
        &prompt_id,
        &probe,
        &cooldowns
    ));

    for stop_reason in [
        tau_proto::ProviderStopReason::Error,
        tau_proto::ProviderStopReason::RepetitionDetected,
    ] {
        let mut non_success = successful.clone();
        non_success.stop_reason = stop_reason;
        assert!(!successful_probe_matches(
            &HarnessInputMessage::emit_transient(Event::ProviderResponseFinishedReported(
                non_success
            )),
            &prompt_id,
            &probe,
            &cooldowns
        ));
    }
    let mut typed_failure = successful.clone();
    typed_failure.failure_kind = Some(tau_proto::ProviderFailureKind::Unknown);
    assert!(!successful_probe_matches(
        &HarnessInputMessage::emit_transient(Event::ProviderResponseFinishedReported(
            typed_failure
        )),
        &prompt_id,
        &probe,
        &cooldowns
    ));

    let cancellation = CancellationState::default();
    cancellation.cancel(prompt_id.clone());
    let (canceled_message, released_provider) = validate_worker_output_and_probe_for_commit(
        Box::new(success_message),
        (0, 0, false),
        &prompt_id,
        &cancellation,
        Some(&probe),
        &cooldowns,
    )
    .expect("successful output becomes a canceled terminal");
    assert!(
        released_provider.is_none(),
        "cancellation validation must precede release authority"
    );
    assert!(!successful_probe_matches(
        &canceled_message,
        &prompt_id,
        &probe,
        &cooldowns
    ));
}

/// Ensures material Responses, Chat Completions, OpenRouter, unavailable, and
/// backend-family changes produce the intended inference identity boundaries.
#[test]
fn inference_profile_identity_tracks_chat_completions_rotation() {
    let provider = ChatCompletionsProvider {
        base_url: "https://generic.invalid/v1".to_owned(),
        api_key: "old-key".to_owned(),
        models: vec![chat_model("model")],
        ..ChatCompletionsProvider::default()
    };
    let old = PromptBackend::ChatCompletions {
        provider: provider.clone(),
        model: chat_model("model"),
    };
    let mut rotated = provider;
    rotated.api_key = "new-key".to_owned();
    let new = PromptBackend::ChatCompletions {
        provider: rotated.clone(),
        model: chat_model("model"),
    };
    let mut moved = rotated;
    moved.base_url = "https://replacement.invalid/v1".to_owned();
    let moved = PromptBackend::ChatCompletions {
        provider: moved,
        model: chat_model("model"),
    };
    let mut profiles = profiles_with_chatgpt_auth(chatgpt_auth());
    let mut refresh_rejections = OAuthRefreshRejectionCache::default();
    let responses = resolve_prompt_backend(
        &prompt().model,
        &mut profiles,
        &mut refresh_rejections,
        &test_network_policy(),
        None,
    )
    .expect("configured Responses backend");

    assert_ne!(
        backend_profile_identity(&old),
        backend_profile_identity(&new)
    );
    assert_ne!(
        backend_profile_identity(&new),
        backend_profile_identity(&responses),
        "backend-kind replacement changes inference identity"
    );
    assert_ne!(
        backend_profile_identity(&old),
        backend_profile_identity(&moved),
        "generic Chat Completions base URL is material identity"
    );

    let router_old = crate::chat_completions::OpenRouterProfile {
        api_key: "router-old".to_owned(),
        models: vec![chat_model("route/model")],
    };
    let router_new = crate::chat_completions::OpenRouterProfile {
        api_key: "router-new".to_owned(),
        ..router_old.clone()
    };
    let router_backend =
        |profile: &crate::chat_completions::OpenRouterProfile| PromptBackend::ChatCompletions {
            provider: profile.to_chat_completions(),
            model: chat_model("route/model"),
        };
    assert_ne!(
        backend_profile_identity(&router_backend(&router_old)),
        backend_profile_identity(&router_backend(&router_new)),
        "OpenRouter route credential rotation changes identity"
    );

    let PromptBackend::Responses(responses_config) = responses else {
        panic!("resolved ChatGPT profile must use Responses");
    };
    for credentials in [
        tau_provider_codex::ResolvedCredentials::new(
            "rotated-token".to_owned(),
            Some("account".to_owned()),
        ),
        tau_provider_codex::ResolvedCredentials::new(
            "token".to_owned(),
            Some("replacement-account".to_owned()),
        ),
    ] {
        let rotated = tau_provider_codex::resolved_config_for_model(
            &tau_proto::ModelName::new(responses_config.model_id()),
            credentials,
            responses_config.mode(),
        );
        assert_ne!(
            responses_profile_identity(&responses_config),
            responses_profile_identity(&rotated),
            "Responses key and account are material profile identity"
        );
    }
    assert_eq!(
        backend_profile_identity(&PromptBackend::Unavailable {
            login_required: None,
        }),
        None,
        "removed profiles carry no stale identity"
    );
}

/// Verifies targeted and global delayed cancellation remove only the intended
/// logical prompts and never wait for their far-future deadlines.
#[test]
fn retry_schedule_queue_cancellation_is_prompt_scoped_and_immediate() {
    let due = Instant::now() + Duration::from_secs(86_400);
    let mut queue = RetryScheduleQueue::default();
    queue
        .schedule(due, None, scheduled_job("target", "limited"))
        .unwrap_or_else(|_| panic!("unique parked prompt"));
    queue
        .schedule(due, None, scheduled_job("peer", "limited"))
        .unwrap_or_else(|_| panic!("unique parked prompt"));

    let canceled = queue.cancel(
        &"target"
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
    );
    assert_eq!(canceled.len(), 1);
    assert_eq!(canceled[0].agent_prompt_id.as_str(), "target");
    assert_eq!(queue.len(), 1, "same-cooldown peer must remain delayed");
    assert_eq!(
        queue.cancel_all()[0].agent_prompt_id.as_str(),
        "peer",
        "global cancellation must synchronously drain the remaining queue"
    );
    assert_eq!(queue.len(), 0);
}

/// Proves the scheduler ownership linearization used by timer-versus-manual
/// races: whichever removal runs first obtains the only logical job.
#[test]
fn retry_schedule_queue_timer_and_manual_release_are_mutually_exclusive() {
    let now = Instant::now();
    let mut timer_wins = RetryScheduleQueue::default();
    timer_wins
        .schedule(now, None, scheduled_job("timer-wins", "limited"))
        .unwrap_or_else(|_| panic!("unique parked prompt"));
    assert!(timer_wins.pop_due(now).is_some());
    assert!(
        timer_wins
            .cancel(
                &"timer-wins"
                    .parse::<tau_proto::AgentPromptId>()
                    .expect("known-safe AgentPromptId must be valid")
            )
            .is_empty()
    );

    let mut manual_wins = RetryScheduleQueue::default();
    manual_wins
        .schedule(now, None, scheduled_job("manual-wins", "limited"))
        .unwrap_or_else(|_| panic!("unique parked prompt"));
    assert_eq!(
        manual_wins
            .cancel(
                &"manual-wins"
                    .parse::<tau_proto::AgentPromptId>()
                    .expect("known-safe AgentPromptId must be valid")
            )
            .len(),
        1
    );
    assert!(manual_wins.pop_due(now).is_none());
}

/// Proves repeated manual commands cannot clone scheduler-owned state and a
/// peer parked under the same provider cooldown remains untouched.
#[test]
fn retry_schedule_queue_double_manual_release_moves_exactly_one_job() {
    let due = Instant::now() + Duration::from_secs(60);
    let mut queue = RetryScheduleQueue::default();
    queue
        .schedule(due, None, scheduled_job("target", "limited"))
        .unwrap_or_else(|_| panic!("unique parked prompt"));
    queue
        .schedule(due, None, scheduled_job("peer", "limited"))
        .unwrap_or_else(|_| panic!("unique parked prompt"));

    let first = queue.cancel(
        &"target"
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
    );
    let second = queue.cancel(
        &"target"
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
    );
    assert_eq!(first.len(), 1);
    assert!(second.is_empty());
    assert_eq!(queue.len(), 1);
    assert_eq!(
        queue.cancel(
            &"peer"
                .parse::<tau_proto::AgentPromptId>()
                .expect("known-safe AgentPromptId must be valid")
        )[0]
        .agent_prompt_id
        .as_str(),
        "peer"
    );
}

/// Proves an atomic manual transfer preserves retry accounting and all prompt
/// identity/origin data; only the later main-loop admission sets its one-shot
/// cooldown bypass.
#[test]
fn retry_schedule_queue_manual_transfer_preserves_logical_prompt_state() {
    let due = Instant::now() + Duration::from_secs(60);
    let mut job = scheduled_job("target", "limited");
    job.retry_state.attempts = 7;
    job.retry_state.previous = 13;
    job.retry_state.current = 21;
    job.debug_provider_requests = true;
    let expected_prompt = job.prompt.clone();
    let mut queue = RetryScheduleQueue::default();
    assert!(
        queue.schedule(due, None, job).is_ok(),
        "unique parked prompt"
    );

    let mut transferred = queue
        .cancel(
            &"target"
                .parse::<tau_proto::AgentPromptId>()
                .expect("known-safe AgentPromptId must be valid"),
        )
        .pop()
        .expect("parked job");
    assert_eq!(transferred.retry_state.attempts, 7);
    assert_eq!(transferred.retry_state.previous, 13);
    assert_eq!(transferred.retry_state.current, 21);
    assert!(transferred.debug_provider_requests);
    assert_eq!(transferred.prompt, expected_prompt);
    assert!(!transferred.manual_cooldown_bypass);

    transferred.manual_cooldown_bypass = true;
    assert!(transferred.manual_cooldown_bypass);
}

/// Exercises the exact worker-channel commit handoff: output is queued first,
/// targeted cancellation wins before main-loop drain, a peer still commits,
/// and the consumed marker permits later prompt-ID reuse.
#[test]
fn targeted_cancel_between_output_enqueue_and_main_drain_is_terminal_once() {
    let (tx, rx) = mpsc::channel();
    let target: tau_proto::AgentPromptId = "target"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    let peer: tau_proto::AgentPromptId = "peer"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    let agent_id = tau_proto::AgentId::parse("agent-1").expect("agent id");
    let originator = tau_proto::PromptOriginator::User;
    tx.send(WorkerMessage::Output {
        message: Box::new(HarnessInputMessage::emit_transient(
            Event::ProviderResponseUpdatedReported(ProviderResponseUpdated {
                agent_prompt_id: target.clone(),
                agent_id: agent_id.clone(),
                deltas: vec![tau_proto::ProviderResponseTextDelta::Message {
                    output_index: 0,
                    text: "must not commit".to_owned(),
                    phase: None,
                }],
                compaction: None,
                status: None,
                response_stats: None,
                originator: originator.clone(),
            }),
        )),
        cancel_generation: 0,
        agent_prompt_id: target.clone(),
        cooldown_probe: None,
    })
    .expect("queue target delta");
    tx.send(WorkerMessage::Output {
        message: Box::new(HarnessInputMessage::emit_transient(
            Event::ProviderResponseUpdatedReported(ProviderResponseUpdated {
                agent_prompt_id: target.clone(),
                agent_id: agent_id.clone(),
                deltas: Vec::new(),
                compaction: None,
                status: Some(tau_proto::ProviderResponseStatusUpdate {
                    text: "discarding tentative output".to_owned(),
                    clear_response: true,
                    retry: None,
                }),
                response_stats: None,
                originator: originator.clone(),
            }),
        )),
        cancel_generation: 0,
        agent_prompt_id: target.clone(),
        cooldown_probe: None,
    })
    .expect("queue target clear");
    for (id, text) in [(&target, "stale success"), (&peer, "peer success")] {
        tx.send(WorkerMessage::Output {
            message: Box::new(HarnessInputMessage::emit_transient(
                Event::ProviderResponseFinishedReported(simple_finished(
                    id.clone(),
                    agent_id.clone(),
                    originator.clone(),
                    text,
                )),
            )),
            cancel_generation: 0,
            agent_prompt_id: id.clone(),
            cooldown_probe: None,
        })
        .expect("queue terminal");
    }

    let cancellation = CancellationState::default();
    cancellation.cancel(target.clone());
    let mut committed = Vec::new();
    while let Ok(WorkerMessage::Output {
        message,
        cancel_generation,
        agent_prompt_id,
        cooldown_probe: _,
    }) = rx.try_recv()
    {
        if let Some(message) = validate_worker_output_for_commit(
            message,
            cancel_generation,
            0,
            false,
            &agent_prompt_id,
            &cancellation,
        ) {
            committed.push(message);
        }
    }

    assert_eq!(
        committed
            .iter()
            .filter(|message| matches!(
                input_event(message),
                Some(Event::ProviderResponseUpdatedReported(update))
                    if update.agent_prompt_id == target
                        && !update.status.as_ref().is_some_and(|status| status.clear_response)
            ))
            .count(),
        0,
        "queued tentative target delta must be discarded"
    );
    let clear_position = committed
        .iter()
        .position(|message| {
            matches!(
                input_event(message),
                Some(Event::ProviderResponseUpdatedReported(update))
                    if update.agent_prompt_id == target
                        && update.status.as_ref().is_some_and(|status| status.clear_response)
            )
        })
        .expect("intentional target clear must commit");
    let canceled_position = committed
        .iter()
        .position(|message| {
            matches!(
                input_event(message),
                Some(Event::ProviderResponseFinishedReported(finished))
                    if finished.agent_prompt_id == target
                        && finished.error.as_deref() == Some("(cancelled by harness)")
            )
        })
        .expect("canceled final must commit");
    assert!(clear_position < canceled_position);
    assert_eq!(
        committed
            .iter()
            .filter(|message| matches!(
                input_event(message),
                Some(Event::ProviderResponseFinishedReported(finished))
                    if finished.agent_prompt_id == target
                        && finished.error.as_deref() == Some("(cancelled by harness)")
            ))
            .count(),
        1,
        "target lifecycle must close exactly once as canceled"
    );
    assert!(committed.iter().any(|message| matches!(
        input_event(message),
        Some(Event::ProviderResponseFinishedReported(finished))
            if finished.agent_prompt_id == peer
                && finished.error.as_deref() == Some("peer success")
    )));
    assert!(
        !cancellation.is_canceled(&target),
        "targeted marker is consumed only by terminal commit"
    );

    let reused = validate_worker_output_for_commit(
        Box::new(HarnessInputMessage::emit_transient(
            Event::ProviderResponseFinishedReported(simple_finished(
                target.clone(),
                agent_id,
                originator,
                "reused prompt success",
            )),
        )),
        0,
        0,
        false,
        &target,
        &cancellation,
    )
    .expect("reused prompt ID may commit");
    assert!(matches!(
        input_event(&reused),
        Some(Event::ProviderResponseFinishedReported(finished))
            if finished.agent_prompt_id == target
                && finished.error.as_deref() == Some("reused prompt success")
    ));
}

/// Registered ChatGPT profiles must publish their models even when credentials
/// are absent, because authentication affects execution rather than discovery.
#[test]
fn chatgpt_profile_publishes_models_even_without_auth_tokens() {
    let models = models_for_auth(&OpenAiAuth::default());

    assert!(model_ids(&models).starts_with(&["chatgpt/gpt-5.6-sol".to_owned()]));
}

/// Ensures ChatGPT publication exposes the owned model set and mirrors the
/// backend capability split: GPT-5.6 uses standalone rather than inline
/// compaction in its default standard mode, while older models retain inline
/// compaction.
#[test]
fn chatgpt_oauth_publishes_chatgpt_models() {
    // ChatGPT/Codex is a provider namespace named `chatgpt`; there is no
    // compatibility fallback to an `openai-codex` provider name.
    let models = models_for_auth(&chatgpt_auth());

    assert_eq!(
        model_ids(&models),
        vec![
            "chatgpt/gpt-5.6-sol",
            "chatgpt/gpt-5.6-terra",
            "chatgpt/gpt-5.6-luna",
            "chatgpt/gpt-5.5",
            "chatgpt/gpt-5.4",
            "chatgpt/gpt-5.4-mini",
            "chatgpt/gpt-5.3-codex"
        ]
    );
    assert!(
        models
            .iter()
            .filter(|model| model.id.model.as_str().starts_with("gpt-5.6-"))
            .all(|model| !model.supports_compaction)
    );
    assert!(
        models
            .iter()
            .filter(|model| model.id.model.as_str().starts_with("gpt-5.6-"))
            .all(|model| model.supports_standalone_compaction)
    );
    assert!(
        models
            .iter()
            .filter(|model| !model.id.model.as_str().starts_with("gpt-5.6-"))
            .all(|model| model.supports_compaction)
    );
}

/// ChatGPT profiles resolve to the Codex Responses transport configuration.
#[test]
fn resolves_chatgpt_to_codex_responses_backend() {
    // ChatGPT is OAuth-backed and enables Codex-specific transport and replay
    // features owned by this provider slice.
    let mut profiles = profiles_with_chatgpt_auth(chatgpt_auth());
    let mut refresh_rejections = OAuthRefreshRejectionCache::default();

    let config = resolve_responses_backend(
        &model_id(CHATGPT_PROVIDER_NAME, "gpt-5.4"),
        &mut profiles,
        &mut refresh_rejections,
        &test_network_policy(),
        None,
    )
    .expect("chatgpt backend");

    assert_eq!(config.base_url(), tau_provider_codex::DEFAULT_BASE_URL);
    assert!(config.credentials_match("access", Some("account")));
    assert!(config.supports_compaction());
    assert!(config.supports_phase());
    assert!(config.supports_encrypted_reasoning());
}

/// Assistant phase metadata remains limited to the supported Codex model
/// families.
#[test]
fn chatgpt_phase_metadata_is_model_specific() {
    // The assistant `phase` field is only accepted by newer Codex model
    // families, so the hardcoded resolver must preserve the old whitelist.
    let mut profiles = profiles_with_chatgpt_auth(chatgpt_auth());
    let mut refresh_rejections = OAuthRefreshRejectionCache::default();

    let old = resolve_responses_backend(
        &model_id(CHATGPT_PROVIDER_NAME, "gpt-5.2-codex"),
        &mut profiles,
        &mut refresh_rejections,
        &test_network_policy(),
        None,
    )
    .expect("old codex backend");
    let new = resolve_responses_backend(
        &model_id(CHATGPT_PROVIDER_NAME, "gpt-5.3-codex"),
        &mut profiles,
        &mut refresh_rejections,
        &test_network_policy(),
        None,
    )
    .expect("new codex backend");

    assert!(!old.supports_phase());
    assert!(new.supports_phase());
}

/// Extra-high reasoning metadata remains limited to supported model families.
#[test]
fn xhigh_metadata_is_model_specific() {
    // The UI cycles through the provider-published effort list, so hardcoded
    // metadata must preserve xhigh only for model families that accept it.
    let models = models_for_auth(&chatgpt_auth());
    let ids_with_xhigh = models
        .iter()
        .filter(|model| model.efforts.contains(&Effort::XHigh))
        .map(|model| model.id.to_string())
        .collect::<Vec<_>>();

    assert_eq!(
        ids_with_xhigh,
        vec![
            "chatgpt/gpt-5.6-sol",
            "chatgpt/gpt-5.6-terra",
            "chatgpt/gpt-5.6-luna",
            "chatgpt/gpt-5.5",
            "chatgpt/gpt-5.4",
            "chatgpt/gpt-5.3-codex"
        ]
    );
}

/// ChatGPT model declarations must publish accepted verbosity choices so UI
/// cycling follows the provider snapshot.
#[test]
fn verbosity_metadata_is_published_for_chatgpt_models() {
    let models = models_for_auth(&chatgpt_auth());
    let gpt = models
        .iter()
        .find(|model| model.id.to_string() == "chatgpt/gpt-5.6-sol")
        .expect("gpt-5.6-sol model");

    assert_eq!(
        gpt.verbosities,
        vec![Verbosity::Low, Verbosity::Medium, Verbosity::High]
    );
}

/// Accepted provider prompts must enter worker execution concurrently rather
/// than waiting for an earlier prompt to finish.
#[test]
fn prompt_workers_start_concurrently() {
    let mut first = prompt();
    first.agent_prompt_id = "sp-par-1"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    let mut second = prompt();
    second.agent_prompt_id = "sp-par-2"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    let input = encode_frames(&[
        live_event(11, Event::AgentPromptCreated(first)),
        live_event(12, Event::AgentPromptCreated(second)),
    ]);
    let started = path_std_sync::Arc::new((Mutex::new((0_usize, 0_usize)), Condvar::new()));
    let executor_started = started.clone();
    let executor: PromptExecutor = path_std_sync::Arc::new(move |execution| {
        let agent_prompt_id = execution.job.agent_prompt_id.clone();
        let originator = execution.job.prompt.originator.clone();
        let (lock, cv) = &*executor_started;
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut guard = lock.lock().expect("started lock");
        guard.0 += 1;
        guard.1 = guard.1.max(guard.0);
        cv.notify_all();
        while guard.0 < 2 {
            let now = Instant::now();
            let Some(remaining) = deadline.checked_duration_since(now) else {
                break;
            };
            let (next, wait) = cv.wait_timeout(guard, remaining).expect("wait for peer");
            guard = next;
            if wait.timed_out() {
                break;
            }
        }
        drop(guard);

        let mut writer = execution.frame_writer();
        write_prompt_submitted(&agent_prompt_id, &originator, &mut writer).expect("submitted");
        writer
            .write_message(&HarnessInputMessage::emit_transient(
                Event::ProviderResponseFinishedReported(simple_finished(
                    agent_prompt_id.clone(),
                    tau_proto::AgentId::parse("agent-1").expect("valid test agent id"),
                    originator,
                    "done",
                )),
            ))
            .expect("finished");
        writer.flush().expect("flush fake response");

        let mut guard = lock.lock().expect("started lock");
        guard.0 -= 1;
        cv.notify_all();
    });

    let profiles = profiles_with_chatgpt_auth(chatgpt_auth());
    let prompt_profiles = profiles.clone();
    let writer = SharedWriter::default();
    let output = writer.clone();
    run_inner_with_prompt_executor(
        Cursor::new(input),
        writer,
        profiles,
        move || prompt_profiles.clone(),
        2,
        executor,
    )
    .expect("run provider extension");

    let max_started = started.0.lock().expect("started lock").1;
    assert_eq!(max_started, 2, "both prompt workers should overlap");
    let frames = decode_frames(&output.bytes());
    let finished_count = frames
        .iter()
        .filter(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseFinishedReported(_))
            )
        })
        .count();
    assert_eq!(finished_count, 2);
}

/// A second parked instance of one APID is rejected at insertion so timer and
/// manual operations can never dispatch two copies of one logical prompt.
#[test]
fn retry_schedule_rejects_duplicate_prompt_identity() {
    let mut queue = RetryScheduleQueue::default();
    let due = Instant::now() + Duration::from_secs(30);
    queue
        .schedule(due, None, scheduled_job("same", "limited"))
        .unwrap_or_else(|_| panic!("first owner"));
    let duplicate = queue.schedule(due, None, scheduled_job("same", "limited"));
    assert!(duplicate.is_err(), "duplicate APID must be returned");
    assert_eq!(queue.len(), 1, "original remains the sole parked owner");
}

/// Ensures a retryable attempt is parked outside the worker pool and later
/// succeeds without duplicating the logical prompt lifecycle.
#[test]
fn retryable_attempt_is_rescheduled_then_finishes_once() {
    let clock = Arc::new(VirtualRetryClock::new(Instant::now()));
    let input = BlockingInput::default();
    input.push(encode_frames(&[live_event(
        11,
        Event::AgentPromptCreated(prompt()),
    )]));
    let attempts = Arc::new(AtomicUsize::new(0));
    let executor_attempts = Arc::clone(&attempts);
    let executor: PromptExecutor = Arc::new(move |execution| {
        if executor_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            send_worker_message(
                &execution.output_tx,
                &execution.output_waker,
                WorkerMessage::Retry {
                    job: execution.job,
                    decision: RetryDecision::new(RetryClass::Transport),
                    live_detail: None,
                    canonical_unauthorized: false,
                    terminal_backend: None,
                },
            )
            .expect("return retry outcome");
            return;
        }
        let mut writer = execution.frame_writer();
        writer
            .write_message(&HarnessInputMessage::emit_transient(
                Event::ProviderResponseFinishedReported(simple_finished(
                    execution.job.agent_prompt_id,
                    execution.job.prompt.agent_id,
                    execution.job.prompt.originator,
                    "done",
                )),
            ))
            .expect("finished frame");
        writer.flush().expect("flush finished frame");
    });
    let profiles = profiles_with_chatgpt_auth(chatgpt_auth());
    let prompt_profiles = profiles.clone();
    let writer = SharedWriter::default();
    let output = writer.clone();
    let runtime_input = input.clone();
    let runtime_clock: Arc<dyn RetryClock> = clock;
    let runtime = thread::spawn(move || {
        run_inner_with_executors_and_clock(
            runtime_input,
            writer,
            profiles,
            move || prompt_profiles.clone(),
            1,
            RuntimeExecutors {
                prompt: executor,
                prewarm: production_prewarm_executor(),
                retry_clock: runtime_clock,
            },
        )
        .expect("run provider");
    });

    // Wait until the failed attempt has crossed the main loop and is owned by
    // the scheduler, then exercise the real control path rather than waiting
    // for its timer.
    wait_for_runtime_frames(&output, |frames| {
        frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseUpdatedReported(update))
                    if update.status.as_ref().is_some_and(|status| status.retry.is_some())
            )
        })
    });
    input.push(encode_frames(&[live_event(
        12,
        Event::UiRetryPrompt(tau_proto::UiRetryPrompt {
            request_id: tau_proto::RetryPromptRequestId::parse("runtime-manual-1")
                .expect("valid retry request id"),
            session_id: "s1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            target_agent_id: Some(
                tau_proto::AgentId::parse("agent-1").expect("valid test agent id"),
            ),
            agent_prompt_id: Some(
                "sp-1"
                    .parse::<tau_proto::AgentPromptId>()
                    .expect("known-safe AgentPromptId must be valid"),
            ),
        }),
    )]));

    let frames = wait_for_runtime_frames(&output, |frames| {
        frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseFinishedReported(finished))
                    if finished.agent_prompt_id.as_str() == "sp-1"
            )
        })
    });
    let submitted = frames
        .iter()
        .filter(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderPromptSubmittedReported(submitted))
                    if submitted.agent_prompt_id.as_str() == "sp-1"
            )
        })
        .count();
    let finished = frames
        .iter()
        .filter(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseFinishedReported(finished))
                    if finished.agent_prompt_id.as_str() == "sp-1"
            )
        })
        .count();
    assert_eq!(submitted, 1);
    assert_eq!(finished, 1);
    assert!(frames.iter().any(|frame| {
        matches!(
            input_event(frame),
            Some(Event::ProviderRetryPromptResultReported(result))
                if result.request_id.as_str() == "runtime-manual-1"
                    && result.status == tau_proto::RetryPromptStatus::Accepted
        )
    }));
    input.push(encode_frames(&[HarnessOutputMessage::Disconnect(
        tau_proto::Disconnect {
            reason: Some("done".to_owned()),
        },
    )]));
    input.close();
    runtime.join().expect("runtime join");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

/// Standalone compaction uses the shared retry scheduler for four delayed
/// retries, then emits one terminal fifth-attempt failure without dispatching
/// a sixth provider attempt.
#[test]
fn standalone_compaction_retry_policy_terminalizes_after_five_attempts() {
    let clock = Arc::new(VirtualRetryClock::new(Instant::now()));
    let input = BlockingInput::default();
    let mut compact_prompt = prompt();
    compact_prompt.operation = tau_proto::PromptOperation::StandaloneCompaction;
    input.push(encode_frames(&[live_event(
        11,
        Event::AgentPromptCreated(compact_prompt),
    )]));
    let attempts = Arc::new(AtomicUsize::new(0));
    let executor_attempts = Arc::clone(&attempts);
    let executor: PromptExecutor = Arc::new(move |execution| {
        executor_attempts.fetch_add(1, Ordering::SeqCst);
        send_worker_message(
            &execution.output_tx,
            &execution.output_waker,
            WorkerMessage::Retry {
                job: execution.job,
                decision: RetryDecision::new(RetryClass::Transport),
                live_detail: None,
                canonical_unauthorized: false,
                terminal_backend: Some(ProviderBackend {
                    kind: ProviderBackendKind::ChatCompletions,
                    base_url: "https://chat.example/v1".to_owned(),
                    transport: ProviderBackendTransport::HttpSse,
                    stale_chain_fallback: false,
                }),
            },
        )
        .expect("return retry outcome");
    });
    let profiles = profiles_with_chatgpt_auth(chatgpt_auth());
    let prompt_profiles = profiles.clone();
    let writer = SharedWriter::default();
    let output = writer.clone();
    let runtime_input = input.clone();
    let runtime_clock: Arc<dyn RetryClock> = clock.clone();
    let runtime = thread::spawn(move || {
        run_inner_with_executors_and_clock(
            runtime_input,
            writer,
            profiles,
            move || prompt_profiles.clone(),
            1,
            RuntimeExecutors {
                prompt: executor,
                prewarm: production_prewarm_executor(),
                retry_clock: runtime_clock,
            },
        )
        .expect("run standalone retry provider");
    });

    for expected_retry_statuses in 1..=4 {
        wait_for_runtime_frames(&output, |frames| {
            frames
                .iter()
                .filter(|frame| {
                    matches!(
                        input_event(frame),
                        Some(Event::ProviderResponseUpdatedReported(update))
                            if update.status.as_ref().is_some_and(|status| status.retry.is_some())
                    )
                })
                .count()
                >= expected_retry_statuses
        });
        clock.advance(Duration::from_secs(60 * 60));
    }
    let frames = wait_for_runtime_frames(&output, |frames| {
        frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseFinishedReported(finished))
                    if finished.stop_reason == ProviderStopReason::Error
                        && finished.provider_attempt.get() == 5
                        && finished.backend.as_ref().is_some_and(|backend| {
                            backend.kind == ProviderBackendKind::ChatCompletions
                                && backend.base_url == "https://chat.example/v1"
                        })
            )
        })
    });
    assert_eq!(attempts.load(Ordering::SeqCst), 5);
    assert_eq!(
        frames
            .iter()
            .filter(|frame| matches!(
                input_event(frame),
                Some(Event::ProviderResponseFinishedReported(_))
            ))
            .count(),
        1
    );
    input.close();
    runtime.join().expect("standalone retry runtime joins");
}

/// Pre-egress Chat cancellation retains the scheduler ordinal while correctly
/// omitting backend metadata.
#[test]
fn pre_egress_chat_cancellation_retains_attempt_without_backend() {
    let prompt = prompt();
    let mut bytes = Vec::new();
    {
        let mut writer = PeerOutputWriter::new(&mut bytes);
        finish_canceled_attempt(
            &prompt.agent_prompt_id,
            &prompt,
            &mut writer,
            false,
            None,
            tau_proto::ProviderAttempt::new(3).expect("attempt"),
        )
        .expect("finish cancellation");
    }
    let frames = decode_frames(&bytes);
    assert!(matches!(
        input_event(&frames[0]),
        Some(Event::ProviderResponseFinishedReported(finished))
            if finished.provider_attempt.get() == 3 && finished.backend.is_none()
    ));
}

/// The fifth transient compaction failure terminalizes that prompt but still
/// extends the shared cooldown that keeps a same-profile peer parked.
#[test]
fn standalone_retry_exhaustion_preserves_shared_peer_cooldown() {
    let clock = Arc::new(VirtualRetryClock::new(Instant::now()));
    let input = BlockingInput::default();
    let mut compact_prompt = prompt();
    compact_prompt.agent_prompt_id = "compact-4290"
        .parse::<tau_proto::AgentPromptId>()
        .expect("compaction prompt id");
    compact_prompt.operation = tau_proto::PromptOperation::StandaloneCompaction;
    let mut peer_prompt = prompt();
    peer_prompt.agent_prompt_id = "peer-3"
        .parse::<tau_proto::AgentPromptId>()
        .expect("peer prompt id");
    input.push(encode_frames(&[
        live_event(11, Event::AgentPromptCreated(compact_prompt)),
        live_event(12, Event::AgentPromptCreated(peer_prompt)),
    ]));
    let compaction_attempts = Arc::new(AtomicUsize::new(0));
    let peer_attempts = Arc::new(AtomicUsize::new(0));
    let (peer_finished_tx, peer_finished_rx) = mpsc::sync_channel(1);
    let executor_compaction_attempts = Arc::clone(&compaction_attempts);
    let executor_peer_attempts = Arc::clone(&peer_attempts);
    let executor: PromptExecutor = Arc::new(move |execution| {
        if execution.job.agent_prompt_id.as_str() == "compact-4290" {
            executor_compaction_attempts.fetch_add(1, Ordering::SeqCst);
            send_worker_message(
                &execution.output_tx,
                &execution.output_waker,
                WorkerMessage::Retry {
                    job: execution.job,
                    decision: RetryDecision::new(RetryClass::Throttle),
                    live_detail: None,
                    canonical_unauthorized: false,
                    terminal_backend: None,
                },
            )
            .expect("return compaction retry");
            return;
        }
        let attempt = executor_peer_attempts.fetch_add(1, Ordering::SeqCst);
        if attempt == 0 {
            send_worker_message(
                &execution.output_tx,
                &execution.output_waker,
                WorkerMessage::Retry {
                    job: execution.job,
                    decision: RetryDecision::new(RetryClass::Throttle),
                    live_detail: None,
                    canonical_unauthorized: false,
                    terminal_backend: None,
                },
            )
            .expect("park peer retry");
            return;
        }
        let mut writer = execution.frame_writer();
        writer
            .write_message(&HarnessInputMessage::emit_transient(
                Event::ProviderResponseFinishedReported(simple_finished(
                    execution.job.agent_prompt_id,
                    execution.job.prompt.agent_id,
                    execution.job.prompt.originator,
                    "peer done",
                )),
            ))
            .expect("finish peer");
        writer.flush().expect("flush peer");
        peer_finished_tx.send(()).expect("report peer finish");
    });
    let profiles = profiles_with_chatgpt_auth(chatgpt_auth());
    let prompt_profiles = profiles.clone();
    let writer = SharedWriter::default();
    let output = writer.clone();
    let runtime_input = input.clone();
    let runtime_clock: Arc<dyn RetryClock> = clock.clone();
    let runtime = thread::spawn(move || {
        run_inner_with_executors_and_clock(
            runtime_input,
            writer,
            profiles,
            move || prompt_profiles.clone(),
            2,
            RuntimeExecutors {
                prompt: executor,
                prewarm: production_prewarm_executor(),
                retry_clock: runtime_clock,
            },
        )
        .expect("run shared-cooldown provider");
    });

    wait_for_runtime_frames(&output, |frames| {
        ["compact-4290", "peer-3"].into_iter().all(|id| {
            frames.iter().any(|frame| {
                matches!(
                    input_event(frame),
                    Some(Event::ProviderResponseUpdatedReported(update))
                        if update.agent_prompt_id.as_str() == id
                            && update.status.as_ref().is_some_and(|status| status.retry.is_some())
                )
            })
        })
    });
    for expected_compaction_statuses in 2..=4 {
        clock.advance(Duration::from_secs(1));
        wait_for_runtime_frames(&output, |frames| {
            frames
                .iter()
                .filter(|frame| matches!(
                    input_event(frame),
                    Some(Event::ProviderResponseUpdatedReported(update))
                        if update.agent_prompt_id.as_str() == "compact-4290"
                            && update.status.as_ref().is_some_and(|status| status.retry.is_some())
                ))
                .count()
                >= expected_compaction_statuses
        });
    }
    clock.advance(Duration::from_secs(1));
    wait_for_runtime_frames(&output, |frames| {
        frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseFinishedReported(finished))
                    if finished.agent_prompt_id.as_str() == "compact-4290"
                        && finished.provider_attempt.get() == 5
            )
        })
    });
    assert_eq!(compaction_attempts.load(Ordering::SeqCst), 5);
    assert_eq!(peer_attempts.load(Ordering::SeqCst), 1);

    clock.advance(Duration::from_secs(4));
    assert!(matches!(
        peer_finished_rx.recv_timeout(Duration::from_millis(50)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    assert_eq!(
        peer_attempts.load(Ordering::SeqCst),
        1,
        "the fifth failure must extend the peer beyond the previous cooldown"
    );
    clock.advance(Duration::from_secs(1));
    peer_finished_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("peer becomes due after the extended cooldown");
    wait_for_runtime_frames(&output, |frames| {
        frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseFinishedReported(finished))
                    if finished.agent_prompt_id.as_str() == "peer-3"
            )
        })
    });
    assert_eq!(peer_attempts.load(Ordering::SeqCst), 2);
    input.close();
    runtime.join().expect("shared-cooldown runtime joins");
}

/// Ensures manual scheduler ownership transfer decrements the delayed count in
/// the main loop, so EOF can finish after the admitted attempt completes.
#[test]
fn manual_retry_transfer_clears_delayed_count_through_main_loop() {
    let clock = Arc::new(VirtualRetryClock::new(Instant::now()));
    let input = BlockingInput::default();
    input.push(encode_frames(&[live_event(
        11,
        Event::AgentPromptCreated(prompt()),
    )]));
    let (attempt_tx, attempt_rx) = mpsc::channel();
    let executor: PromptExecutor = Arc::new(move |execution| {
        let attempt = execution.job.retry_state.attempts;
        attempt_tx.send(attempt).expect("report admitted attempt");
        if attempt == 0 {
            send_worker_message(
                &execution.output_tx,
                &execution.output_waker,
                WorkerMessage::Retry {
                    job: execution.job,
                    decision: RetryDecision::new(RetryClass::Transport),
                    live_detail: None,
                    canonical_unauthorized: false,
                    terminal_backend: None,
                },
            )
            .expect("park first attempt");
        } else {
            let mut writer = execution.frame_writer();
            writer
                .write_message(&HarnessInputMessage::emit_transient(
                    Event::ProviderResponseFinishedReported(simple_finished(
                        execution.job.agent_prompt_id,
                        execution.job.prompt.agent_id,
                        execution.job.prompt.originator,
                        "done",
                    )),
                ))
                .expect("finish manual attempt");
            writer.flush().expect("flush finish");
        }
    });
    let profiles = profiles_with_chatgpt_auth(chatgpt_auth());
    let prompt_profiles = profiles.clone();
    let output = SharedWriter::default();
    let runtime_output = output.clone();
    let runtime_input = input.clone();
    let runtime_clock: Arc<dyn RetryClock> = clock;
    let runtime = thread::spawn(move || {
        run_inner_with_executors_and_clock(
            runtime_input,
            runtime_output,
            profiles,
            move || prompt_profiles.clone(),
            1,
            RuntimeExecutors {
                prompt: executor,
                prewarm: production_prewarm_executor(),
                retry_clock: runtime_clock,
            },
        )
        .expect("run provider");
    });
    assert_eq!(attempt_rx.recv().expect("first attempt"), 0);
    wait_for_runtime_frames(&output, |frames| {
        frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseUpdatedReported(update))
                    if update.status.as_ref().is_some_and(|status| status.retry.is_some())
            )
        })
    });
    input.push(encode_frames(&[live_event(
        12,
        Event::UiRetryPrompt(tau_proto::UiRetryPrompt {
            request_id: tau_proto::RetryPromptRequestId::parse("count-transfer")
                .expect("valid retry request id"),
            session_id: "session-1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            target_agent_id: None,
            agent_prompt_id: Some(
                "sp-1"
                    .parse::<tau_proto::AgentPromptId>()
                    .expect("known-safe AgentPromptId must be valid"),
            ),
        }),
    )]));
    assert_eq!(attempt_rx.recv().expect("manually admitted attempt"), 1);
    input.close();
    runtime.join().expect("runtime exits with no delayed owner");
    let frames = decode_frames(&output.bytes());
    assert!(frames.iter().any(|frame| matches!(
        input_event(frame),
        Some(Event::ProviderRetryPromptResultReported(result))
            if result.request_id.as_str() == "count-transfer"
                && result.status == tau_proto::RetryPromptStatus::Accepted
    )));
}

/// Reproduces the `tau-agent-rrqmwy` quota incident under virtual time.
///
/// This is the Stage 1 acceptance gate: quota display has no scheduler
/// authority, one typed usage-window probe releases its exact generation,
/// attempt-zero peers and a deterministic tool-result continuation progress,
/// and all provider-owned work reaches one terminal without a wall-clock wait.
#[test]
fn rrqmwy_virtual_time_quota_recovery_acceptance() {
    let epoch = Instant::now();
    let clock = Arc::new(VirtualRetryClock::new(epoch));
    let input = BlockingInput::default();
    let mut probe = prompt();
    probe.agent_prompt_id = "probe"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    input.push(encode_frames(&[live_event(
        11,
        Event::AgentPromptCreated(probe),
    )]));

    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
    let executor_calls = Arc::clone(&calls);
    let (completed_tx, completed_rx) = mpsc::channel();
    let executor: PromptExecutor = Arc::new(move |execution| {
        let id = execution.job.agent_prompt_id.to_string();
        executor_calls.lock().expect("call log").push(id.clone());
        if id == "probe" && execution.job.retry_state.attempts == 0 {
            let PromptBackend::Responses(config) = &execution.job.backend else {
                panic!("probe uses canonical Responses profile");
            };
            let profile_identity = quota_profile_identity(config);
            let decision = RetryDecision::new(RetryClass::UsageWindow)
                .with_retry_after(Some(Duration::from_secs(5 * 86_400)));
            assert_eq!(decision.class, RetryClass::UsageWindow);
            assert_eq!(decision.retry_after, Some(Duration::from_secs(5 * 86_400)));
            let output_tx = execution.output_tx.clone();
            let output_waker = execution.output_waker.clone();
            send_worker_message(
                &output_tx,
                &output_waker,
                WorkerMessage::Retry {
                    job: execution.job,
                    decision,
                    live_detail: None,
                    canonical_unauthorized: false,
                    terminal_backend: None,
                },
            )
            .expect("install generated usage-window cooldown");
            for used_basis_points in [10_000, 0] {
                send_worker_message(
                    &output_tx,
                    &output_waker,
                    WorkerMessage::QuotaRolling {
                        model: model_id(CHATGPT_PROVIDER_NAME, "gpt-5.6-sol"),
                        profile_identity,
                        observation: tau_provider_codex::RollingQuotaObservation {
                            windows: vec![tau_provider_codex::QuotaWindowObservation {
                                limit_id: tau_proto::ProviderQuotaLimitId::parse("codex")
                                    .expect("limit id"),
                                window_id: tau_proto::ProviderQuotaWindowId::parse("primary")
                                    .expect("window id"),
                                used_basis_points,
                                window_seconds: Some(tau_proto::QuotaWindowSeconds::new(
                                    5 * 86_400,
                                )),
                                reset_at_unix_seconds: Some(tau_proto::UnixSeconds::new(
                                    2_100_000_000,
                                )),
                                remaining_seconds: None,
                            }],
                            active_limit_id: Some(
                                tau_proto::ProviderQuotaLimitId::parse("codex").expect("limit id"),
                            ),
                            binding_provenance: Some(
                                tau_proto::ProviderQuotaBindingProvenance::TurnEvent,
                            ),
                        },
                        observed_at_unix_ms: tau_proto::UnixMillis::new(now_ms()),
                    },
                )
                .expect("publish display-only quota transition");
            }
            return;
        }
        if id == "continuation" {
            assert_eq!(
                execution
                    .job
                    .prompt
                    .context
                    .blocks
                    .iter()
                    .filter_map(|block| match block {
                        tau_proto::ContextBlock::ToolResults(results) => Some(results.items.len()),
                        _ => None,
                    })
                    .sum::<usize>(),
                1,
                "continuation contains one deterministic tool result"
            );
        }

        let mut finished = simple_finished(
            execution.job.agent_prompt_id.clone(),
            execution.job.prompt.agent_id.clone(),
            execution.job.prompt.originator.clone(),
            "deterministic success",
        );
        finished.error = None;
        finished.stop_reason = if id == "probe" {
            tau_proto::ProviderStopReason::ToolCalls
        } else {
            tau_proto::ProviderStopReason::EndTurn
        };
        if id == "probe" {
            finished.output_items = vec![ContextItem::ToolCall(tau_proto::ToolCallItem {
                call_id: "call-no-side-effect".into(),
                name: tau_proto::ToolName::new("test_no_side_effect"),
                tool_type: tau_proto::ToolType::Function,
                arguments: tau_proto::CborValue::Null,
                raw_arguments_json: Some("null".to_owned()),
                responses_envelope: None,
            })];
        }
        let mut writer = execution.frame_writer();
        writer
            .write_message(&HarnessInputMessage::emit_transient(
                Event::ProviderResponseFinishedReported(finished),
            ))
            .expect("successful terminal");
        writer.flush().expect("flush successful terminal");
        completed_tx.send(id).expect("report successful attempt");
    });

    let mut profiles = profiles_with_chatgpt_auth(chatgpt_auth());
    profiles.providers.insert(
        ProviderName::new("healthy"),
        BuiltinProviderProfile::ChatCompletions(ChatCompletionsProvider {
            base_url: "https://healthy.invalid/v1".to_owned(),
            api_key: "fixture-key".to_owned(),
            models: vec![chat_model("model")],
            ..ChatCompletionsProvider::default()
        }),
    );
    let prompt_profiles = profiles.clone();
    let output = SharedWriter::default();
    let runtime_output = output.clone();
    let runtime_input = input.clone();
    let runtime_clock: Arc<dyn RetryClock> = clock.clone();
    let (runtime_done_tx, runtime_done_rx) = mpsc::sync_channel(0);
    let runtime = thread::spawn(move || {
        run_inner_with_executors_and_clock(
            runtime_input,
            runtime_output,
            profiles,
            move || prompt_profiles.clone(),
            2,
            RuntimeExecutors {
                prompt: executor,
                prewarm: production_prewarm_executor(),
                retry_clock: runtime_clock,
            },
        )
        .expect("run provider");
        runtime_done_tx.send(()).expect("report provider shutdown");
    });

    wait_for_runtime_frames(&output, |frames| {
        frames
            .iter()
            .filter(|frame| {
                matches!(
                    input_event(frame),
                    Some(Event::ProviderQuotaPatchReported(_))
                )
            })
            .count()
            == 2
    });
    let frames = decode_frames(&output.bytes());
    assert!(
        frames
            .iter()
            .filter_map(|frame| {
                let HarnessInputMessage::Emit(emit) = frame else {
                    return None;
                };
                matches!(
                    emit.event.as_ref(),
                    Event::ProviderQuotaReplaceReported(_)
                        | Event::ProviderQuotaPatchReported(_)
                        | Event::ProviderQuotaClearReported(_)
                )
                .then_some(!emit.persist)
            })
            .all(std::convert::identity),
        "first-party quota reports must set explicit persist=false metadata"
    );
    let status = frames
        .iter()
        .find_map(|frame| match input_event(frame) {
            Some(Event::ProviderResponseUpdatedReported(update))
                if update.agent_prompt_id.as_str() == "probe" =>
            {
                update
                    .status
                    .as_ref()
                    .filter(|status| status.retry.is_some())
            }
            _ => None,
        })
        .expect("probe retry status");
    let retry = status.retry.as_ref().expect("structured retry status");
    assert_eq!(
        retry.category,
        tau_proto::ProviderRetryCategory::UsageWindow
    );
    assert!(retry.next_retry_delay_secs < 60);
    assert!(!status.text.contains("5d"));
    assert!(!status.text.contains("432000"));
    assert_eq!(calls.lock().expect("call log").as_slice(), ["probe"]);

    let mut peer_one = prompt();
    peer_one.agent_prompt_id = "peer-1"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    let mut peer_two = prompt();
    peer_two.agent_prompt_id = "peer-2"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    let mut unrelated = prompt();
    unrelated.agent_prompt_id = "unrelated"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    unrelated.model = model_id("healthy", "model");
    input.push(encode_frames(&[
        live_event(12, Event::AgentPromptCreated(peer_one)),
        live_event(13, Event::AgentPromptCreated(peer_two)),
        live_event(14, Event::AgentPromptCreated(unrelated)),
    ]));
    assert_eq!(
        completed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("unrelated provider progresses"),
        "unrelated"
    );
    wait_for_runtime_frames(&output, |frames| {
        ["peer-1", "peer-2"].iter().all(|id| {
            frames.iter().any(|frame| {
                matches!(
                    input_event(frame),
                    Some(Event::ProviderResponseUpdatedReported(update))
                        if update.agent_prompt_id.as_str() == *id
                            && update.status.as_ref().is_some_and(|status| {
                                status.retry.as_ref().is_some_and(|retry| retry.attempt == 0)
                            })
                )
            })
        })
    });
    assert_eq!(
        calls.lock().expect("call log").as_slice(),
        ["probe", "unrelated"]
    );

    let retry = |request_id: &str| {
        Event::UiRetryPrompt(tau_proto::UiRetryPrompt {
            request_id: tau_proto::RetryPromptRequestId::parse(request_id)
                .expect("retry request id"),
            session_id: "session-1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            target_agent_id: None,
            agent_prompt_id: Some(
                "probe"
                    .parse::<tau_proto::AgentPromptId>()
                    .expect("known-safe AgentPromptId must be valid"),
            ),
        })
    };
    input.push(encode_frames(&[
        live_event(15, retry("one-probe")),
        live_event(16, retry("duplicate-probe")),
    ]));
    assert_eq!(
        completed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("manual probe succeeds"),
        "probe"
    );
    let probe_frames = wait_for_runtime_frames(&output, |frames| {
        frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderRetryPromptResultReported(result))
                    if result.request_id.as_str() == "one-probe"
                        && result.status == tau_proto::RetryPromptStatus::Accepted
            )
        }) && frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderRetryPromptResultReported(result))
                    if result.request_id.as_str() == "duplicate-probe"
                        && result.status == tau_proto::RetryPromptStatus::NotParked
            )
        }) && frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseFinishedReported(finished))
                    if finished.agent_prompt_id.as_str() == "probe"
                        && finished.stop_reason == tau_proto::ProviderStopReason::ToolCalls
                        && finished.error.is_none()
                        && finished.failure_kind.is_none()
            )
        })
    });
    assert_eq!(
        probe_frames
            .iter()
            .filter(|frame| matches!(
                input_event(frame),
                Some(Event::ProviderRetryPromptResultReported(_))
            ))
            .count(),
        2,
        "both competing :retry controls resolve exactly once"
    );

    // The harness-owned Stage 3 gate covers routing/renderer composition. Here
    // a deterministic no-side-effect result is paired to the validated call and
    // submitted as a production provider continuation.
    let mut continuation = prompt();
    continuation.agent_prompt_id = "continuation"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    continuation
        .context
        .blocks
        .push(tau_proto::ContextBlock::ToolResults(
            tau_proto::ToolResultsBlock {
                items: vec![tau_proto::ToolResultItem {
                    presentation: Default::default(),
                    call_id: "call-no-side-effect".into(),
                    tool_type: tau_proto::ToolType::Function,
                    status: tau_proto::ToolResultStatus::Success,
                    output: tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Text(
                        "fixture-result".to_owned(),
                    )),
                    provider_content: Vec::new(),
                }],
            },
        ));
    input.push(encode_frames(&[live_event(
        17,
        Event::AgentPromptCreated(continuation),
    )]));
    assert_eq!(
        completed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("continuation progresses"),
        "continuation"
    );

    clock.advance(Duration::from_secs(RESET_BOUNDARY_JITTER_MAX.as_secs() + 1));
    let mut released = path_std_collections::BTreeSet::new();
    while released.len() < 2 {
        released.insert(
            completed_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("virtual-time released peer"),
        );
    }
    assert_eq!(
        released,
        std::collections::BTreeSet::from(["peer-1".to_owned(), "peer-2".to_owned()])
    );

    wait_for_runtime_frames(&output, |frames| {
        ["peer-1", "peer-2", "unrelated", "continuation"]
            .iter()
            .all(|id| {
                frames.iter().any(|frame| {
                    matches!(
                        input_event(frame),
                        Some(Event::ProviderResponseFinishedReported(finished))
                            if finished.agent_prompt_id.as_str() == *id
                                && finished.stop_reason == tau_proto::ProviderStopReason::EndTurn
                                && finished.error.is_none()
                                && finished.failure_kind.is_none()
                    )
                })
            })
    });
    input.close();
    runtime_done_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("provider and scheduler shut down after draining");
    runtime
        .join()
        .expect("provider drains all active and delayed work");
    let frames = decode_frames(&output.bytes());
    assert_eq!(
        frames
            .iter()
            .filter(|frame| matches!(
                input_event(frame),
                Some(Event::ProviderRetryPromptResultReported(_))
            ))
            .count(),
        2,
        "manual controls have no duplicate late result"
    );
    for id in ["probe", "peer-1", "peer-2", "unrelated", "continuation"] {
        assert_eq!(
            frames
                .iter()
                .filter(|frame| matches!(
                    input_event(frame),
                    Some(Event::ProviderPromptSubmittedReported(submitted))
                        if submitted.agent_prompt_id.as_str() == id
                ))
                .count(),
            1,
            "{id} submitted exactly once"
        );
        assert_eq!(
            frames
                .iter()
                .filter(|frame| matches!(
                    input_event(frame),
                    Some(Event::ProviderResponseFinishedReported(finished))
                        if finished.agent_prompt_id.as_str() == id
                ))
                .count(),
            1,
            "{id} terminated exactly once"
        );
    }
    assert_eq!(
        frames
            .iter()
            .filter_map(|frame| match input_event(frame) {
                Some(Event::ProviderResponseFinishedReported(finished)) =>
                    Some(&finished.output_items),
                _ => None,
            })
            .flatten()
            .filter(|item| matches!(item, ContextItem::ToolCall(_)))
            .count(),
        1,
        "the probe emits one tool call without duplication"
    );
    assert!(!frames.iter().any(|frame| matches!(
        input_event(frame),
        Some(Event::ProviderResponseUpdatedReported(update))
            if update.agent_prompt_id.as_str() == "continuation"
                && update.status.as_ref().is_some_and(|status| status.retry.is_some())
    )));
    let calls = calls.lock().expect("call log");
    assert_eq!(calls.iter().filter(|id| id.as_str() == "probe").count(), 2);
    for id in ["peer-1", "peer-2", "unrelated", "continuation"] {
        assert_eq!(calls.iter().filter(|call| call.as_str() == id).count(), 1);
    }
}

/// Ensures best-effort quota telemetry cannot clear inference cooldown state or
/// immediately admit a same-provider peer before its generated shared boundary.
#[test]
fn quota_telemetry_does_not_release_shared_inference_cooldown() {
    let clock = Arc::new(VirtualRetryClock::new(Instant::now()));
    let input = BlockingInput::default();
    input.push(encode_frames(&[live_event(
        11,
        Event::AgentPromptCreated(prompt()),
    )]));
    let (attempt_tx, attempt_rx) = mpsc::channel();
    let executor: PromptExecutor = Arc::new(move |execution| {
        attempt_tx
            .send(execution.job.agent_prompt_id.to_string())
            .expect("report attempt");
        let model = execution.job.prompt.model.clone();
        let PromptBackend::Responses(config) = &execution.job.backend else {
            panic!("configured Responses backend");
        };
        let profile_identity = quota_profile_identity(config);
        let output_tx = execution.output_tx.clone();
        let output_waker = execution.output_waker.clone();
        send_worker_message(
            &output_tx,
            &output_waker,
            WorkerMessage::Retry {
                job: execution.job,
                decision: RetryDecision::new(RetryClass::UsageWindow)
                    .with_retry_after(Some(Duration::from_secs(86_400))),
                live_detail: None,
                canonical_unauthorized: false,
                terminal_backend: None,
            },
        )
        .expect("park usage failure");
        send_worker_message(
            &output_tx,
            &output_waker,
            WorkerMessage::QuotaRolling {
                model,
                profile_identity,
                observation: tau_provider_codex::RollingQuotaObservation {
                    windows: vec![tau_provider_codex::QuotaWindowObservation {
                        limit_id: tau_proto::ProviderQuotaLimitId::parse("codex")
                            .expect("limit id"),
                        window_id: tau_proto::ProviderQuotaWindowId::parse("primary")
                            .expect("window id"),
                        used_basis_points: 0,
                        window_seconds: Some(tau_proto::QuotaWindowSeconds::new(604_800)),
                        reset_at_unix_seconds: Some(tau_proto::UnixSeconds::new(2_100_000_000)),
                        remaining_seconds: None,
                    }],
                    active_limit_id: Some(
                        tau_proto::ProviderQuotaLimitId::parse("codex").expect("limit id"),
                    ),
                    binding_provenance: Some(tau_proto::ProviderQuotaBindingProvenance::TurnEvent),
                },
                observed_at_unix_ms: tau_proto::UnixMillis::new(now_ms()),
            },
        )
        .expect("inject quota telemetry");
    });
    let profiles = profiles_with_chatgpt_auth(chatgpt_auth());
    let prompt_profiles = profiles.clone();
    let output = SharedWriter::default();
    let runtime_output = output.clone();
    let runtime_input = input.clone();
    let runtime_clock: Arc<dyn RetryClock> = clock;
    let runtime = thread::spawn(move || {
        run_inner_with_executors_and_clock(
            runtime_input,
            runtime_output,
            profiles,
            move || prompt_profiles.clone(),
            2,
            RuntimeExecutors {
                prompt: executor,
                prewarm: production_prewarm_executor(),
                retry_clock: runtime_clock,
            },
        )
        .expect("run provider");
    });
    assert_eq!(attempt_rx.recv().expect("initial attempt"), "sp-1");
    wait_for_runtime_frames(&output, |frames| {
        frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseUpdatedReported(update))
                    if update.agent_prompt_id.as_str() == "sp-1"
                        && update.status.as_ref().is_some_and(|status| status.retry.is_some())
            )
        })
    });
    wait_for_runtime_frames(&output, |frames| {
        frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderQuotaPatchReported(_))
            )
        })
    });
    let mut peer = prompt();
    peer.agent_prompt_id = "telemetry-peer"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    input.push(encode_frames(&[live_event(
        12,
        Event::AgentPromptCreated(peer),
    )]));
    wait_for_runtime_frames(&output, |frames| {
        frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseUpdatedReported(update))
                    if update.agent_prompt_id.as_str() == "telemetry-peer"
                        && update.status.as_ref().is_some_and(|status| {
                            status.retry.as_ref().is_some_and(|retry| retry.attempt == 0)
                        })
            )
        })
    });
    input.close();
    runtime.join().expect("provider exits");
}

/// Ensures ordered session shutdown wins against a following manual request:
/// the parked job is canceled once, cannot be admitted, and is never
/// resurrected.
#[test]
fn shutdown_then_manual_retry_is_terminal_once_without_dispatch() {
    let input = BlockingInput::default();
    input.push(encode_frames(&[live_event(
        11,
        Event::AgentPromptCreated(prompt()),
    )]));
    let attempts = Arc::new(AtomicUsize::new(0));
    let executor_attempts = Arc::clone(&attempts);
    let executor: PromptExecutor = Arc::new(move |execution| {
        executor_attempts.fetch_add(1, Ordering::SeqCst);
        send_worker_message(
            &execution.output_tx,
            &execution.output_waker,
            WorkerMessage::Retry {
                job: execution.job,
                decision: RetryDecision::new(RetryClass::Transport),
                live_detail: None,
                canonical_unauthorized: false,
                terminal_backend: None,
            },
        )
        .expect("park attempt");
    });
    let profiles = profiles_with_chatgpt_auth(chatgpt_auth());
    let prompt_profiles = profiles.clone();
    let output = SharedWriter::default();
    let runtime_output = output.clone();
    let runtime_input = input.clone();
    let runtime = thread::spawn(move || {
        run_inner_with_prompt_executor(
            runtime_input,
            runtime_output,
            profiles,
            move || prompt_profiles.clone(),
            1,
            executor,
        )
        .expect("run provider");
    });
    wait_for_runtime_frames(&output, |frames| {
        frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseUpdatedReported(update))
                    if update.status.as_ref().is_some_and(|status| status.retry.is_some())
            )
        })
    });
    input.push(encode_frames(&[
        live_event(
            12,
            Event::SessionShutdown(tau_proto::SessionShutdown {
                session_id: "session-1"
                    .parse::<tau_proto::SessionId>()
                    .expect("known-safe SessionId must be valid"),
            }),
        ),
        live_event(
            13,
            Event::UiRetryPrompt(tau_proto::UiRetryPrompt {
                request_id: tau_proto::RetryPromptRequestId::parse("after-shutdown")
                    .expect("valid retry request id"),
                session_id: "session-1"
                    .parse::<tau_proto::SessionId>()
                    .expect("known-safe SessionId must be valid"),
                target_agent_id: None,
                agent_prompt_id: Some(
                    "sp-1"
                        .parse::<tau_proto::AgentPromptId>()
                        .expect("known-safe AgentPromptId must be valid"),
                ),
            }),
        ),
    ]));
    let frames = wait_for_runtime_frames(&output, |frames| {
        frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderRetryPromptResultReported(result))
                    if result.request_id.as_str() == "after-shutdown"
            )
        })
    });
    input.close();
    runtime.join().expect("shutdown runtime");
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(
        frames
            .iter()
            .filter(|frame| matches!(
                input_event(frame),
                Some(Event::ProviderPromptSubmittedReported(submitted))
                    if submitted.agent_prompt_id.as_str() == "sp-1"
            ))
            .count(),
        1
    );
    assert_eq!(
        frames
            .iter()
            .filter(|frame| matches!(
                input_event(frame),
                Some(Event::ProviderResponseFinishedReported(finished))
                    if finished.agent_prompt_id.as_str() == "sp-1"
            ))
            .count(),
        1
    );
    assert!(frames.iter().any(|frame| matches!(
        input_event(frame),
        Some(Event::ProviderRetryPromptResultReported(result))
            if result.request_id.as_str() == "after-shutdown"
                && result.status == tau_proto::RetryPromptStatus::NotParked
    )));
}

/// Ensures clicking retry does not alter attempt accounting: a retryable manual
/// attempt re-parks at the next normal backoff step and later completes once.
#[test]
fn manual_retry_failure_reparks_with_normal_accounting_then_finishes_once() {
    let input = BlockingInput::default();
    input.push(encode_frames(&[live_event(
        11,
        Event::AgentPromptCreated(prompt()),
    )]));
    let (attempt_tx, attempt_rx) = mpsc::channel();
    let executor: PromptExecutor = Arc::new(move |execution| {
        let attempt = execution.job.retry_state.attempts;
        attempt_tx.send(attempt).expect("report attempt");
        if attempt < 2 {
            send_worker_message(
                &execution.output_tx,
                &execution.output_waker,
                WorkerMessage::Retry {
                    job: execution.job,
                    decision: RetryDecision::new(RetryClass::Transport),
                    live_detail: None,
                    canonical_unauthorized: false,
                    terminal_backend: None,
                },
            )
            .expect("return retry");
        } else {
            let mut writer = execution.frame_writer();
            writer
                .write_message(&HarnessInputMessage::emit_transient(
                    Event::ProviderResponseFinishedReported(simple_finished(
                        execution.job.agent_prompt_id,
                        execution.job.prompt.agent_id,
                        execution.job.prompt.originator,
                        "done",
                    )),
                ))
                .expect("finish");
            writer.flush().expect("flush finish");
        }
    });
    let profiles = profiles_with_chatgpt_auth(chatgpt_auth());
    let prompt_profiles = profiles.clone();
    let output = SharedWriter::default();
    let runtime_output = output.clone();
    let runtime_input = input.clone();
    let runtime = thread::spawn(move || {
        run_inner_with_prompt_executor(
            runtime_input,
            runtime_output,
            profiles,
            move || prompt_profiles.clone(),
            1,
            executor,
        )
        .expect("run provider");
    });
    assert_eq!(attempt_rx.recv().expect("initial attempt"), 0);
    for (recorded_at, request_id, expected_attempt) in
        [(12, "manual-first", 1_u64), (13, "manual-second", 2_u64)]
    {
        wait_for_runtime_frames(&output, |frames| {
            frames
                .iter()
                .filter(|frame| {
                    matches!(
                        input_event(frame),
                        Some(Event::ProviderResponseUpdatedReported(update))
                            if update.status.as_ref().is_some_and(|status| status.retry.is_some())
                    )
                })
                .count()
                >= expected_attempt as usize
        });
        input.push(encode_frames(&[live_event(
            recorded_at,
            Event::UiRetryPrompt(tau_proto::UiRetryPrompt {
                request_id: tau_proto::RetryPromptRequestId::parse(request_id)
                    .expect("valid retry request id"),
                session_id: "session-1"
                    .parse::<tau_proto::SessionId>()
                    .expect("known-safe SessionId must be valid"),
                target_agent_id: None,
                agent_prompt_id: Some(
                    "sp-1"
                        .parse::<tau_proto::AgentPromptId>()
                        .expect("known-safe AgentPromptId must be valid"),
                ),
            }),
        )]));
        assert_eq!(attempt_rx.recv().expect("manual attempt"), expected_attempt);
    }
    let frames = wait_for_runtime_frames(&output, |frames| {
        frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseFinishedReported(finished))
                    if finished.agent_prompt_id.as_str() == "sp-1"
            )
        })
    });
    let retry_attempts = frames
        .iter()
        .filter_map(|frame| match input_event(frame) {
            Some(Event::ProviderResponseUpdatedReported(update)) => update
                .status
                .as_ref()?
                .retry
                .as_ref()
                .map(|retry| retry.attempt),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(retry_attempts, vec![1, 2]);
    assert_eq!(
        frames
            .iter()
            .filter(|frame| matches!(
                input_event(frame),
                Some(Event::ProviderResponseFinishedReported(finished))
                    if finished.agent_prompt_id.as_str() == "sp-1"
            ))
            .count(),
        1
    );
    input.close();
    runtime.join().expect("runtime join");
}

/// A typed context-window rejection bypasses the effectively unlimited logical
/// retry scheduler and emits one final response with no retry status.
#[test]
fn context_window_rejection_finishes_once_without_retry_status() {
    let input = BlockingInput::default();
    input.push(encode_frames(&[live_event(
        11,
        Event::AgentPromptCreated(prompt()),
    )]));
    let attempts = Arc::new(AtomicUsize::new(0));
    let executor_attempts = Arc::clone(&attempts);
    let executor: PromptExecutor = Arc::new(move |execution| {
        executor_attempts.fetch_add(1, Ordering::SeqCst);
        let mut writer = execution.frame_writer();
        let mut finished = simple_finished(
            execution.job.agent_prompt_id,
            execution.job.prompt.agent_id,
            execution.job.prompt.originator,
            "context window exceeded",
        );
        finished.failure_kind = Some(tau_proto::ProviderFailureKind::ContextWindowExceeded);
        writer
            .write_message(&HarnessInputMessage::emit_transient(
                Event::ProviderResponseFinishedReported(finished),
            ))
            .expect("terminal frame");
        writer.flush().expect("flush terminal frame");
    });
    let profiles = profiles_with_chatgpt_auth(chatgpt_auth());
    let prompt_profiles = profiles.clone();
    let writer = SharedWriter::default();
    let output = writer.clone();
    let runtime_input = input.clone();
    let runtime = thread::spawn(move || {
        run_inner_with_prompt_executor(
            runtime_input,
            writer,
            profiles,
            move || prompt_profiles.clone(),
            1,
            executor,
        )
        .expect("run provider");
    });

    let frames = wait_for_runtime_frames(&output, |frames| {
        frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseFinishedReported(finished))
                    if finished.agent_prompt_id.as_str() == "sp-1"
            )
        })
    });
    let terminal = frames
        .iter()
        .filter_map(|frame| match input_event(frame) {
            Some(Event::ProviderResponseFinishedReported(finished))
                if finished.agent_prompt_id.as_str() == "sp-1" =>
            {
                Some(finished)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(terminal.len(), 1);
    assert_eq!(
        terminal[0].failure_kind,
        Some(tau_proto::ProviderFailureKind::ContextWindowExceeded)
    );
    assert!(!frames.iter().any(|frame| matches!(
        input_event(frame),
        Some(Event::ProviderResponseUpdatedReported(update)) if update.status.is_some()
    )));
    input.push(encode_frames(&[HarnessOutputMessage::Disconnect(
        tau_proto::Disconnect {
            reason: Some("done".to_owned()),
        },
    )]));
    input.close();
    runtime.join().expect("runtime join");
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

/// Proves four far-future account-limit retries release all four bounded worker
/// permits so an unrelated provider runs with no second attempt before
/// shutdown.
#[test]
fn four_delayed_prompts_release_capacity_for_an_unrelated_provider() {
    let input = BlockingInput::default();
    let mut frames = Vec::new();
    for index in 1..=4 {
        let mut limited = prompt();
        limited.agent_prompt_id = format!("limited-{index}")
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid");
        limited.model.provider = ProviderName::new("limited");
        frames.push(live_event(10 + index, Event::AgentPromptCreated(limited)));
    }
    let mut healthy = prompt();
    healthy.agent_prompt_id = "healthy"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    healthy.model.provider = ProviderName::new("healthy");
    frames.push(live_event(20, Event::AgentPromptCreated(healthy)));
    input.push(encode_frames(&frames));

    let attempts = Arc::new(Mutex::new(
        std::collections::BTreeMap::<String, usize>::new(),
    ));
    let limited_barrier = Arc::new((Mutex::new(0_usize), Condvar::new()));
    let executor_barrier = Arc::clone(&limited_barrier);
    let (healthy_tx, healthy_rx) = mpsc::sync_channel(1);
    let executor_attempts = Arc::clone(&attempts);
    let executor: PromptExecutor = Arc::new(move |execution| {
        *executor_attempts
            .lock()
            .expect("attempt map")
            .entry(execution.job.agent_prompt_id.to_string())
            .or_default() += 1;
        if execution
            .job
            .agent_prompt_id
            .as_str()
            .starts_with("limited-")
        {
            let (lock, cv) = &*executor_barrier;
            let mut started = lock.lock().expect("limited barrier");
            *started += 1;
            cv.notify_all();
            while *started < 4 {
                started = cv.wait(started).expect("wait for four limited workers");
            }
            drop(started);
            send_worker_message(
                &execution.output_tx,
                &execution.output_waker,
                WorkerMessage::Retry {
                    job: execution.job,
                    decision: RetryDecision::new(RetryClass::Account)
                        .with_retry_after(Some(Duration::from_secs(86_400))),
                    live_detail: None,
                    canonical_unauthorized: false,
                    terminal_backend: None,
                },
            )
            .expect("park limited prompt");
            return;
        }
        let mut writer = execution.frame_writer();
        writer
            .write_message(&HarnessInputMessage::emit_transient(
                Event::ProviderResponseFinishedReported(simple_finished(
                    execution.job.agent_prompt_id,
                    execution.job.prompt.agent_id,
                    execution.job.prompt.originator,
                    "healthy done",
                )),
            ))
            .expect("healthy finished");
        writer.flush().expect("flush healthy finish");
        healthy_tx.send(()).expect("report healthy completion");
    });
    let profiles = profiles_with_chatgpt_auth(chatgpt_auth());
    let prompt_profiles = profiles.clone();
    let writer = SharedWriter::default();
    let output = writer.clone();
    let runtime_input = input.clone();
    let runtime = thread::spawn(move || {
        run_inner_with_prompt_executor(
            runtime_input,
            writer,
            profiles,
            move || prompt_profiles.clone(),
            4,
            executor,
        )
        .expect("run provider");
    });

    healthy_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("unrelated fifth prompt was wedged behind delayed work");
    let observed = attempts.lock().expect("attempt map");
    for index in 1..=4 {
        assert_eq!(
            observed.get(&format!("limited-{index}")),
            Some(&1),
            "far-future delayed prompt must not execute another attempt"
        );
    }
    assert_eq!(observed.get("healthy"), Some(&1));
    drop(observed);

    input.push(encode_frames(&[HarnessOutputMessage::Disconnect(
        tau_proto::Disconnect {
            reason: Some("done".to_owned()),
        },
    )]));
    input.close();
    runtime.join().expect("runtime join");
    let decoded = decode_frames(&output.bytes());
    assert_eq!(
        decoded
            .iter()
            .filter(|frame| matches!(
                input_event(frame),
                Some(Event::ProviderPromptSubmittedReported(submitted))
                    if submitted.agent_prompt_id.as_str() == "healthy"
            ))
            .count(),
        1
    );
    assert_eq!(
        decoded
            .iter()
            .filter(|frame| matches!(
                input_event(frame),
                Some(Event::ProviderResponseFinishedReported(finished))
                    if finished.agent_prompt_id.as_str() == "healthy"
            ))
            .count(),
        1
    );
}

/// Verifies every due attempt re-resolves mutable profile state while retaining
/// the startup-selected Responses mode: repaired credentials replace stale
/// captures, an opposite on-disk mode edit is ignored, deletion becomes
/// Unavailable, and a re-added profile is resolved as a fresh identity.
#[test]
fn delayed_retry_reloads_repaired_and_deleted_profile_state() {
    let input = BlockingInput::default();
    input.push(encode_frames(&[live_event(
        11,
        Event::AgentPromptCreated(prompt()),
    )]));
    let old = OpenAiAuth {
        access_token: "old-token".to_owned(),
        ..chatgpt_auth()
    };
    let fresh = OpenAiAuth {
        access_token: "fresh-token".to_owned(),
        account_id: Some("account".to_owned()),
        ..chatgpt_auth()
    };
    let mut startup_profiles = profiles_with_chatgpt_auth(old.clone());
    let BuiltinProviderProfile::Chatgpt(startup_profile) = startup_profiles
        .providers
        .get_mut(&ProviderName::new(CHATGPT_PROVIDER_NAME))
        .expect("startup ChatGPT profile")
    else {
        unreachable!()
    };
    startup_profile.responses_lite_compatibility = true;
    let mutable_profiles = Arc::new(Mutex::new(startup_profiles.clone()));
    let profiles_for_loader = Arc::clone(&mutable_profiles);
    let profiles_for_executor = Arc::clone(&mutable_profiles);
    let attempts = Arc::new(AtomicUsize::new(0));
    let executor_attempts = Arc::clone(&attempts);
    let (finished_tx, finished_rx) = mpsc::sync_channel(1);
    let executor: PromptExecutor = Arc::new(move |execution| {
        let attempt = executor_attempts.fetch_add(1, Ordering::SeqCst);
        match (attempt, &execution.job.backend) {
            (0, PromptBackend::Responses(config)) => {
                assert!(config.credentials_match("old-token", Some("account")));
                assert_eq!(config.mode(), CodexMode::LiteCompatibility);
                *profiles_for_executor.lock().expect("mutable profiles") =
                    profiles_with_chatgpt_auth(fresh.clone());
            }
            (1, PromptBackend::Responses(config)) => {
                assert!(config.credentials_match("fresh-token", Some("account")));
                assert_eq!(
                    config.mode(),
                    CodexMode::LiteCompatibility,
                    "retry must retain the startup mode after a standard-mode disk edit"
                );
                *profiles_for_executor.lock().expect("mutable profiles") =
                    BuiltinProviderProfiles::default();
            }
            (
                2,
                PromptBackend::Unavailable {
                    login_required: None,
                },
            ) => {
                *profiles_for_executor.lock().expect("mutable profiles") =
                    profiles_with_chatgpt_auth(fresh.clone());
            }
            (3, PromptBackend::Responses(config)) => {
                assert!(config.credentials_match("fresh-token", Some("account")));
                let mut writer = execution.frame_writer();
                writer
                    .write_message(&HarnessInputMessage::emit_transient(
                        Event::ProviderResponseFinishedReported(simple_finished(
                            execution.job.agent_prompt_id,
                            execution.job.prompt.agent_id,
                            execution.job.prompt.originator,
                            "observed unavailable",
                        )),
                    ))
                    .expect("finish after re-addition");
                writer.flush().expect("flush re-added finish");
                finished_tx.send(()).expect("report finish");
                return;
            }
            _ => panic!("unexpected attempt/backend combination: {attempt}"),
        }
        send_worker_message(
            &execution.output_tx,
            &execution.output_waker,
            WorkerMessage::Retry {
                job: execution.job,
                decision: RetryDecision::new(RetryClass::Transport),
                live_detail: None,
                canonical_unauthorized: false,
                terminal_backend: None,
            },
        )
        .expect("schedule profile reload");
    });
    let writer = SharedWriter::default();
    let output = writer.clone();
    let runtime_input = input.clone();
    let runtime = thread::spawn(move || {
        run_inner_with_prompt_executor(
            runtime_input,
            writer,
            startup_profiles,
            move || profiles_for_loader.lock().expect("profile loader").clone(),
            1,
            executor,
        )
        .expect("run provider");
    });

    finished_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("profile reload sequence did not finish");
    input.push(encode_frames(&[HarnessOutputMessage::Disconnect(
        tau_proto::Disconnect {
            reason: Some("done".to_owned()),
        },
    )]));
    input.close();
    runtime.join().expect("runtime join");
    assert_eq!(attempts.load(Ordering::SeqCst), 4);
    let frames = decode_frames(&output.bytes());
    assert_eq!(
        frames
            .iter()
            .filter(|frame| matches!(
                input_event(frame),
                Some(Event::ProviderPromptSubmittedReported(submitted))
                    if submitted.agent_prompt_id.as_str() == "sp-1"
            ))
            .count(),
        1
    );
    assert_eq!(
        frames
            .iter()
            .filter(|frame| matches!(
                input_event(frame),
                Some(Event::ProviderResponseFinishedReported(finished))
                    if finished.agent_prompt_id.as_str() == "sp-1"
            ))
            .count(),
        1
    );
}

/// Ensures tentative output from a failed attempt is visibly cleared and never
/// appears in the single durable response produced by the successful attempt.
#[test]
fn retry_clears_failed_attempt_output_before_durable_success() {
    let input = BlockingInput::default();
    input.push(encode_frames(&[live_event(
        11,
        Event::AgentPromptCreated(prompt()),
    )]));
    let attempts = Arc::new(AtomicUsize::new(0));
    let executor_attempts = Arc::clone(&attempts);
    let (finished_tx, finished_rx) = mpsc::sync_channel(1);
    let executor: PromptExecutor = Arc::new(move |execution| {
        if executor_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            let mut writer = execution.frame_writer();
            writer
                .write_message(&HarnessInputMessage::emit_transient(
                    Event::ProviderResponseUpdatedReported(ProviderResponseUpdated {
                        agent_prompt_id: execution.job.agent_prompt_id.clone(),
                        agent_id: execution.job.prompt.agent_id.clone(),
                        deltas: vec![tau_proto::ProviderResponseTextDelta::Message {
                            output_index: 0,
                            text: "attempt-one-tentative".to_owned(),
                            phase: None,
                        }],
                        compaction: None,
                        status: None,
                        response_stats: None,
                        originator: execution.job.prompt.originator.clone(),
                    }),
                ))
                .expect("tentative update");
            writer.flush().expect("flush tentative update");
            send_worker_message(
                &execution.output_tx,
                &execution.output_waker,
                WorkerMessage::Retry {
                    job: execution.job,
                    decision: RetryDecision::new(RetryClass::Transport),
                    live_detail: None,
                    canonical_unauthorized: false,
                    terminal_backend: None,
                },
            )
            .expect("schedule retry after partial output");
            return;
        }
        let mut finished = simple_finished(
            execution.job.agent_prompt_id.clone(),
            execution.job.prompt.agent_id.clone(),
            execution.job.prompt.originator.clone(),
            "unused",
        );
        finished.error = None;
        finished.stop_reason = tau_proto::ProviderStopReason::EndTurn;
        finished.output_items = vec![ContextItem::Message(tau_proto::MessageItem {
            role: tau_proto::ContextRole::Assistant,
            content: vec![tau_proto::ContentPart::Text {
                text: "attempt-two-durable".to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        })];
        let mut writer = execution.frame_writer();
        writer
            .write_message(&HarnessInputMessage::emit_transient(
                Event::ProviderResponseFinishedReported(finished),
            ))
            .expect("durable finish");
        writer.flush().expect("flush durable finish");
        finished_tx.send(()).expect("report durable finish");
    });
    let profiles = profiles_with_chatgpt_auth(chatgpt_auth());
    let prompt_profiles = profiles.clone();
    let writer = SharedWriter::default();
    let output = writer.clone();
    let runtime_input = input.clone();
    let runtime = thread::spawn(move || {
        run_inner_with_prompt_executor(
            runtime_input,
            writer,
            profiles,
            move || prompt_profiles.clone(),
            1,
            executor,
        )
        .expect("run provider");
    });

    finished_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("retry did not reach durable success");
    input.push(encode_frames(&[HarnessOutputMessage::Disconnect(
        tau_proto::Disconnect {
            reason: Some("done".to_owned()),
        },
    )]));
    input.close();
    runtime.join().expect("runtime join");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    let frames = decode_frames(&output.bytes());
    let tentative_position = frames
        .iter()
        .position(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseUpdatedReported(update))
                    if update.deltas.iter().any(|delta| matches!(
                        delta,
                        tau_proto::ProviderResponseTextDelta::Message { text, .. }
                            if text == "attempt-one-tentative"
                    ))
            )
        })
        .expect("tentative update");
    let clear_position = frames
        .iter()
        .position(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseUpdatedReported(update))
                    if update.status.as_ref().is_some_and(|status| status.clear_response)
            )
        })
        .expect("partial clear");
    let finish_position = frames
        .iter()
        .position(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseFinishedReported(_))
            )
        })
        .expect("durable finish");
    assert!(
        tentative_position < clear_position && clear_position < finish_position,
        "tentative output must be cleared before durable retry success"
    );
    let finished = frames
        .iter()
        .filter_map(|frame| match input_event(frame) {
            Some(Event::ProviderResponseFinishedReported(finished)) => Some(finished),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(finished.len(), 1);
    let durable = serde_json::to_string(finished[0]).expect("serialize durable response");
    assert!(durable.contains("attempt-two-durable"));
    assert!(!durable.contains("attempt-one-tentative"));
}

/// Covers the shared scheduler boundary for ChatGPT Responses, generic Chat
/// Completions, and OpenRouter while asserting each retry keeps its routing
/// kind.
#[test]
fn all_builtin_provider_families_retry_then_finish_on_the_shared_scheduler() {
    let input = BlockingInput::default();
    let mut chatgpt_prompt = prompt();
    chatgpt_prompt.agent_prompt_id = "chatgpt-retry"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    let mut generic_prompt = prompt();
    generic_prompt.agent_prompt_id = "generic-retry"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    generic_prompt.model = model_id("generic", "generic-model");
    let mut router_prompt = prompt();
    router_prompt.agent_prompt_id = "router-retry"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    router_prompt.model = model_id("router", "router-model");
    input.push(encode_frames(&[
        live_event(11, Event::AgentPromptCreated(chatgpt_prompt)),
        live_event(12, Event::AgentPromptCreated(generic_prompt)),
        live_event(13, Event::AgentPromptCreated(router_prompt)),
    ]));

    let mut startup_profiles = profiles_with_chatgpt_auth(chatgpt_auth());
    startup_profiles.providers.insert(
        ProviderName::new("generic"),
        BuiltinProviderProfile::ChatCompletions(ChatCompletionsProvider {
            base_url: "https://generic.invalid/v1".to_owned(),
            api_key: "generic-key".to_owned(),
            models: vec![chat_model("generic-model")],
            ..ChatCompletionsProvider::default()
        }),
    );
    startup_profiles.providers.insert(
        ProviderName::new("router"),
        BuiltinProviderProfile::OpenRouter(OpenRouterProfile {
            api_key: "router-key".to_owned(),
            models: vec![chat_model("router-model")],
        }),
    );
    let prompt_profiles = startup_profiles.clone();
    let attempts = Arc::new(Mutex::new(
        std::collections::BTreeMap::<String, usize>::new(),
    ));
    let executor_attempts = Arc::clone(&attempts);
    let (finished_tx, finished_rx) = mpsc::channel();
    let executor: PromptExecutor = Arc::new(move |execution| {
        let id = execution.job.agent_prompt_id.to_string();
        match (id.as_str(), &execution.job.backend) {
            ("chatgpt-retry", PromptBackend::Responses(config)) => {
                assert!(config.credentials_match("access", Some("account")));
            }
            (
                "generic-retry",
                PromptBackend::ChatCompletions {
                    provider, model, ..
                },
            ) => {
                assert_eq!(provider.base_url, "https://generic.invalid/v1");
                assert_eq!(model.id.as_str(), "generic-model");
            }
            (
                "router-retry",
                PromptBackend::ChatCompletions {
                    provider, model, ..
                },
            ) => {
                assert_eq!(provider.base_url, "https://openrouter.ai/api/v1");
                assert_eq!(provider.api_key, "router-key");
                assert_eq!(model.id.as_str(), "router-model");
            }
            _ => panic!("provider family changed routing backend for {id}"),
        }
        let attempt = {
            let mut attempts = executor_attempts.lock().expect("family attempts");
            let attempt = attempts.entry(id.clone()).or_default();
            *attempt += 1;
            *attempt
        };
        if attempt == 1 {
            let class = match id.as_str() {
                "chatgpt-retry" => RetryClass::Transport,
                "generic-retry" => RetryClass::Overload,
                "router-retry" => RetryClass::Unknown,
                _ => unreachable!(),
            };
            send_worker_message(
                &execution.output_tx,
                &execution.output_waker,
                WorkerMessage::Retry {
                    job: execution.job,
                    decision: RetryDecision::new(class),
                    live_detail: None,
                    canonical_unauthorized: false,
                    terminal_backend: None,
                },
            )
            .expect("schedule family retry");
            return;
        }
        let mut writer = execution.frame_writer();
        writer
            .write_message(&HarnessInputMessage::emit_transient(
                Event::ProviderResponseFinishedReported(simple_finished(
                    execution.job.agent_prompt_id,
                    execution.job.prompt.agent_id,
                    execution.job.prompt.originator,
                    "family done",
                )),
            ))
            .expect("family finish");
        writer.flush().expect("flush family finish");
        finished_tx.send(id).expect("report family finish");
    });
    let writer = SharedWriter::default();
    let output = writer.clone();
    let runtime_input = input.clone();
    let runtime = thread::spawn(move || {
        run_inner_with_prompt_executor(
            runtime_input,
            writer,
            startup_profiles,
            move || prompt_profiles.clone(),
            3,
            executor,
        )
        .expect("run provider");
    });

    let mut completed = path_std_collections::BTreeSet::new();
    for _ in 0..3 {
        completed.insert(
            finished_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("provider family did not retry to completion"),
        );
    }
    assert_eq!(
        completed,
        ["chatgpt-retry", "generic-retry", "router-retry"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    );
    input.push(encode_frames(&[HarnessOutputMessage::Disconnect(
        tau_proto::Disconnect {
            reason: Some("done".to_owned()),
        },
    )]));
    input.close();
    runtime.join().expect("runtime join");
    assert!(
        attempts
            .lock()
            .expect("family attempts")
            .values()
            .all(|count| *count == 2)
    );
    let frames = decode_frames(&output.bytes());
    for id in ["chatgpt-retry", "generic-retry", "router-retry"] {
        assert_eq!(
            frames
                .iter()
                .filter(|frame| matches!(
                    input_event(frame),
                    Some(Event::ProviderPromptSubmittedReported(submitted))
                        if submitted.agent_prompt_id.as_str() == id
                ))
                .count(),
            1
        );
        assert_eq!(
            frames
                .iter()
                .filter(|frame| matches!(
                    input_event(frame),
                    Some(Event::ProviderResponseFinishedReported(finished))
                        if finished.agent_prompt_id.as_str() == id
                ))
                .count(),
            1
        );
    }
}

/// Termination boundary exercised by the mixed-state runtime fixture.
#[derive(Clone, Copy)]
enum MixedStateShutdown {
    /// Broadcast prompt cancellation while input remains open.
    GlobalCancel,
    /// Harness input EOF and provider-loop shutdown.
    Eof,
}

const MIXED_STATE_TIMEOUT: Duration = Duration::from_secs(2);

/// Runs the mixed delayed/active/queued lifecycle fixture for broadcast cancel
/// or input EOF, both of which must close every accepted prompt exactly once.
fn assert_mixed_state_shutdown(shutdown: MixedStateShutdown) {
    let clock = Arc::new(VirtualRetryClock::new(Instant::now()));
    let input = BlockingInput::default();
    let mut delayed = prompt();
    delayed.agent_prompt_id = "mixed-delayed"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    let mut active = prompt();
    active.agent_prompt_id = "mixed-active"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    let mut queued = prompt();
    queued.agent_prompt_id = "mixed-queued"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    input.push(encode_frames(&[
        live_event(11, Event::AgentPromptCreated(delayed)),
        live_event(12, Event::AgentPromptCreated(active)),
        live_event(13, Event::AgentPromptCreated(queued)),
    ]));
    let (delayed_tx, delayed_rx) = mpsc::sync_channel(1);
    let (active_tx, active_rx) = mpsc::sync_channel(1);
    let (active_cancel_tx, active_cancel_rx) = mpsc::channel();
    let active_cancel_rx = Mutex::new(active_cancel_rx);
    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
    let executor_calls = Arc::clone(&calls);
    let initial_workers_started = Arc::new(Barrier::new(2));
    let executor: PromptExecutor = Arc::new(move |execution| {
        let id = execution.job.agent_prompt_id.to_string();
        executor_calls.lock().expect("mixed calls").push(id.clone());
        match id.as_str() {
            "mixed-delayed" => {
                initial_workers_started.wait();
                send_worker_message(
                    &execution.output_tx,
                    &execution.output_waker,
                    WorkerMessage::Retry {
                        job: execution.job,
                        decision: RetryDecision::new(RetryClass::UsageWindow)
                            .with_retry_after(Some(Duration::from_secs(86_400))),
                        live_detail: None,
                        canonical_unauthorized: false,
                        terminal_backend: None,
                    },
                )
                .expect("park delayed prompt");
                delayed_tx.send(()).expect("report delayed state");
            }
            "mixed-active" => {
                initial_workers_started.wait();
                let cancel_tx = active_cancel_tx.clone();
                let _active_abort_waker_guard = execution.cancellation.register_abort_waker(
                    &execution.job.agent_prompt_id,
                    execution.job.cancel_generation,
                    Arc::new(move || {
                        cancel_tx.send(()).expect("report active cancellation");
                    }),
                );
                active_tx.send(()).expect("report active state");
                active_cancel_rx
                    .lock()
                    .expect("active cancel receiver")
                    .recv_timeout(MIXED_STATE_TIMEOUT)
                    .expect("shutdown did not wake active backend");
                let mut writer = execution.frame_writer();
                writer
                    .write_message(&HarnessInputMessage::emit_transient(
                        Event::ProviderResponseFinishedReported(simple_finished(
                            execution.job.agent_prompt_id,
                            execution.job.prompt.agent_id,
                            execution.job.prompt.originator,
                            "late success must become canceled",
                        )),
                    ))
                    .expect("late active terminal");
                writer.flush().expect("flush late active terminal");
            }
            other => panic!("queued prompt unexpectedly executed: {other}"),
        }
    });
    let profiles = profiles_with_chatgpt_auth(chatgpt_auth());
    let prompt_profiles = profiles.clone();
    let writer = SharedWriter::default();
    let output = writer.clone();
    let runtime_input = input.clone();
    let runtime_clock: Arc<dyn RetryClock> = clock;
    let (runtime_done_tx, runtime_done_rx) = mpsc::sync_channel(1);
    let runtime = thread::spawn(move || {
        let result = run_inner_with_executors_and_clock(
            runtime_input,
            writer,
            profiles,
            move || prompt_profiles.clone(),
            2,
            RuntimeExecutors {
                prompt: executor,
                prewarm: production_prewarm_executor(),
                retry_clock: runtime_clock,
            },
        );
        runtime_done_tx
            .send(())
            .expect("report mixed runtime completion");
        result.expect("run provider");
    });

    delayed_rx
        .recv_timeout(MIXED_STATE_TIMEOUT)
        .expect("delayed state");
    wait_for_runtime_frames(&output, |frames| {
        frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseUpdatedReported(update))
                    if update.agent_prompt_id.as_str() == "mixed-delayed"
                        && update.status.as_ref().is_some_and(|status| {
                            status.retry.as_ref().is_some_and(|retry| {
                                retry.category
                                    == tau_proto::ProviderRetryCategory::UsageWindow
                            })
                        })
            )
        })
    });
    let mut cooldown_peer = prompt();
    cooldown_peer.agent_prompt_id = "mixed-cooldown-peer"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    input.push(encode_frames(&[live_event(
        14,
        Event::AgentPromptCreated(cooldown_peer),
    )]));
    wait_for_runtime_frames(&output, |frames| {
        frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseUpdatedReported(update))
                    if update.agent_prompt_id.as_str() == "mixed-cooldown-peer"
                        && update.status.as_ref().is_some_and(|status| {
                            status.retry.as_ref().is_some_and(|retry| retry.attempt == 0)
                        })
            )
        })
    });
    active_rx
        .recv_timeout(MIXED_STATE_TIMEOUT)
        .expect("active state");
    match shutdown {
        MixedStateShutdown::GlobalCancel => {
            input.push(encode_frames(&[live_event(
                20,
                Event::UiCancelPrompt(tau_proto::UiCancelPrompt {
                    session_id: tau_proto::SessionId::parse("test-session")
                        .expect("known-safe SessionId must be valid"),
                    target_agent_id: None,
                    agent_prompt_id: None,
                }),
            )]));
        }
        MixedStateShutdown::Eof => input.close(),
    }

    if matches!(shutdown, MixedStateShutdown::GlobalCancel) {
        wait_for_runtime_frames(&output, |frames| {
            frames
                .iter()
                .filter(|frame| {
                    matches!(
                        input_event(frame),
                        Some(Event::ProviderResponseFinishedReported(finished))
                            if finished.error.as_deref() == Some("(cancelled by harness)")
                    )
                })
                .count()
                == 4
        });
        input.push(encode_frames(&[HarnessOutputMessage::Disconnect(
            tau_proto::Disconnect {
                reason: Some("done".to_owned()),
            },
        )]));
        input.close();
    }
    runtime_done_rx
        .recv_timeout(MIXED_STATE_TIMEOUT)
        .expect("mixed runtime shuts down within bound");
    runtime.join().expect("mixed runtime join");

    let frames = decode_frames(&output.bytes());
    for id in [
        "mixed-delayed",
        "mixed-active",
        "mixed-queued",
        "mixed-cooldown-peer",
    ] {
        assert_eq!(
            frames
                .iter()
                .filter(|frame| matches!(
                    input_event(frame),
                    Some(Event::ProviderPromptSubmittedReported(submitted))
                        if submitted.agent_prompt_id.as_str() == id
                ))
                .count(),
            1
        );
        assert_eq!(
            frames
                .iter()
                .filter(|frame| matches!(
                    input_event(frame),
                    Some(Event::ProviderResponseFinishedReported(finished))
                        if finished.agent_prompt_id.as_str() == id
                            && finished.error.as_deref() == Some("(cancelled by harness)")
                ))
                .count(),
            1
        );
    }
    assert_eq!(
        calls
            .lock()
            .expect("mixed calls")
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from(["mixed-delayed".to_owned(), "mixed-active".to_owned()]),
        "queued work must close without provider execution"
    );
}

/// Global cancellation closes delayed, active-late-success, and queued work
/// once.
#[test]
fn global_cancel_closes_mixed_prompt_states_exactly_once() {
    assert_mixed_state_shutdown(MixedStateShutdown::GlobalCancel);
}

/// Harness EOF closes delayed, active-late-success, and queued work once.
#[test]
fn eof_closes_mixed_prompt_states_exactly_once() {
    assert_mixed_state_shutdown(MixedStateShutdown::Eof);
}

/// Proves a cold provider restart neither replays ambiguous old work nor
/// inherits its process-local cooldown before admitting fresh work.
#[test]
fn cold_restart_discards_old_work_and_cooldown() {
    assert_mixed_state_shutdown(MixedStateShutdown::Eof);

    let input = BlockingInput::default();
    let mut fresh = prompt();
    fresh.agent_prompt_id = "fresh-after-restart"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    input.push(encode_frames(&[live_event(
        40,
        Event::AgentPromptCreated(fresh),
    )]));
    let (finished_tx, finished_rx) = mpsc::sync_channel(1);
    let executor: PromptExecutor = Arc::new(move |execution| {
        assert_eq!(
            execution.job.agent_prompt_id.as_str(),
            "fresh-after-restart",
            "old process work must not replay into the fresh runtime"
        );
        let mut writer = execution.frame_writer();
        writer
            .write_message(&HarnessInputMessage::emit_transient(
                Event::ProviderResponseFinishedReported(simple_finished(
                    execution.job.agent_prompt_id,
                    execution.job.prompt.agent_id,
                    execution.job.prompt.originator,
                    "fresh success",
                )),
            ))
            .expect("write fresh terminal");
        writer.flush().expect("flush fresh terminal");
        finished_tx.send(()).expect("report fresh completion");
    });
    let profiles = profiles_with_chatgpt_auth(chatgpt_auth());
    let prompt_profiles = profiles.clone();
    let writer = SharedWriter::default();
    let output = writer.clone();
    let runtime_input = input.clone();
    let (fresh_done_tx, fresh_done_rx) = mpsc::sync_channel(1);
    let runtime = thread::spawn(move || {
        let result = run_inner_with_prompt_executor(
            runtime_input,
            writer,
            profiles,
            move || prompt_profiles.clone(),
            1,
            executor,
        );
        fresh_done_tx.send(()).expect("report fresh runtime exit");
        result.expect("run fresh provider process");
    });
    finished_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("fresh work admitted without old cooldown");
    input.push(encode_frames(&[HarnessOutputMessage::Disconnect(
        tau_proto::Disconnect {
            reason: Some("fresh done".to_owned()),
        },
    )]));
    input.close();
    fresh_done_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("fresh runtime exits within bound");
    runtime.join().expect("fresh runtime joins");

    let frames = decode_frames(&output.bytes());
    assert_eq!(
        frames
            .iter()
            .filter(|frame| matches!(
                input_event(frame),
                Some(Event::ProviderPromptSubmittedReported(submitted))
                    if submitted.agent_prompt_id.as_str() == "fresh-after-restart"
            ))
            .count(),
        1
    );
    assert_eq!(
        frames
            .iter()
            .filter(|frame| matches!(
                input_event(frame),
                Some(Event::ProviderResponseFinishedReported(finished))
                    if finished.agent_prompt_id.as_str() == "fresh-after-restart"
            ))
            .count(),
        1
    );
    assert!(
        frames.iter().all(|frame| !matches!(
            input_event(frame),
            Some(Event::ProviderPromptSubmittedReported(submitted))
                if submitted.agent_prompt_id.as_str().starts_with("mixed-")
        )),
        "old ambiguous APs must not be replayed after restart"
    );
}

/// A real Chat Completions transport repetition reaches the production
/// executor/runtime as one local deterministic terminal with no reschedule.
#[test]
fn real_repetition_failure_finishes_once_without_scheduler_retry() {
    let listener =
        path_std_net::TcpListener::bind(("127.0.0.1", 0)).expect("bind repetition server");
    let address = listener.local_addr().expect("repetition server address");
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept repetition request");
        let mut request = [0_u8; 8192];
        let _ = socket.read(&mut request).expect("read repetition request");
        let event = serde_json::json!({
            "choices": [{
                "delta": { "content": ".".repeat(1024) },
                "finish_reason": null
            }]
        });
        let body = format!("data: {event}\n\n");
        write!(
            socket,
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .expect("write repetition response");
    });
    let input = BlockingInput::default();
    let mut created = prompt();
    created.agent_prompt_id = "real-terminal"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    created.model = model_id("generic", "generic-model");
    input.push(encode_frames(&[live_event(
        11,
        Event::AgentPromptCreated(created),
    )]));
    let mut profiles = BuiltinProviderProfiles::default();
    profiles.providers.insert(
        ProviderName::new("generic"),
        BuiltinProviderProfile::ChatCompletions(ChatCompletionsProvider {
            base_url: format!("http://{address}"),
            api_key: "key".to_owned(),
            models: vec![chat_model("generic-model")],
            ..ChatCompletionsProvider::default()
        }),
    );
    let prompt_profiles = profiles.clone();
    let writer = SharedWriter::default();
    let output = writer.clone();
    let runtime_input = input.clone();
    let runtime = thread::spawn(move || {
        run_inner_with_prompt_executor(
            runtime_input,
            writer,
            profiles,
            move || prompt_profiles.clone(),
            1,
            production_prompt_executor(),
        )
        .expect("run production provider");
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let frames = try_decode_frames(&output.bytes()).unwrap_or_default();
        if frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseFinishedReported(finished))
                    if finished.agent_prompt_id.as_str() == "real-terminal"
            )
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "real deterministic terminal did not finish"
        );
        thread::yield_now();
    }
    input.push(encode_frames(&[HarnessOutputMessage::Disconnect(
        tau_proto::Disconnect {
            reason: Some("done".to_owned()),
        },
    )]));
    input.close();
    runtime.join().expect("production runtime join");
    server.join().expect("repetition server join");
    let frames = decode_frames(&output.bytes());
    let finished = frames
        .iter()
        .filter_map(|frame| match input_event(frame) {
            Some(Event::ProviderResponseFinishedReported(finished))
                if finished.agent_prompt_id.as_str() == "real-terminal" =>
            {
                Some(finished)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(finished.len(), 1);
    assert_eq!(
        finished[0].stop_reason,
        tau_proto::ProviderStopReason::RepetitionDetected
    );
    assert!(frames.iter().all(|frame| !matches!(
        input_event(frame),
        Some(Event::ProviderResponseUpdatedReported(update))
            if update.agent_prompt_id.as_str() == "real-terminal"
                && update.status.as_ref().is_some_and(|status|
                    status.text.contains("next attempt"))
    )));
}

/// Retry statuses are one-per-attempt, bounded, and may carry only the
/// provider layer's already-redacted live detail.
#[test]
fn retry_status_is_bounded_safe_and_attempt_rate_limited() {
    let input = BlockingInput::default();
    input.push(encode_frames(&[live_event(
        11,
        Event::AgentPromptCreated(prompt()),
    )]));
    let attempts = Arc::new(AtomicUsize::new(0));
    let executor_attempts = Arc::clone(&attempts);
    let secret = "provider-secret-body\n\u{1b}[31m account=acct-123";
    let live_detail = tau_provider_codex::test_redacted_provider_detail(
        "provider overloaded safely provider-secret-body\n\u{1b}[31m account=acct-123",
        "provider-secret-body",
        Some("acct-123"),
    )
    .expect("production Codex redaction");
    let executor: PromptExecutor = Arc::new(move |execution| {
        let attempt = executor_attempts.fetch_add(1, Ordering::SeqCst);
        if attempt < 2 {
            send_worker_message(
                &execution.output_tx,
                &execution.output_waker,
                WorkerMessage::Retry {
                    job: execution.job,
                    decision: RetryDecision::new(if attempt == 0 {
                        RetryClass::Transport
                    } else {
                        RetryClass::Unknown
                    }),
                    live_detail: Some(live_detail.clone()),
                    canonical_unauthorized: false,
                    terminal_backend: None,
                },
            )
            .expect("schedule status fixture retry");
            return;
        }
        let mut writer = execution.frame_writer();
        writer
            .write_message(&HarnessInputMessage::emit_transient(
                Event::ProviderResponseFinishedReported(simple_finished(
                    execution.job.agent_prompt_id,
                    execution.job.prompt.agent_id,
                    execution.job.prompt.originator,
                    "done",
                )),
            ))
            .expect("status fixture finish");
        writer.flush().expect("flush status fixture");
    });
    let profiles = profiles_with_chatgpt_auth(chatgpt_auth());
    let prompt_profiles = profiles.clone();
    let writer = SharedWriter::default();
    let output = writer.clone();
    let runtime_input = input.clone();
    let runtime = thread::spawn(move || {
        run_inner_with_prompt_executor(
            runtime_input,
            writer,
            profiles,
            move || prompt_profiles.clone(),
            1,
            executor,
        )
        .expect("run provider");
    });
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let frames = try_decode_frames(&output.bytes()).unwrap_or_default();
        if frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseFinishedReported(finished))
                    if finished.agent_prompt_id.as_str() == "sp-1"
            )
        }) {
            break;
        }
        assert!(Instant::now() < deadline, "status fixture did not finish");
        thread::yield_now();
    }
    input.push(encode_frames(&[HarnessOutputMessage::Disconnect(
        tau_proto::Disconnect {
            reason: Some("done".to_owned()),
        },
    )]));
    input.close();
    runtime.join().expect("status runtime join");
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    let frames = decode_frames(&output.bytes());
    let statuses = frames
        .iter()
        .filter_map(|frame| match input_event(frame) {
            Some(Event::ProviderResponseUpdatedReported(update)) => update.status.as_ref(),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(statuses.len(), 2, "one status per failed real attempt");
    for status in statuses {
        assert!(status.clear_response);
        assert!(status.text.len() <= 256);
        assert!(!status.text.contains(secret));
        assert!(!status.text.contains("provider-secret-body"));
        assert!(!status.text.chars().any(char::is_control));
        assert!(status.text.contains("cancel the prompt to stop"));
        assert!(status.text.contains("provider overloaded safely"));
    }
    assert!(matches!(
        frames.last().and_then(input_event),
        Some(Event::ProviderResponseFinishedReported(finished))
            if finished.agent_prompt_id.as_str() == "sp-1"
    ));
}

/// Ensures a missing ChatGPT OAuth Secret gives the initiating user the exact
/// login command on the first and later retry status, without exposing the
/// Secret backend's diagnostic, while other unavailable profiles retain the
/// generic authentication status.
#[test]
fn missing_chatgpt_login_status_is_actionable_and_redacted() {
    let provider = ProviderName::new("chatgpt-fedi");
    let api_key_provider = ProviderName::new("local");
    let secret_backend_detail = "Secret providers/credential-id/oauth.json missing";
    let mut profiles = profiles_with_chatgpt_auth(chatgpt_auth());
    profiles
        .providers
        .remove(&ProviderName::new(CHATGPT_PROVIDER_NAME));
    let chatgpt = BuiltinProviderProfile::Chatgpt(ChatGptProfile::default());
    profiles.providers.insert(provider.clone(), chatgpt);
    profiles.providers.insert(
        api_key_provider.clone(),
        BuiltinProviderProfile::ChatCompletions(ChatCompletionsProvider {
            base_url: "https://local.invalid/v1".to_owned(),
            models: vec![chat_model("model")],
            ..ChatCompletionsProvider::default()
        }),
    );
    profiles.credentials.insert(
        provider.clone(),
        ProviderCredential::Stored(
            ProviderCredentialReference::new(
                ProviderCredentialIdentity::random(),
                ProviderCredentialSlot::OAuth,
                None,
            )
            .expect("valid OAuth credential reference"),
        ),
    );
    profiles.credentials.insert(
        api_key_provider.clone(),
        ProviderCredential::Stored(
            ProviderCredentialReference::new(
                ProviderCredentialIdentity::random(),
                ProviderCredentialSlot::ApiKey,
                None,
            )
            .expect("valid API-key credential reference"),
        ),
    );
    let trace = SharedTraceWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .without_time()
        .with_ansi(false)
        .with_writer({
            let trace = trace.clone();
            move || trace.clone()
        })
        .finish();
    let dispatch = tracing::subscriber::set_default(subscriber);
    hydrate_profile_credentials_with(&mut profiles, |_| {
        Err(tau_client::ExtensionDataRpcError::Harness {
            kind: tau_proto::ExtensionDataErrorKind::NotFound,
            message: secret_backend_detail.to_owned(),
        })
    });
    drop(dispatch);
    let trace = String::from_utf8(trace.bytes()).expect("trace is UTF-8");
    assert!(!trace.contains(secret_backend_detail));
    assert!(trace.contains("credential_error"));
    assert!(trace.contains("not_found"));
    assert!(profiles.missing_login(&provider));
    assert!(!profiles.missing_login(&api_key_provider));
    assert!(profiles.providers.is_empty());

    let now = Instant::now();
    let mut job = scheduled_job("missing-login", provider.as_str());
    job.backend = resolve_prompt_backend(
        &job.prompt.model,
        &mut profiles,
        &mut OAuthRefreshRejectionCache::default(),
        &test_network_policy(),
        None,
    )
    .unwrap_or_else(|| PromptBackend::Unavailable {
        login_required: profiles
            .missing_login(&job.prompt.model.provider)
            .then(|| job.prompt.model.provider.clone()),
    });
    job.retry_state.attempts = 1;
    let initial = retry_status_text(
        &job,
        RetryClass::Auth,
        now + Duration::from_secs(1),
        now,
        None,
    );
    job.retry_state.attempts = 2;
    let retry = retry_status_text(
        &job,
        RetryClass::Auth,
        now + Duration::from_secs(1),
        now,
        Some(secret_backend_detail),
    );
    for text in [&initial, &retry] {
        assert!(text.contains("provider chatgpt-fedi is not logged in"));
        assert!(text.contains("tau provider login chatgpt-fedi"));
        assert!(!text.contains(secret_backend_detail));
        assert!(text.contains("Tau will keep trying; cancel the prompt to stop."));
    }
    assert!(initial.contains("attempt 1"));
    assert!(retry.contains("attempt 2"));

    let mut generic_job = scheduled_job("missing-api-key", api_key_provider.as_str());
    generic_job.prompt.model.model = ModelName::new("model");
    generic_job.retry_state.attempts = 2;
    generic_job.backend = resolve_prompt_backend(
        &generic_job.prompt.model,
        &mut profiles,
        &mut OAuthRefreshRejectionCache::default(),
        &test_network_policy(),
        None,
    )
    .unwrap_or_else(|| PromptBackend::Unavailable {
        login_required: profiles
            .missing_login(&generic_job.prompt.model.provider)
            .then(|| generic_job.prompt.model.provider.clone()),
    });
    assert!(matches!(
        generic_job.backend,
        PromptBackend::Unavailable {
            login_required: None,
        }
    ));
    let generic = retry_status_text(
        &generic_job,
        RetryClass::Auth,
        now + Duration::from_secs(1),
        now,
        None,
    );
    assert_eq!(
        generic,
        format!(
            "{}; next attempt in about 1s (attempt 2). Tau will keep trying; cancel the prompt to stop.",
            RetryClass::Auth.public_reason(),
        ),
        "unclassified Secret failures retain the generic status rather than a login claim"
    );
    assert!(!generic.contains(secret_backend_detail));
}

/// A queued targeted cancel consumes its marker after terminal commit so the
/// same prompt ID can be accepted again without inheriting stale cancellation.
#[test]
fn queued_targeted_cancel_allows_prompt_id_reuse() {
    let input = BlockingInput::default();
    let mut active = prompt();
    active.agent_prompt_id = "occupying"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    let mut queued = prompt();
    queued.agent_prompt_id = "reused"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    input.push(encode_frames(&[
        live_event(11, Event::AgentPromptCreated(active)),
        live_event(12, Event::AgentPromptCreated(queued)),
    ]));
    let (active_tx, active_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let release_rx = Mutex::new(release_rx);
    let (reused_tx, reused_rx) = mpsc::sync_channel(1);
    let executor: PromptExecutor = Arc::new(move |execution| {
        if execution.job.agent_prompt_id.as_str() == "occupying" {
            active_tx.send(()).expect("report occupying prompt");
            release_rx
                .lock()
                .expect("release receiver")
                .recv()
                .expect("release occupying prompt");
        } else {
            reused_tx.send(()).expect("report reused prompt execution");
        }
        let mut writer = execution.frame_writer();
        writer
            .write_message(&HarnessInputMessage::emit_transient(
                Event::ProviderResponseFinishedReported(simple_finished(
                    execution.job.agent_prompt_id,
                    execution.job.prompt.agent_id,
                    execution.job.prompt.originator,
                    "done",
                )),
            ))
            .expect("reuse fixture finish");
        writer.flush().expect("flush reuse fixture");
    });
    let profiles = profiles_with_chatgpt_auth(chatgpt_auth());
    let prompt_profiles = profiles.clone();
    let writer = SharedWriter::default();
    let output = writer.clone();
    let runtime_input = input.clone();
    let runtime = thread::spawn(move || {
        run_inner_with_prompt_executor(
            runtime_input,
            writer,
            profiles,
            move || prompt_profiles.clone(),
            1,
            executor,
        )
        .expect("run provider");
    });
    active_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("occupying prompt start");
    input.push(encode_frames(&[live_event(
        13,
        Event::UiCancelPrompt(tau_proto::UiCancelPrompt {
            session_id: tau_proto::SessionId::parse("test-session")
                .expect("known-safe SessionId must be valid"),
            target_agent_id: None,
            agent_prompt_id: Some(
                "reused"
                    .parse::<tau_proto::AgentPromptId>()
                    .expect("known-safe AgentPromptId must be valid"),
            ),
        }),
    )]));
    input.wait_for_reader_waiting(Duration::from_secs(1));
    let mut reused_prompt = prompt();
    reused_prompt.agent_prompt_id = "reused"
        .parse::<tau_proto::AgentPromptId>()
        .expect("known-safe AgentPromptId must be valid");
    input.push(encode_frames(&[live_event(
        14,
        Event::AgentPromptCreated(reused_prompt),
    )]));
    input.wait_for_reader_waiting(Duration::from_secs(1));
    release_tx.send(()).expect("release occupying prompt");
    reused_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("reused prompt ID inherited stale cancellation");
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let frames = try_decode_frames(&output.bytes()).unwrap_or_default();
        if frames
            .iter()
            .filter(|frame| {
                matches!(
                    input_event(frame),
                    Some(Event::ProviderResponseFinishedReported(finished))
                        if finished.agent_prompt_id.as_str() == "reused"
                )
            })
            .count()
            == 2
        {
            break;
        }
        assert!(Instant::now() < deadline, "reused lifecycle did not close");
        thread::yield_now();
    }
    input.push(encode_frames(&[HarnessOutputMessage::Disconnect(
        tau_proto::Disconnect {
            reason: Some("done".to_owned()),
        },
    )]));
    input.close();
    runtime.join().expect("reuse runtime join");
    let frames = decode_frames(&output.bytes());
    let reused_finishes = frames
        .iter()
        .filter_map(|frame| match input_event(frame) {
            Some(Event::ProviderResponseFinishedReported(finished))
                if finished.agent_prompt_id.as_str() == "reused" =>
            {
                Some(finished)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(reused_finishes.len(), 2);
    assert_eq!(
        reused_finishes
            .iter()
            .filter(|finished| finished.error.as_deref() == Some("(cancelled by harness)"))
            .count(),
        1
    );
    assert_eq!(
        reused_finishes
            .iter()
            .filter(|finished| finished.error.as_deref() == Some("done"))
            .count(),
        1
    );
}

/// Ensures a retry outcome arriving after targeted cancellation is completed as
/// canceled instead of being scheduled and resurrecting the logical prompt.
#[test]
fn late_retry_after_targeted_cancel_is_not_rescheduled() {
    let input = BlockingInput::default();
    input.push(encode_frames(&[live_event(
        11,
        Event::AgentPromptCreated(prompt()),
    )]));
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let release_rx = Mutex::new(release_rx);
    let executor: PromptExecutor = Arc::new(move |execution| {
        started_tx.send(()).expect("report worker start");
        release_rx
            .lock()
            .expect("release receiver lock")
            .recv()
            .expect("release worker");
        send_worker_message(
            &execution.output_tx,
            &execution.output_waker,
            WorkerMessage::Retry {
                job: execution.job,
                decision: RetryDecision::new(RetryClass::Transport),
                live_detail: None,
                canonical_unauthorized: false,
                terminal_backend: None,
            },
        )
        .expect("return late retry");
    });
    let profiles = profiles_with_chatgpt_auth(chatgpt_auth());
    let prompt_profiles = profiles.clone();
    let writer = SharedWriter::default();
    let output = writer.clone();
    let runtime_input = input.clone();
    let runtime = thread::spawn(move || {
        run_inner_with_prompt_executor(
            runtime_input,
            writer,
            profiles,
            move || prompt_profiles.clone(),
            1,
            executor,
        )
        .expect("run provider");
    });
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("worker start");
    input.push(encode_frames(&[live_event(
        12,
        Event::UiCancelPrompt(tau_proto::UiCancelPrompt {
            session_id: tau_proto::SessionId::parse("test-session")
                .expect("known-safe SessionId must be valid"),
            target_agent_id: None,
            agent_prompt_id: Some(
                "sp-1"
                    .parse::<tau_proto::AgentPromptId>()
                    .expect("known-safe AgentPromptId must be valid"),
            ),
        }),
    )]));
    input.wait_for_reader_waiting(Duration::from_secs(1));
    release_tx.send(()).expect("release retry outcome");

    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let frames = decode_frames(&output.bytes());
        if frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseFinishedReported(finished))
                    if finished.agent_prompt_id.as_str() == "sp-1"
                        && finished.error.as_deref() == Some("(cancelled by harness)")
            )
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "late retry did not finish canceled"
        );
        thread::sleep(Duration::from_millis(5));
    }
    let frames = decode_frames(&output.bytes());
    let canceled = frames
        .iter()
        .filter(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseFinishedReported(finished))
                    if finished.agent_prompt_id.as_str() == "sp-1"
                        && finished.error.as_deref() == Some("(cancelled by harness)")
            )
        })
        .count();
    assert_eq!(canceled, 1, "targeted cancel must finish exactly once");
    input.push(encode_frames(&[HarnessOutputMessage::Disconnect(
        tau_proto::Disconnect {
            reason: Some("done".to_owned()),
        },
    )]));
    input.close();
    runtime.join().expect("runtime join");
}

/// Ensures worker output wakes the provider loop without waiting for worker
/// completion.
#[test]
fn worker_output_wakes_loop_before_prompt_done() {
    // Regression coverage for the event-driven provider loop: a worker output
    // frame must wake the main loop as soon as it is enqueued. If the loop only
    // woke for `PromptDone`, this test would time out while the fake worker is
    // deliberately blocked after flushing its output.
    let input = BlockingInput::default();
    input.push(encode_frames(&[live_event(
        11,
        Event::AgentPromptCreated(prompt()),
    )]));
    let input_control = input.clone();
    let executor_started = Arc::new((Mutex::new(false), Condvar::new()));
    let executor_emit = Arc::new((Mutex::new(false), Condvar::new()));
    let release_worker = Arc::new((Mutex::new(false), Condvar::new()));
    let executor_started_worker = executor_started.clone();
    let executor_emit_worker = executor_emit.clone();
    let executor_release = release_worker.clone();
    let executor: PromptExecutor = Arc::new(move |execution| {
        let (lock, cv) = &*executor_started_worker;
        *lock.lock().expect("started lock") = true;
        cv.notify_all();

        let (lock, cv) = &*executor_emit_worker;
        let mut emit = lock.lock().expect("emit lock");
        while !*emit {
            emit = cv.wait(emit).expect("wait to emit");
        }
        drop(emit);

        let mut writer = execution.frame_writer();
        write_prompt_submitted(
            &execution.job.agent_prompt_id,
            &execution.job.prompt.originator,
            &mut writer,
        )
        .expect("submitted");
        writer.flush().expect("flush submitted");

        let (lock, cv) = &*executor_release;
        let mut released = lock.lock().expect("release lock");
        while !*released {
            released = cv.wait(released).expect("wait for release");
        }
    });

    let profiles = profiles_with_chatgpt_auth(chatgpt_auth());
    let prompt_profiles = profiles.clone();
    let writer = SharedWriter::default();
    let output = writer.clone();
    let (result_tx, result_rx) = mpsc::channel();
    let runner = thread::spawn(move || {
        let result = run_inner_with_prompt_executor(
            input,
            writer,
            profiles,
            move || prompt_profiles.clone(),
            1,
            executor,
        )
        .map_err(|error| error.to_string());
        let _ = result_tx.send(result);
    });

    let (lock, cv) = &*executor_started;
    let mut started = lock.lock().expect("started lock");
    while !*started {
        started = cv.wait(started).expect("wait for worker start");
    }
    drop(started);
    input_control.wait_for_reader_waiting(Duration::from_secs(1));
    thread::sleep(Duration::from_millis(25));

    let (lock, cv) = &*executor_emit;
    *lock.lock().expect("emit lock") = true;
    cv.notify_all();

    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let frames = decode_frames(&output.bytes());
        if frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderPromptSubmittedReported(_))
            )
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "worker output did not wake provider loop before PromptDone; frames: {frames:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }

    let (lock, cv) = &*release_worker;
    *lock.lock().expect("release lock") = true;
    cv.notify_all();
    input_control.close();
    result_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("provider runner result before timeout")
        .expect("run provider extension");
    runner.join().expect("provider runner thread");
}

/// Replayed prompt creation is historical state and must not repeat provider
/// side effects after the extension subscribes to live events.
#[test]
fn replayed_prompt_creation_does_not_start_executor_or_emit_prompt_events() {
    let input = encode_frames(&[
        replay_event(10, Event::AgentPromptCreated(prompt())),
        HarnessOutputMessage::Disconnect(tau_proto::Disconnect {
            reason: Some("done".to_owned()),
        }),
    ]);
    let executor_calls = Arc::new(AtomicUsize::new(0));
    let executor_calls_for_worker = executor_calls.clone();
    let executor: PromptExecutor = Arc::new(move |_| {
        executor_calls_for_worker.fetch_add(1, Ordering::SeqCst);
    });

    let profiles = profiles_with_chatgpt_auth(chatgpt_auth());
    let prompt_profiles = profiles.clone();
    let writer = SharedWriter::default();
    let output = writer.clone();
    run_inner_with_prompt_executor(
        Cursor::new(input),
        writer,
        profiles,
        move || prompt_profiles.clone(),
        1,
        executor,
    )
    .expect("run provider extension");

    assert_eq!(executor_calls.load(Ordering::SeqCst), 0);
    let frames = decode_frames(&output.bytes());
    assert!(
        frames.iter().all(|frame| {
            !matches!(
                input_event(frame),
                Some(
                    Event::ProviderPromptSubmittedReported(_)
                        | Event::ProviderResponseUpdatedReported(_)
                        | Event::ProviderResponseFinishedReported(_)
                )
            )
        }),
        "replayed prompt must not emit provider prompt effects: {frames:?}"
    );
}

/// Replayed session-directory facts are current-state catch-up and must still
/// control whether later live prompts may write provider diagnostics.
#[test]
fn replayed_session_dir_controls_live_prompt_debug_policy() {
    for (status, expected_debug) in [
        (tau_proto::SessionDirStatus::New, true),
        (tau_proto::SessionDirStatus::Ephemeral, false),
    ] {
        let input = encode_frames(&[
            replay_event(10, Event::HarnessSessionDir(session_dir(status))),
            live_event(11, Event::AgentPromptCreated(prompt())),
        ]);
        let observed_debug = Arc::new(Mutex::new(None));
        let observed_debug_for_worker = observed_debug.clone();
        let executor: PromptExecutor = Arc::new(move |execution| {
            *observed_debug_for_worker
                .lock()
                .expect("observed debug lock") = Some(execution.job.debug_provider_requests);
        });

        let profiles = profiles_with_chatgpt_auth(chatgpt_auth());
        let prompt_profiles = profiles.clone();
        let writer = SharedWriter::default();
        run_inner_with_prompt_executor(
            Cursor::new(input),
            writer,
            profiles,
            move || prompt_profiles.clone(),
            1,
            executor,
        )
        .expect("run provider extension");

        assert_eq!(
            *observed_debug.lock().expect("observed debug lock"),
            Some(expected_debug),
            "session status should map to debug policy"
        );
    }
}

/// Production startup captures only the initial scoped Configure map, resolves
/// references before model publication, and never exposes the resolved value.
#[test]
fn provider_startup_declares_exact_subscriptions_and_models_before_ready() {
    // Provider model snapshots need to reach the harness during startup so
    // model/role UI state is available immediately after all extensions are
    // ready.
    let writer = SharedWriter::default();
    let output = writer.clone();
    run_with_auth(Cursor::new(encode_frames(&[])), writer, chatgpt_auth())
        .expect("run provider extension");

    let frames = decode_frames(&output.bytes());
    assert!(
        matches!(
            &frames[0],
            HarnessInputMessage::Hello(hello)
                if hello.client_kind == ClientKind::Provider
                    && hello.client_name.as_str() == EXTENSION_NAME
        ),
        "first frame should be provider hello: {frames:?}"
    );
    let subscribe_frames = frames
        .iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::Subscribe(subscribe) => Some(subscribe),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        subscribe_frames.len(),
        1,
        "provider should emit one startup Subscribe frame: {frames:?}",
    );
    assert_eq!(
        subscribe_frames[0].live_selectors,
        [
            tau_proto::EventSelector::Exact(EventName::AGENT_PROMPT_PREWARM_REQUESTED),
            tau_proto::EventSelector::Exact(EventName::AGENT_CACHE_REFRESH_REQUESTED),
            tau_proto::EventSelector::Exact(EventName::AGENT_CACHE_REFRESH_CANCEL_REQUESTED),
            tau_proto::EventSelector::Exact(EventName::HARNESS_SESSION_DIR),
            tau_proto::EventSelector::Exact(EventName::UI_CANCEL_PROMPT),
            tau_proto::EventSelector::Exact(EventName::SESSION_SHUTDOWN),
        ],
        "provider startup subscriptions must stay exact and exclude ordinary prompt routing",
    );
    assert_eq!(
        subscribe_frames[0].historical_selectors,
        [tau_proto::EventSelector::Exact(
            EventName::HARNESS_SESSION_DIR
        )],
        "provider must request the current session-dir catch-up snapshot for restore-time debug policy",
    );

    let models_index = frames
        .iter()
        .position(|frame| {
            matches!(
                frame,
                HarnessInputMessage::Emit(emit)
                    if !emit.persist
                        && matches!(
                            emit.event.as_ref(),
                            Event::ProviderModelsDeclared(updated)
                                if model_ids(&updated.models)
                                    .starts_with(&["chatgpt/gpt-5.6-sol".to_owned()])
                        )
            )
        })
        .unwrap_or_else(|| panic!("startup frames should announce provider models: {frames:?}"));
    let ready_index = frames
        .iter()
        .position(|frame| matches!(frame, HarnessInputMessage::Ready(_)))
        .unwrap_or_else(|| panic!("startup frames should end with Ready: {frames:?}"));
    assert!(
        models_index < ready_index,
        "provider models must be announced before Ready: {frames:?}",
    );
}

/// A missing backend remains pending because external repair can restore it;
/// only disconnect ends the extension's retry loop.
#[test]
fn direct_prompt_request_with_missing_backend_remains_pending_until_disconnect() {
    let input = encode_frames(&[
        live_event(11, Event::AgentPromptCreated(prompt())),
        HarnessOutputMessage::Disconnect(tau_proto::Disconnect {
            reason: Some("done".to_owned()),
        }),
    ]);
    let writer = SharedWriter::default();
    let output = writer.clone();
    run_with_auth(Cursor::new(input), writer, OpenAiAuth::default())
        .expect("run provider extension");

    let frames = decode_frames(&output.bytes());
    let submitted = frames.iter().position(|frame| {
        matches!(
            input_event(frame),
            Some(Event::ProviderPromptSubmittedReported(submitted))
                if submitted.agent_prompt_id.as_str() == "sp-1"
        )
    });
    submitted.expect("prompt submitted event");
    assert!(
        frames.iter().all(|frame| !matches!(
            input_event(frame),
            Some(Event::ProviderResponseFinishedReported(finished))
                if finished.agent_prompt_id.as_str() == "sp-1"
        )),
        "reloadable missing backend must not be reported as terminal"
    );
}
