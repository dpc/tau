use std::collections::VecDeque;
use std::io::{BufReader, Cursor, Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use tau_proto::{
    Effort, HarnessInputMessage, HarnessInputReader, HarnessOutputMessage, HarnessOutputWriter,
    Verbosity,
};

use super::*;

/// Shared byte sink used by tests that run tau-client's writer thread.
#[derive(Clone, Default)]
struct SharedWriter {
    /// Shared byte buffer written by the provider runtime.
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl SharedWriter {
    /// Returns a snapshot of bytes written so far.
    fn bytes(&self) -> Vec<u8> {
        self.bytes.lock().expect("lock shared writer").clone()
    }
}

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.bytes.lock().expect("lock shared writer").extend(buf);
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
            Err(_) => return None,
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
                    instance_name: None,
                    state_dir: None,
                    secrets: std::collections::BTreeMap::new(),
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
        if let Some(frames) = try_decode_frames(&output.bytes())
            && predicate(&frames)
        {
            return frames;
        }
        assert!(
            Instant::now() < deadline,
            "runtime output boundary not reached"
        );
        thread::yield_now();
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
        session_id: "session-1".into(),
        path: std::path::PathBuf::from("/tmp/tau-test-session-1"),
        status,
    }
}

#[test]
fn retry_banner_emits_status_not_message_delta() {
    let mut bytes = Vec::new();
    {
        let mut writer = tau_proto::PeerOutputWriter::new(&mut bytes);
        emit_retry_banner(
            "sp-retry",
            &tau_proto::AgentId::parse("main").expect("agent id"),
            &tau_proto::PromptOriginator::User,
            &mut writer,
            &common::LlmError::HttpStatus(500, "temporary".to_owned()),
            Duration::from_secs(1),
            1,
        );
    }

    let frames = decode_frames(&bytes);
    let Some(Event::ProviderResponseUpdated(update)) = frames.first().and_then(input_event) else {
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

fn prompt() -> tau_proto::AgentPromptCreated {
    tau_proto::AgentPromptCreated {
        agent_prompt_id: "sp-1".into(),
        agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
        session_id: "session-1".into(),
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
    });
    let prompt_executor: PromptExecutor = Arc::new(|execution| {
        let mut writer = execution.frame_writer();
        writer
            .write_message(&HarnessInputMessage::emit(Event::ProviderResponseFinished(
                simple_finished(
                    execution.job.agent_prompt_id,
                    execution.job.prompt.agent_id,
                    execution.job.prompt.originator,
                    "done",
                ),
            )))
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
                Some(Event::ProviderResponseFinished(finished))
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
            session_id: "session-1".into(),
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
    });
    let prompt_executor: PromptExecutor = Arc::new(|execution| {
        let mut writer = execution.frame_writer();
        writer
            .write_message(&HarnessInputMessage::emit(Event::ProviderResponseFinished(
                simple_finished(
                    execution.job.agent_prompt_id,
                    execution.job.prompt.agent_id,
                    execution.job.prompt.originator,
                    "done",
                ),
            )))
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

/// Builds scheduler-owned logical state without starting a provider worker.
fn scheduled_job(prompt_id: &str, provider: &str) -> PromptJob {
    let mut prompt = prompt();
    prompt.agent_prompt_id = prompt_id.into();
    prompt.model.provider = ProviderName::new(provider);
    PromptJob {
        agent_prompt_id: prompt.agent_prompt_id.clone(),
        debug_provider_requests: false,
        prompt,
        backend: PromptBackend::Unavailable,
        retry_state: PromptRetryState::default(),
        cancel_generation: 0,
        manual_cooldown_bypass: false,
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
            .schedule(initial_due, scheduled_job(id, "limited"))
            .unwrap_or_else(|_| panic!("unique parked prompt"));
    }
    queue
        .schedule(unaffected_due, scheduled_job("peer", "healthy"))
        .unwrap_or_else(|_| panic!("unique parked prompt"));

    queue.extend_cooldown(&ProviderName::new("limited"), extended_boundary);

    let deadlines = queue.deadlines();
    assert_eq!(queue.len(), 5);
    let limited = deadlines
        .iter()
        .filter(|(_, provider, _)| provider.as_str() == "limited")
        .map(|(_, _, due)| *due)
        .collect::<Vec<_>>();
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

/// Verifies targeted and global delayed cancellation remove only the intended
/// logical prompts and never wait for their far-future deadlines.
#[test]
fn retry_schedule_queue_cancellation_is_prompt_scoped_and_immediate() {
    let due = Instant::now() + Duration::from_secs(86_400);
    let mut queue = RetryScheduleQueue::default();
    queue
        .schedule(due, scheduled_job("target", "limited"))
        .unwrap_or_else(|_| panic!("unique parked prompt"));
    queue
        .schedule(due, scheduled_job("peer", "limited"))
        .unwrap_or_else(|_| panic!("unique parked prompt"));

    let canceled = queue.cancel(&"target".into());
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
        .schedule(now, scheduled_job("timer-wins", "limited"))
        .unwrap_or_else(|_| panic!("unique parked prompt"));
    assert!(timer_wins.pop_due(now).is_some());
    assert!(timer_wins.cancel(&"timer-wins".into()).is_empty());

    let mut manual_wins = RetryScheduleQueue::default();
    manual_wins
        .schedule(now, scheduled_job("manual-wins", "limited"))
        .unwrap_or_else(|_| panic!("unique parked prompt"));
    assert_eq!(manual_wins.cancel(&"manual-wins".into()).len(), 1);
    assert!(manual_wins.pop_due(now).is_none());
}

/// Proves repeated manual commands cannot clone scheduler-owned state and a
/// peer parked under the same provider cooldown remains untouched.
#[test]
fn retry_schedule_queue_double_manual_release_moves_exactly_one_job() {
    let due = Instant::now() + Duration::from_secs(60);
    let mut queue = RetryScheduleQueue::default();
    queue
        .schedule(due, scheduled_job("target", "limited"))
        .unwrap_or_else(|_| panic!("unique parked prompt"));
    queue
        .schedule(due, scheduled_job("peer", "limited"))
        .unwrap_or_else(|_| panic!("unique parked prompt"));

    let first = queue.cancel(&"target".into());
    let second = queue.cancel(&"target".into());
    assert_eq!(first.len(), 1);
    assert!(second.is_empty());
    assert_eq!(queue.len(), 1);
    assert_eq!(
        queue.cancel(&"peer".into())[0].agent_prompt_id.as_str(),
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
    assert!(queue.schedule(due, job).is_ok(), "unique parked prompt");

    let mut transferred = queue.cancel(&"target".into()).pop().expect("parked job");
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
    let target: tau_proto::AgentPromptId = "target".into();
    let peer: tau_proto::AgentPromptId = "peer".into();
    let agent_id = tau_proto::AgentId::parse("agent-1").expect("agent id");
    let originator = tau_proto::PromptOriginator::User;
    tx.send(WorkerMessage::Output {
        message: Box::new(HarnessInputMessage::emit(Event::ProviderResponseUpdated(
            ProviderResponseUpdated {
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
            },
        ))),
        cancel_generation: 0,
        agent_prompt_id: target.clone(),
    })
    .expect("queue target delta");
    for (id, text) in [(&target, "stale success"), (&peer, "peer success")] {
        tx.send(WorkerMessage::Output {
            message: Box::new(HarnessInputMessage::emit(Event::ProviderResponseFinished(
                simple_finished(id.clone(), agent_id.clone(), originator.clone(), text),
            ))),
            cancel_generation: 0,
            agent_prompt_id: id.clone(),
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
                Some(Event::ProviderResponseUpdated(update))
                    if update.agent_prompt_id == target
            ))
            .count(),
        0,
        "queued tentative target delta must be discarded"
    );
    assert_eq!(
        committed
            .iter()
            .filter(|message| matches!(
                input_event(message),
                Some(Event::ProviderResponseFinished(finished))
                    if finished.agent_prompt_id == target
                        && finished.error.as_deref() == Some("(cancelled by harness)")
            ))
            .count(),
        1,
        "target lifecycle must close exactly once as canceled"
    );
    assert!(committed.iter().any(|message| matches!(
        input_event(message),
        Some(Event::ProviderResponseFinished(finished))
            if finished.agent_prompt_id == peer
                && finished.error.as_deref() == Some("peer success")
    )));
    assert!(
        !cancellation.is_canceled(&target),
        "targeted marker is consumed only by terminal commit"
    );

    let reused = validate_worker_output_for_commit(
        Box::new(HarnessInputMessage::emit(Event::ProviderResponseFinished(
            simple_finished(
                target.clone(),
                agent_id,
                originator,
                "reused prompt success",
            ),
        ))),
        0,
        0,
        false,
        &target,
        &cancellation,
    )
    .expect("reused prompt ID may commit");
    assert!(matches!(
        input_event(&reused),
        Some(Event::ProviderResponseFinished(finished))
            if finished.agent_prompt_id == target
                && finished.error.as_deref() == Some("reused prompt success")
    ));
}

#[test]
fn chatgpt_profile_publishes_models_even_without_auth_tokens() {
    // Profile existence is the registration signal. Auth validity affects
    // prompt execution, not whether the registered account's models are visible.
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

#[test]
fn resolves_chatgpt_to_codex_responses_backend() {
    // ChatGPT is OAuth-backed and enables Codex-specific transport and replay
    // features owned by this provider slice.
    let mut profiles = profiles_with_chatgpt_auth(chatgpt_auth());

    let config =
        resolve_responses_backend(&model_id(CHATGPT_PROVIDER_NAME, "gpt-5.4"), &mut profiles)
            .expect("chatgpt backend");

    assert_eq!(config.surface, responses::ResponsesSurface::ChatGpt);
    assert_eq!(config.base_url, tau_provider_chatgpt::DEFAULT_BASE_URL);
    assert_eq!(config.api_key, "access");
    assert_eq!(config.account_id.as_deref(), Some("account"));
    assert!(config.supports_websocket);
    assert!(config.supports_compaction);
    assert!(config.supports_phase);
    assert!(config.supports_encrypted_reasoning);
}

#[test]
fn chatgpt_phase_metadata_is_model_specific() {
    // The assistant `phase` field is only accepted by newer Codex model
    // families, so the hardcoded resolver must preserve the old whitelist.
    let mut profiles = profiles_with_chatgpt_auth(chatgpt_auth());

    let old = resolve_responses_backend(
        &model_id(CHATGPT_PROVIDER_NAME, "gpt-5.2-codex"),
        &mut profiles,
    )
    .expect("old codex backend");
    let new = resolve_responses_backend(
        &model_id(CHATGPT_PROVIDER_NAME, "gpt-5.3-codex"),
        &mut profiles,
    )
    .expect("new codex backend");

    assert!(!old.supports_phase);
    assert!(new.supports_phase);
}

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

#[test]
fn verbosity_metadata_is_published_for_chatgpt_models() {
    // The provider snapshot is authoritative for UI cycling, so ChatGPT
    // models must publish the verbosity choices they accept.
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

#[test]
fn prompt_workers_start_concurrently() {
    // Regression coverage for backend-agent parallelism: two accepted
    // provider prompts must both enter worker execution before the first
    // one finishes. A serial dispatcher would time out the first worker's
    // wait and never observe two active starts at once. See
    // `DESIGN-tau-ext-provider-builtin-bounded-prompt-workers`.
    let mut first = prompt();
    first.agent_prompt_id = "sp-par-1".into();
    let mut second = prompt();
    second.agent_prompt_id = "sp-par-2".into();
    let input = encode_frames(&[
        live_event(11, Event::AgentPromptCreated(first)),
        live_event(12, Event::AgentPromptCreated(second)),
    ]);
    let started = std::sync::Arc::new((Mutex::new((0_usize, 0_usize)), Condvar::new()));
    let executor_started = started.clone();
    let executor: PromptExecutor = std::sync::Arc::new(move |execution| {
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
            .write_message(&HarnessInputMessage::emit(Event::ProviderResponseFinished(
                simple_finished(
                    agent_prompt_id.clone(),
                    tau_proto::AgentId::parse("agent-1").expect("valid test agent id"),
                    originator,
                    "done",
                ),
            )))
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
        .filter(|frame| matches!(input_event(frame), Some(Event::ProviderResponseFinished(_))))
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
        .schedule(due, scheduled_job("same", "limited"))
        .unwrap_or_else(|_| panic!("first owner"));
    let duplicate = queue.schedule(due, scheduled_job("same", "limited"));
    assert!(duplicate.is_err(), "duplicate APID must be returned");
    assert_eq!(queue.len(), 1, "original remains the sole parked owner");
}

/// Ensures a retryable attempt is parked outside the worker pool and later
/// succeeds without duplicating the logical prompt lifecycle.
#[test]
fn retryable_attempt_is_rescheduled_then_finishes_once() {
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
                },
            )
            .expect("return retry outcome");
            return;
        }
        let mut writer = execution.frame_writer();
        writer
            .write_message(&HarnessInputMessage::emit(Event::ProviderResponseFinished(
                simple_finished(
                    execution.job.agent_prompt_id,
                    execution.job.prompt.agent_id,
                    execution.job.prompt.originator,
                    "done",
                ),
            )))
            .expect("finished frame");
        writer.flush().expect("flush finished frame");
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

    // Wait until the failed attempt has crossed the main loop and is owned by
    // the scheduler, then exercise the real control path rather than waiting
    // for its timer.
    wait_for_runtime_frames(&output, |frames| {
        frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseUpdated(update))
                    if update.status.as_ref().is_some_and(|status| status.retry.is_some())
            )
        })
    });
    input.push(encode_frames(&[live_event(
        12,
        Event::UiRetryPrompt(tau_proto::UiRetryPrompt {
            request_id: tau_proto::RetryPromptRequestId::parse("runtime-manual-1")
                .expect("valid retry request id"),
            session_id: "s1".into(),
            target_agent_id: Some(
                tau_proto::AgentId::parse("agent-1").expect("valid test agent id"),
            ),
            agent_prompt_id: Some("sp-1".into()),
        }),
    )]));

    let frames = wait_for_runtime_frames(&output, |frames| {
        frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseFinished(finished))
                    if finished.agent_prompt_id.as_str() == "sp-1"
            )
        })
    });
    let submitted = frames
        .iter()
        .filter(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderPromptSubmitted(submitted))
                    if submitted.agent_prompt_id.as_str() == "sp-1"
            )
        })
        .count();
    let finished = frames
        .iter()
        .filter(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseFinished(finished))
                    if finished.agent_prompt_id.as_str() == "sp-1"
            )
        })
        .count();
    assert_eq!(submitted, 1);
    assert_eq!(finished, 1);
    assert!(frames.iter().any(|frame| {
        matches!(
            input_event(frame),
            Some(Event::ProviderRetryPromptResult(result))
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

/// Ensures manual scheduler ownership transfer decrements the delayed count in
/// the main loop, so EOF can finish after the admitted attempt completes.
#[test]
fn manual_retry_transfer_clears_delayed_count_through_main_loop() {
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
                },
            )
            .expect("park first attempt");
        } else {
            let mut writer = execution.frame_writer();
            writer
                .write_message(&HarnessInputMessage::emit(Event::ProviderResponseFinished(
                    simple_finished(
                        execution.job.agent_prompt_id,
                        execution.job.prompt.agent_id,
                        execution.job.prompt.originator,
                        "done",
                    ),
                )))
                .expect("finish manual attempt");
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
    assert_eq!(attempt_rx.recv().expect("first attempt"), 0);
    wait_for_runtime_frames(&output, |frames| {
        frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseUpdated(update))
                    if update.status.as_ref().is_some_and(|status| status.retry.is_some())
            )
        })
    });
    input.push(encode_frames(&[live_event(
        12,
        Event::UiRetryPrompt(tau_proto::UiRetryPrompt {
            request_id: tau_proto::RetryPromptRequestId::parse("count-transfer")
                .expect("valid retry request id"),
            session_id: "session-1".into(),
            target_agent_id: None,
            agent_prompt_id: Some("sp-1".into()),
        }),
    )]));
    assert_eq!(attempt_rx.recv().expect("manually admitted attempt"), 1);
    input.close();
    runtime.join().expect("runtime exits with no delayed owner");
    let frames = decode_frames(&output.bytes());
    assert!(frames.iter().any(|frame| matches!(
        input_event(frame),
        Some(Event::ProviderRetryPromptResult(result))
            if result.request_id.as_str() == "count-transfer"
                && result.status == tau_proto::RetryPromptStatus::Accepted
    )));
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
                Some(Event::ProviderResponseUpdated(update))
                    if update.status.as_ref().is_some_and(|status| status.retry.is_some())
            )
        })
    });
    input.push(encode_frames(&[
        live_event(
            12,
            Event::SessionShutdown(tau_proto::SessionShutdown {
                session_id: "session-1".into(),
            }),
        ),
        live_event(
            13,
            Event::UiRetryPrompt(tau_proto::UiRetryPrompt {
                request_id: tau_proto::RetryPromptRequestId::parse("after-shutdown")
                    .expect("valid retry request id"),
                session_id: "session-1".into(),
                target_agent_id: None,
                agent_prompt_id: Some("sp-1".into()),
            }),
        ),
    ]));
    let frames = wait_for_runtime_frames(&output, |frames| {
        frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderRetryPromptResult(result))
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
                Some(Event::ProviderPromptSubmitted(submitted))
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
                Some(Event::ProviderResponseFinished(finished))
                    if finished.agent_prompt_id.as_str() == "sp-1"
            ))
            .count(),
        1
    );
    assert!(frames.iter().any(|frame| matches!(
        input_event(frame),
        Some(Event::ProviderRetryPromptResult(result))
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
                },
            )
            .expect("return retry");
        } else {
            let mut writer = execution.frame_writer();
            writer
                .write_message(&HarnessInputMessage::emit(Event::ProviderResponseFinished(
                    simple_finished(
                        execution.job.agent_prompt_id,
                        execution.job.prompt.agent_id,
                        execution.job.prompt.originator,
                        "done",
                    ),
                )))
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
                        Some(Event::ProviderResponseUpdated(update))
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
                session_id: "session-1".into(),
                target_agent_id: None,
                agent_prompt_id: Some("sp-1".into()),
            }),
        )]));
        assert_eq!(attempt_rx.recv().expect("manual attempt"), expected_attempt);
    }
    let frames = wait_for_runtime_frames(&output, |frames| {
        frames.iter().any(|frame| {
            matches!(
                input_event(frame),
                Some(Event::ProviderResponseFinished(finished))
                    if finished.agent_prompt_id.as_str() == "sp-1"
            )
        })
    });
    let retry_attempts = frames
        .iter()
        .filter_map(|frame| match input_event(frame) {
            Some(Event::ProviderResponseUpdated(update)) => update
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
                Some(Event::ProviderResponseFinished(finished))
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
            .write_message(&HarnessInputMessage::emit(Event::ProviderResponseFinished(
                finished,
            )))
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

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let frames = decode_frames(&output.bytes());
        let terminal: Vec<_> = frames
            .iter()
            .filter_map(|frame| match input_event(frame) {
                Some(Event::ProviderResponseFinished(finished))
                    if finished.agent_prompt_id.as_str() == "sp-1" =>
                {
                    Some(finished)
                }
                _ => None,
            })
            .collect();
        if let Some(finished) = terminal.first() {
            assert_eq!(terminal.len(), 1);
            assert_eq!(
                finished.failure_kind,
                Some(tau_proto::ProviderFailureKind::ContextWindowExceeded)
            );
            assert!(!frames.iter().any(|frame| matches!(
                input_event(frame),
                Some(Event::ProviderResponseUpdated(update)) if update.status.is_some()
            )));
            break;
        }
        assert!(Instant::now() < deadline, "terminal response not emitted");
        thread::sleep(Duration::from_millis(5));
    }
    input.push(encode_frames(&[HarnessOutputMessage::Disconnect(
        tau_proto::Disconnect {
            reason: Some("done".to_owned()),
        },
    )]));
    input.close();
    runtime.join().expect("runtime join");
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

/// Proves four far-future retries release all four bounded worker permits so an
/// unrelated provider can run immediately, with no attempt before fake repair.
#[test]
fn four_delayed_prompts_release_capacity_for_an_unrelated_provider() {
    let input = BlockingInput::default();
    let mut frames = Vec::new();
    for index in 1..=4 {
        let mut limited = prompt();
        limited.agent_prompt_id = format!("limited-{index}").into();
        limited.model.provider = ProviderName::new("limited");
        frames.push(live_event(10 + index, Event::AgentPromptCreated(limited)));
    }
    let mut healthy = prompt();
    healthy.agent_prompt_id = "healthy".into();
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
                    decision: RetryDecision::new(RetryClass::UsageWindow)
                        .with_retry_after(Some(Duration::from_secs(86_400))),
                },
            )
            .expect("park limited prompt");
            return;
        }
        let mut writer = execution.frame_writer();
        writer
            .write_message(&HarnessInputMessage::emit(Event::ProviderResponseFinished(
                simple_finished(
                    execution.job.agent_prompt_id,
                    execution.job.prompt.agent_id,
                    execution.job.prompt.originator,
                    "healthy done",
                ),
            )))
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
                Some(Event::ProviderPromptSubmitted(submitted))
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
                Some(Event::ProviderResponseFinished(finished))
                    if finished.agent_prompt_id.as_str() == "healthy"
            ))
            .count(),
        1
    );
}

/// Verifies every due attempt re-resolves mutable profile state while retaining
/// the startup-selected Responses mode: repaired credentials replace stale
/// captures, an opposite on-disk mode edit is ignored, and later deletion
/// becomes Unavailable.
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
        account_id: Some("fresh-account".to_owned()),
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
                assert_eq!(config.api_key, "old-token");
                assert_eq!(config.mode, responses::ResponsesMode::LiteCompatibility);
                *profiles_for_executor.lock().expect("mutable profiles") =
                    profiles_with_chatgpt_auth(fresh.clone());
            }
            (1, PromptBackend::Responses(config)) => {
                assert_eq!(config.api_key, "fresh-token");
                assert_eq!(config.account_id.as_deref(), Some("fresh-account"));
                assert_eq!(
                    config.mode,
                    responses::ResponsesMode::LiteCompatibility,
                    "retry must retain the startup mode after a standard-mode disk edit"
                );
                *profiles_for_executor.lock().expect("mutable profiles") =
                    BuiltinProviderProfiles::default();
            }
            (2, PromptBackend::Unavailable) => {
                let mut writer = execution.frame_writer();
                writer
                    .write_message(&HarnessInputMessage::emit(Event::ProviderResponseFinished(
                        simple_finished(
                            execution.job.agent_prompt_id,
                            execution.job.prompt.agent_id,
                            execution.job.prompt.originator,
                            "observed unavailable",
                        ),
                    )))
                    .expect("finish after deletion");
                writer.flush().expect("flush deletion finish");
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
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    let frames = decode_frames(&output.bytes());
    assert_eq!(
        frames
            .iter()
            .filter(|frame| matches!(
                input_event(frame),
                Some(Event::ProviderPromptSubmitted(submitted))
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
                Some(Event::ProviderResponseFinished(finished))
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
                .write_message(&HarnessInputMessage::emit(Event::ProviderResponseUpdated(
                    ProviderResponseUpdated {
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
                    },
                )))
                .expect("tentative update");
            writer.flush().expect("flush tentative update");
            send_worker_message(
                &execution.output_tx,
                &execution.output_waker,
                WorkerMessage::Retry {
                    job: execution.job,
                    decision: RetryDecision::new(RetryClass::Transport),
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
            .write_message(&HarnessInputMessage::emit(Event::ProviderResponseFinished(
                finished,
            )))
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
    assert!(frames.iter().any(|frame| matches!(
        input_event(frame),
        Some(Event::ProviderResponseUpdated(update))
            if update.deltas.iter().any(|delta| matches!(
                delta,
                tau_proto::ProviderResponseTextDelta::Message { text, .. }
                    if text == "attempt-one-tentative"
            ))
    )));
    assert!(frames.iter().any(|frame| matches!(
        input_event(frame),
        Some(Event::ProviderResponseUpdated(update))
            if update.status.as_ref().is_some_and(|status| status.clear_response)
    )));
    let finished = frames
        .iter()
        .filter_map(|frame| match input_event(frame) {
            Some(Event::ProviderResponseFinished(finished)) => Some(finished),
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
    chatgpt_prompt.agent_prompt_id = "chatgpt-retry".into();
    let mut generic_prompt = prompt();
    generic_prompt.agent_prompt_id = "generic-retry".into();
    generic_prompt.model = model_id("generic", "generic-model");
    let mut router_prompt = prompt();
    router_prompt.agent_prompt_id = "router-retry".into();
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
                assert_eq!(config.api_key, "access");
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
                },
            )
            .expect("schedule family retry");
            return;
        }
        let mut writer = execution.frame_writer();
        writer
            .write_message(&HarnessInputMessage::emit(Event::ProviderResponseFinished(
                simple_finished(
                    execution.job.agent_prompt_id,
                    execution.job.prompt.agent_id,
                    execution.job.prompt.originator,
                    "family done",
                ),
            )))
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

    let mut completed = std::collections::BTreeSet::new();
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
                    Some(Event::ProviderPromptSubmitted(submitted))
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
                    Some(Event::ProviderResponseFinished(finished))
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

/// Runs the mixed delayed/active/queued lifecycle fixture for broadcast cancel
/// or input EOF, both of which must close every accepted prompt exactly once.
fn assert_mixed_state_shutdown(shutdown: MixedStateShutdown) {
    let input = BlockingInput::default();
    let mut delayed = prompt();
    delayed.agent_prompt_id = "mixed-delayed".into();
    let mut active = prompt();
    active.agent_prompt_id = "mixed-active".into();
    let mut queued = prompt();
    queued.agent_prompt_id = "mixed-queued".into();
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
    let executor: PromptExecutor = Arc::new(move |execution| {
        let id = execution.job.agent_prompt_id.to_string();
        executor_calls.lock().expect("mixed calls").push(id.clone());
        match id.as_str() {
            "mixed-delayed" => {
                send_worker_message(
                    &execution.output_tx,
                    &execution.output_waker,
                    WorkerMessage::Retry {
                        job: execution.job,
                        decision: RetryDecision::new(RetryClass::Transport)
                            .with_retry_after(Some(Duration::from_secs(86_400))),
                    },
                )
                .expect("park delayed prompt");
                delayed_tx.send(()).expect("report delayed state");
            }
            "mixed-active" => {
                let cancel_tx = active_cancel_tx.clone();
                let _active_abort_waker_guard = execution.cancellation.register_abort_waker(
                    &execution.job.agent_prompt_id,
                    execution.job.cancel_generation,
                    Arc::new(move || {
                        cancel_tx.send(()).expect("report active cancellation");
                    }),
                );
                active_tx.send(()).expect("report active state");
                if matches!(shutdown, MixedStateShutdown::Eof) {
                    while !execution
                        .cancellation
                        .is_canceled(&execution.job.agent_prompt_id)
                    {
                        thread::yield_now();
                    }
                } else {
                    active_cancel_rx
                        .lock()
                        .expect("active cancel receiver")
                        .recv_timeout(Duration::from_secs(1))
                        .expect("global cancel did not wake active backend");
                }
                let mut writer = execution.frame_writer();
                writer
                    .write_message(&HarnessInputMessage::emit(Event::ProviderResponseFinished(
                        simple_finished(
                            execution.job.agent_prompt_id,
                            execution.job.prompt.agent_id,
                            execution.job.prompt.originator,
                            "late success must become canceled",
                        ),
                    )))
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

    delayed_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("delayed state");
    active_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("active state");
    match shutdown {
        MixedStateShutdown::GlobalCancel => {
            input.push(encode_frames(&[live_event(
                20,
                Event::UiCancelPrompt(tau_proto::UiCancelPrompt {
                    session_id: tau_proto::SessionId::new("test-session"),
                    target_agent_id: None,
                    agent_prompt_id: None,
                }),
            )]));
        }
        MixedStateShutdown::Eof => input.close(),
    }

    if matches!(shutdown, MixedStateShutdown::GlobalCancel) {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let frames = try_decode_frames(&output.bytes()).unwrap_or_default();
            let canceled = frames
                .iter()
                .filter(|frame| {
                    matches!(
                        input_event(frame),
                        Some(Event::ProviderResponseFinished(finished))
                            if finished.error.as_deref() == Some("(cancelled by harness)")
                    )
                })
                .count();
            if canceled == 3 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "global cancel did not close all mixed states"
            );
            thread::yield_now();
        }
        input.push(encode_frames(&[HarnessOutputMessage::Disconnect(
            tau_proto::Disconnect {
                reason: Some("done".to_owned()),
            },
        )]));
        input.close();
    }
    runtime.join().expect("mixed runtime join");

    let frames = decode_frames(&output.bytes());
    for id in ["mixed-delayed", "mixed-active", "mixed-queued"] {
        assert_eq!(
            frames
                .iter()
                .filter(|frame| matches!(
                    input_event(frame),
                    Some(Event::ProviderPromptSubmitted(submitted))
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
                    Some(Event::ProviderResponseFinished(finished))
                        if finished.agent_prompt_id.as_str() == id
                            && finished.error.as_deref() == Some("(cancelled by harness)")
                ))
                .count(),
            1
        );
    }
    assert_eq!(
        *calls.lock().expect("mixed calls"),
        vec!["mixed-delayed".to_owned(), "mixed-active".to_owned()],
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

/// A real Chat Completions transport repetition reaches the production
/// executor/runtime as one local deterministic terminal with no reschedule.
#[test]
fn real_repetition_failure_finishes_once_without_scheduler_retry() {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind repetition server");
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
    created.agent_prompt_id = "real-terminal".into();
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
                Some(Event::ProviderResponseFinished(finished))
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
            Some(Event::ProviderResponseFinished(finished))
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
        Some(Event::ProviderResponseUpdated(update))
            if update.agent_prompt_id.as_str() == "real-terminal"
                && update.status.as_ref().is_some_and(|status|
                    status.text.contains("next attempt"))
    )));
}

/// Retry statuses are one-per-attempt, bounded, provider-content-free, and the
/// final terminal event closes the transient status lifecycle.
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
                },
            )
            .expect("schedule status fixture retry");
            return;
        }
        let mut writer = execution.frame_writer();
        writer
            .write_message(&HarnessInputMessage::emit(Event::ProviderResponseFinished(
                simple_finished(
                    execution.job.agent_prompt_id,
                    execution.job.prompt.agent_id,
                    execution.job.prompt.originator,
                    "done",
                ),
            )))
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
                Some(Event::ProviderResponseFinished(finished))
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
            Some(Event::ProviderResponseUpdated(update)) => update.status.as_ref(),
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
    }
    assert!(matches!(
        frames.last().and_then(input_event),
        Some(Event::ProviderResponseFinished(finished))
            if finished.agent_prompt_id.as_str() == "sp-1"
    ));
}

/// A queued targeted cancel consumes its marker after terminal commit so the
/// same prompt ID can be accepted again without inheriting stale cancellation.
#[test]
fn queued_targeted_cancel_allows_prompt_id_reuse() {
    let input = BlockingInput::default();
    let mut active = prompt();
    active.agent_prompt_id = "occupying".into();
    let mut queued = prompt();
    queued.agent_prompt_id = "reused".into();
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
            .write_message(&HarnessInputMessage::emit(Event::ProviderResponseFinished(
                simple_finished(
                    execution.job.agent_prompt_id,
                    execution.job.prompt.agent_id,
                    execution.job.prompt.originator,
                    "done",
                ),
            )))
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
            session_id: tau_proto::SessionId::new("test-session"),
            target_agent_id: None,
            agent_prompt_id: Some("reused".into()),
        }),
    )]));
    input.wait_for_reader_waiting(Duration::from_secs(1));
    let mut reused_prompt = prompt();
    reused_prompt.agent_prompt_id = "reused".into();
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
                    Some(Event::ProviderResponseFinished(finished))
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
            Some(Event::ProviderResponseFinished(finished))
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
            session_id: tau_proto::SessionId::new("test-session"),
            target_agent_id: None,
            agent_prompt_id: Some("sp-1".into()),
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
                Some(Event::ProviderResponseFinished(finished))
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
                Some(Event::ProviderResponseFinished(finished))
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
        if frames
            .iter()
            .any(|frame| matches!(input_event(frame), Some(Event::ProviderPromptSubmitted(_))))
        {
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
                    Event::ProviderPromptSubmitted(_)
                        | Event::ProviderResponseUpdated(_)
                        | Event::ProviderResponseFinished(_)
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
            tau_proto::EventSelector::Exact(EventName::HARNESS_SESSION_DIR),
            tau_proto::EventSelector::Exact(EventName::UI_CANCEL_PROMPT),
            tau_proto::EventSelector::Exact(EventName::SESSION_SHUTDOWN),
        ],
        "provider startup subscriptions must stay exact and must not include direct prompt routing",
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
                input_event(frame),
                Some(Event::ProviderModelsUpdated(updated))
                    if model_ids(&updated.models).starts_with(&["chatgpt/gpt-5.6-sol".to_owned()])
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

#[test]
fn direct_prompt_request_with_missing_backend_remains_pending_until_disconnect() {
    // Missing credentials/profile state can be repaired externally, so it is not
    // a proven terminal request failure. Process disconnect still ends retries.
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
            Some(Event::ProviderPromptSubmitted(submitted))
                if submitted.agent_prompt_id.as_str() == "sp-1"
        )
    });
    submitted.expect("prompt submitted event");
    assert!(
        frames.iter().all(|frame| !matches!(
            input_event(frame),
            Some(Event::ProviderResponseFinished(finished))
                if finished.agent_prompt_id.as_str() == "sp-1"
        )),
        "reloadable missing backend must not be reported as terminal"
    );
}
