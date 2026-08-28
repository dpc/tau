//! Slack send-delivery state machine, retry, replay, and concurrency tests.

use std::io as path_std_io;

use super::*;
use crate::send_delivery::{InternalSourceMention, PostCompositionError, SendLedgerDisposition};

/// Multi-step scheduler exposing every logical wait to a deterministic test.
struct StepScheduler {
    entered: mpsc::Sender<Duration>,
    release: Mutex<mpsc::Receiver<()>>,
}

impl SendScheduler for StepScheduler {
    fn wait(
        &self,
        wake: &SendWake,
        generation: crate::generations::SlackSendWakeGeneration,
        delay: Duration,
    ) -> bool {
        self.entered.send(delay).expect("announce scheduled wait");
        self.release
            .lock()
            .expect("release")
            .recv()
            .expect("release scheduled wait");
        wake.generation() != generation
    }
}

/// Scripted typed post boundary used by retry and ambiguity tests.
struct ScriptedPostClient {
    /// Exact wire bodies observed in attempt order.
    bodies: Mutex<Vec<String>>,
    /// Typed provider outcomes consumed in attempt order.
    outcomes: Mutex<VecDeque<PostAttemptOutcome<PostedMessage>>>,
}

impl ScriptedPostClient {
    /// Construct a client with exact ordered provider outcomes.
    fn new(outcomes: impl IntoIterator<Item = PostAttemptOutcome<PostedMessage>>) -> Arc<Self> {
        Arc::new(Self {
            bodies: Mutex::new(Vec::new()),
            outcomes: Mutex::new(outcomes.into_iter().collect()),
        })
    }
}

impl SlackClient for ScriptedPostClient {
    fn open_socket(&self, _cfg: &RuntimeConfig) -> Result<String, SlackApiError> {
        unreachable!("post-only client")
    }

    fn auth_test(&self, _cfg: &RuntimeConfig) -> Result<SlackInstallationIdentity, SlackApiError> {
        Ok(SlackInstallationIdentity {
            bot_user_id: "UBOT123".to_owned(),
            team_id: "T123".to_owned(),
        })
    }

    fn verified_human_identity(
        &self,
        _cfg: &RuntimeConfig,
        _user_id: &str,
    ) -> Result<Option<VerifiedSlackHuman>, SlackApiError> {
        unreachable!("post-only client")
    }

    fn post_message(
        &self,
        _cfg: &RuntimeConfig,
        body: &FrozenPostBody,
    ) -> PostAttemptOutcome<PostedMessage> {
        self.bodies
            .lock()
            .expect("bodies")
            .push(body.wire_json().to_owned());
        self.outcomes
            .lock()
            .expect("outcomes")
            .pop_front()
            .expect("scripted post outcome")
    }
}

impl ReactionClient for ScriptedPostClient {
    fn react(
        &self,
        _cfg: &RuntimeConfig,
        _action: ReactionActionKind,
        _channel_id: &str,
        _message_ts: &str,
        _emoji: &str,
    ) -> Result<(), ReactionApiError> {
        unreachable!("post-only client")
    }
}

/// Reader that keeps the harness connection open after delivering fixture
/// frames, proving fatal shutdown does not rely on harness EOF.
struct BlockingAfterInputReader {
    /// Encoded startup and live-delivery frames.
    input: path_std_io::Cursor<Vec<u8>>,
    /// Gate released after the production loop exits.
    gate: Arc<(Mutex<bool>, Condvar)>,
}

impl Read for BlockingAfterInputReader {
    fn read(&mut self, buffer: &mut [u8]) -> path_std_io::Result<usize> {
        let read = self.input.read(buffer)?;
        if read != 0 {
            return Ok(read);
        }
        let (lock, condvar) = &*self.gate;
        let mut blocked = lock.lock().expect("reader gate");
        while *blocked {
            blocked = condvar.wait(blocked).expect("wait reader gate");
        }
        Ok(0)
    }
}

/// Writer that fails the asynchronous typed error report after startup output
/// has succeeded.
struct AsyncErrorReportFailureWriter;

impl Write for AsyncErrorReportFailureWriter {
    fn write(&mut self, buffer: &[u8]) -> path_std_io::Result<usize> {
        if buffer
            .windows(b"tool.error_reported".len())
            .any(|window| window == b"tool.error_reported")
        {
            return Err(path_std_io::Error::other(
                "forced asynchronous terminal failure",
            ));
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> path_std_io::Result<()> {
        Ok(())
    }
}

/// Install a causal notification for one bounded send worker's final release.
fn send_worker_release_notification(ext: &Extension) -> mpsc::Receiver<()> {
    let (sender, receiver) = mpsc::channel();
    *ext.test_hooks.send_worker_released.lock().expect("hook") = Some(sender);
    receiver
}

/// Wait for one causally observed send-worker release with a finite bound.
fn wait_for_send_worker(receiver: &mpsc::Receiver<()>) {
    receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("send worker release deadline");
}

/// Deterministic agent-text rejection must not contact Slack merely to
/// establish installation evidence or freeze otherwise replaceable
/// configuration.
#[test]
fn invalid_agent_text_fails_before_installation_preflight() {
    let (ext, _rx, client) = extension();
    apply_test_config(&ext, proactive_cfg());
    {
        let mut state = ext.state.lock().expect("state");
        state.socket.bot_user_id = None;
        state.socket.installation_team_id = None;
    }
    let invoke = tool(
        SEND_TOOL_NAME,
        "agent-a",
        tau_proto::json_to_cbor(
            &serde_json::json!({"message":"unsafe <@U123>","destination":"team-ops"}),
        ),
    );

    assert!(matches!(ext.handle_send(invoke), Some(Event::ToolError(_))));
    assert_eq!(*client.auth_count.lock().expect("auth count"), 0);
    assert!(client.sent_pairs().is_empty());
    assert!(!ext.state.lock().expect("state").configuration.config_frozen);
}

/// Proactive calls submit the sent report before the correlated
/// ordinary tool result without a special harness messaging handshake.
#[test]
fn proactive_send_submits_report_before_tool_result() {
    let (ext, rx, client) = extension();
    apply_test_config(&ext, proactive_cfg());
    let event = ext.handle_send(tool(
        SEND_TOOL_NAME,
        "agent-a",
        tau_proto::json_to_cbor(
            &serde_json::json!({"message": "update", "destination": "team-ops"}),
        ),
    ));
    assert!(event.is_none());
    let HarnessInputMessage::Emit(emit) = rx.recv().expect("sent report") else {
        panic!("expected sent report");
    };
    assert!(!emit.persist);
    let Event::MessageSentReported(report) = *emit.event else {
        panic!("expected sent report");
    };
    assert_eq!(report.text, "update");
    assert_eq!(report.publisher_extension_id.as_str(), "std-slack");
    assert!(matches!(
        rx.recv().expect("tool result"),
        HarnessInputMessage::Emit(emit)
            if !emit.persist
                && matches!(emit.event.as_ref(), Event::ToolResultReported(_))
    ));
    assert_eq!(client.sent_pairs().len(), 1);
}

/// Canonical sent correlation requires exact type, agent, publisher, and
/// message ID before it installs authority and exposes stable replay.
#[test]
fn accepted_proactive_replay_returns_stable_result_without_reposting() {
    let (ext, rx, client) = extension();
    apply_test_config(&ext, proactive_cfg());
    let invoke = tool(
        SEND_TOOL_NAME,
        "agent-a",
        tau_proto::json_to_cbor(
            &serde_json::json!({"message": "update", "destination": "team-ops"}),
        ),
    );
    assert!(ext.handle_send(invoke.clone()).is_none());
    let HarnessInputMessage::Emit(report) = rx.recv().expect("sent report") else {
        panic!("expected sent report");
    };
    let Event::MessageSentReported(report) = *report.event else {
        panic!("expected sent report");
    };
    let message_id = report.message_id.clone();
    let HarnessInputMessage::Emit(first) = rx.recv().expect("first result") else {
        panic!("expected result emission");
    };
    let Event::ToolResultReported(first_result) = first.event.as_ref() else {
        panic!("expected result report");
    };
    assert!(
        ext.handle_send(invoke.clone()).is_none(),
        "local report flush must not terminalize the ledger"
    );
    assert!(matches!(
        ext.state
            .lock()
            .expect("state")
            .sends
            .send_ledger
            .get(&invoke.call_id)
            .map(|entry| &entry.disposition),
        Some(SendLedgerDisposition::PendingCanonical(_))
    ));
    assert!(
        !ext.state
            .lock()
            .expect("state")
            .ingress
            .reactions
            .targets
            .contains_key(&message_id)
    );
    let Event::MessageSent(canonical) = Event::MessageSentReported(report)
        .into_stamped_canonical_message_fact(
            tau_proto::MessagePublisherId::parse("std-slack").expect("publisher"),
        )
        .expect("canonical sent fact")
    else {
        panic!("expected canonical sent fact");
    };
    let mut wrong_publisher = canonical.clone();
    wrong_publisher.publisher_extension_id =
        tau_proto::MessagePublisherId::parse("other-slack").expect("publisher");
    ext.apply_live_event(&Event::MessageSent(wrong_publisher));
    assert!(
        ext.handle_send(invoke.clone()).is_none(),
        "another configured publisher must not complete the send"
    );
    let mut wrong_message = canonical.clone();
    wrong_message.message_id = MessageFactId::new("slack-message:wrong");
    ext.apply_live_event(&Event::MessageSent(wrong_message));
    assert!(ext.handle_send(invoke.clone()).is_none());
    let mut wrong_agent = canonical.clone();
    wrong_agent.agent_id = MessageAgentTarget::new("agent-b");
    ext.apply_live_event(&Event::MessageSent(wrong_agent));
    assert!(ext.handle_send(invoke.clone()).is_none());
    assert!(
        !ext.state
            .lock()
            .expect("state")
            .ingress
            .reactions
            .targets
            .contains_key(&message_id)
    );
    ext.apply_live_event(&Event::MessageSent(canonical));
    assert!(matches!(
        ext.state
            .lock()
            .expect("state")
            .sends
            .send_ledger
            .get(&invoke.call_id)
            .map(|entry| &entry.disposition),
        Some(SendLedgerDisposition::Completed { .. })
    ));
    assert!(
        ext.state
            .lock()
            .expect("state")
            .ingress
            .reactions
            .targets
            .contains_key(&message_id)
    );
    let Some(replay) = ext.handle_send(invoke.clone()) else {
        panic!("canonical acknowledgement must complete replay");
    };
    let Event::ToolResult(replay_result) = replay else {
        panic!("completed replay must return an internal result");
    };
    assert_eq!(first_result, &replay_result);
    assert!(rx.try_recv().is_err());
    assert_eq!(client.sent_pairs().len(), 1);
}

/// A canonical echo that races ahead of typed-result submission is retained and
/// completes authority after the independent result write succeeds.
#[test]
fn canonical_echo_during_result_submission_is_reconciled() {
    let (ext, rx, _client) = extension();
    apply_test_config(&ext, proactive_cfg());
    let worker_released = send_worker_release_notification(&ext);
    let (reached_tx, reached_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    *ext.test_hooks.sent_report_boundary.lock().expect("hook") = Some(BlockingTestHook {
        reached: reached_tx,
        release: release_rx,
    });
    let invoke = tool(
        SEND_TOOL_NAME,
        "agent-a",
        tau_proto::json_to_cbor(
            &serde_json::json!({"message": "update", "destination": "team-ops"}),
        ),
    );
    let ext = Arc::new(ext);
    let sending = Arc::clone(&ext);
    let worker = std::thread::spawn(move || sending.handle_send(invoke));
    let HarnessInputMessage::Emit(report) = rx.recv().expect("sent report") else {
        panic!("expected sent report");
    };
    let Event::MessageSentReported(report) = *report.event else {
        panic!("expected sent report");
    };
    reached_rx.recv().expect("report boundary");
    let canonical = Event::MessageSentReported(report)
        .into_stamped_canonical_message_fact(
            tau_proto::MessagePublisherId::parse("std-slack").expect("publisher"),
        )
        .expect("canonical sent fact");
    ext.apply_live_event(&canonical);
    release_tx.send(()).expect("release result write");
    assert!(worker.join().expect("send worker").is_none());
    assert!(matches!(
        rx.recv().expect("tool result"),
        HarnessInputMessage::Emit(emit)
            if matches!(emit.event.as_ref(), Event::ToolResultReported(_))
    ));
    wait_for_send_worker(&worker_released);
    assert!(
        ext.state
            .lock()
            .expect("state")
            .ingress
            .reactions
            .targets
            .len()
            == 1
    );
}

/// An early canonical echo cannot install authority when the later independent
/// typed-result write fails and retires protocol output.
#[test]
fn canonical_echo_before_result_failure_installs_no_authority() {
    let (ext, rx, _client) = extension();
    apply_test_config(&ext, proactive_cfg());
    let (reached_tx, reached_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (failure_tx, failure_rx) = mpsc::channel();
    let (finish_failure_tx, finish_failure_rx) = mpsc::channel();
    *ext.test_hooks.sent_report_boundary.lock().expect("hook") = Some(BlockingTestHook {
        reached: reached_tx,
        release: release_rx,
    });
    *ext.test_hooks.output_failure_boundary.lock().expect("hook") = Some(BlockingTestHook {
        reached: failure_tx,
        release: finish_failure_rx,
    });
    let invoke = tool(
        SEND_TOOL_NAME,
        "agent-a",
        tau_proto::json_to_cbor(
            &serde_json::json!({"message": "update", "destination": "team-ops"}),
        ),
    );
    let call_id = invoke.call_id.clone();
    let ext = Arc::new(ext);
    let sending = Arc::clone(&ext);
    let worker = std::thread::spawn(move || sending.handle_send(invoke));
    let HarnessInputMessage::Emit(report) = rx.recv().expect("sent report") else {
        panic!("expected sent report");
    };
    let Event::MessageSentReported(report) = *report.event else {
        panic!("expected sent report");
    };
    reached_rx.recv().expect("report boundary");
    let canonical = Event::MessageSentReported(report)
        .into_stamped_canonical_message_fact(
            tau_proto::MessagePublisherId::parse("std-slack").expect("publisher"),
        )
        .expect("canonical sent fact");
    ext.apply_live_event(&canonical);
    drop(rx);
    release_tx.send(()).expect("release failed result write");
    failure_rx.recv().expect("result failure boundary");
    {
        let state = ext.state.lock().expect("state");
        assert!(state.ingress.reactions.targets.is_empty());
        assert!(matches!(
            state
                .sends
                .send_ledger
                .get(&call_id)
                .map(|entry| &entry.disposition),
            Some(SendLedgerDisposition::Submitting {
                canonical_echoed: true,
                ..
            })
        ));
    }
    finish_failure_tx.send(()).expect("release retirement");
    assert!(worker.join().expect("send worker").is_none());
}

/// Production SendWake notification interrupts a long SystemSendScheduler wait
/// without polling or a test-side release signal.
#[test]
fn system_send_scheduler_is_woken_by_lifecycle_notification() {
    let wake = Arc::new(SendWake::default());
    let generation = wake.generation();
    let (started_tx, started_rx) = mpsc::channel();
    let (finished_tx, finished_rx) = mpsc::channel();
    let worker_wake = Arc::clone(&wake);
    let worker = std::thread::spawn(move || {
        started_tx.send(()).expect("announce wait");
        let cancelled = SystemSendScheduler.wait(&worker_wake, generation, Duration::from_secs(60));
        finished_tx.send(cancelled).expect("finish wait");
    });
    started_rx.recv().expect("wait started");
    wake.notify_lifecycle_change();
    assert!(
        finished_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("wait should wake")
    );
    worker.join().expect("scheduler worker");
}

/// Active delivery-worker backpressure rejects before freezing configuration or
/// reserving ledger/I/O state.
#[test]
fn active_send_worker_capacity_rejects_before_freeze_or_io() {
    let (ext, _rx, client) = extension();
    apply_test_config(&ext, proactive_cfg());
    {
        let mut state = ext.state.lock().expect("state");
        state.sends.active_send_workers = ACTIVE_SEND_WORKER_LIMIT;
        state.configuration.config_frozen = false;
    }
    let event = ext.handle_send(tool(
        SEND_TOOL_NAME,
        "agent-a",
        tau_proto::json_to_cbor(
            &serde_json::json!({"message":"backpressure","destination":"team-ops"}),
        ),
    ));
    assert!(matches!(
        event,
        Some(Event::ToolError(error)) if error.message.contains("workers are busy")
    ));
    let state = ext.state.lock().expect("state");
    assert!(!state.configuration.config_frozen);
    assert!(state.sends.send_ledger.is_empty());
    assert!(client.sent_pairs().is_empty());
}

/// Retry parsing clamps unsafe server hints and keeps route-keyed jitter
/// stable.
#[test]
fn retry_delay_and_jitter_are_bounded_and_deterministic() {
    assert_eq!(parse_retry_after(Some("0")), Duration::from_secs(1));
    assert_eq!(
        parse_retry_after(Some("999999")),
        send_delivery::MAX_RETRY_AFTER
    );
    assert_eq!(parse_retry_after(Some("secret")), Duration::from_secs(1));
    assert_eq!(
        send_delivery::retry_jitter("call-a", "C123"),
        send_delivery::retry_jitter("call-a", "C123")
    );
    assert_ne!(
        send_delivery::retry_jitter("call-a", "C123"),
        send_delivery::retry_jitter("call-a", "C456")
    );
}

/// Agent posts reject native Slack controls while bridge literals remain
/// escaped and bounded on the Slack wire.
#[test]
fn post_composition_rejects_native_controls_and_escapes_bridge_text() {
    assert!(matches!(
        SlackPostMode::agent("hello <@U123>".to_owned(), None),
        Err(PostCompositionError::NativeControlMarkup)
    ));
    let mention = InternalSourceMention::new("U123").expect("valid internal mention");
    let generated = SlackPostMode::agent("hello".to_owned(), Some(&mention))
        .expect("internal exact source mention");
    assert_eq!(generated.text(), "<@U123> hello");
    let semantic_reference = SlackPostMode::agent("@slack_bridge".to_owned(), None)
        .expect("semantic reference stays literal");
    assert_eq!(semantic_reference.text(), "@slack_bridge");
    let literal = SlackPostMode::bridge_literal("<@U123> & <!channel> <#C123>");
    assert_eq!(
        literal.text(),
        "&lt;@U123&gt; &amp; &lt;!channel&gt; &lt;#C123&gt;"
    );
    let body = FrozenPostBody::new("C123", None, &literal);
    let value: serde_json::Value =
        serde_json::from_str(body.wire_json()).expect("literal body JSON");
    assert_eq!(value["mrkdwn"], false);
    assert_eq!(value["link_names"], false);
    assert!(value["text"].as_str().expect("text").len() <= 8 * 1_024);
}

/// Typed send failures expose no caller input or Slack control data in
/// diagnostics.
#[test]
fn send_failure_diagnostics_do_not_reflect_private_input() {
    for sentinel in [
        "xoxb-secret",
        "<@U123>",
        "<!channel>",
        "<#C123>",
        "C123\npayload\u{202e}",
    ] {
        assert!(!SlackApiError::RemoteFailure.to_string().contains(sentinel));
        assert!(
            !SendFailureCategory::MalformedResponse
                .to_string()
                .contains(sentinel)
        );
    }
    let hostile = "xoxb-secret <@U123> <!channel> <#C123>\npayload\u{202e}";
    let event = tool_error(
        tool(
            SEND_TOOL_NAME,
            "agent-a",
            tau_proto::json_to_cbor(&serde_json::json!({"message": hostile})),
        ),
        SendFailureCategory::MalformedResponse.to_string(),
    );
    let encoded = serde_json::to_string(&event).expect("serialize closed tool error");
    assert!(!encoded.contains(hostile));
    let Event::ToolError(error) = event else {
        panic!("expected tool error");
    };
    assert!(error.details.is_none());
}

/// Exactly-one selector validation and the closed argument set prevent
/// reply/proactive confusion and native destination or thread injection.
#[test]
fn proactive_send_rejects_ambiguous_unknown_and_native_arguments() {
    let (ext, _rx, client) = extension();
    apply_test_config(&ext, proactive_cfg());
    for arguments in [
        serde_json::json!({"message": "x"}),
        serde_json::json!({"message": "x", "reply_to": "msg-x", "destination": "team-ops"}),
        serde_json::json!({"message": "x", "destination": "TEAM-OPS"}),
        serde_json::json!({"message": "x", "destination": " team-ops"}),
        serde_json::json!({"message": "x", "destination": "C456"}),
        serde_json::json!({"message": "x", "destination": "team-ops", "thread_ts": "1.0"}),
        serde_json::json!({"message": "x", "destination": "team-ops", "channel_id": "C999"}),
    ] {
        assert!(matches!(
            ext.handle_send(tool(
                SEND_TOOL_NAME,
                "agent-a",
                tau_proto::json_to_cbor(&arguments)
            )),
            Some(Event::ToolError(_))
        ));
    }
    assert!(client.sent_pairs().is_empty());
}

/// The send schema stays constant-sized and never embeds configured route data.
#[test]
fn proactive_schema_is_config_independent() {
    let spec = send_tool_spec();
    let parameters = spec.parameters.expect("parameters");
    let schema = parameters.as_object().expect("object schema");
    let destination = &schema["properties"]["destination"];
    assert!(destination.get("enum").is_none());
    assert_eq!(destination["pattern"], "^[a-z][a-z0-9_-]{0,63}$");
    let serialized = serde_json::to_string(&parameters).expect("schema json");
    for private in [
        "team-ops",
        "Trusted ops hint",
        DYNAMIC_DM_LABEL,
        "C456",
        "G789",
        "D123",
        "1720000000.123456",
    ] {
        assert!(!serialized.contains(private), "schema leaked {private}");
    }
}

/// Retry exhaustion must publish exactly one typed report, commit its ledger
/// disposition afterward, and never perform a late second post.
#[test]
fn retry_resuming_after_absolute_horizon_never_posts_again() {
    let client = ScriptedPostClient::new([PostAttemptOutcome::OutcomeUnknown(
        SendFailureCategory::Transport,
    )]);
    let (output_tx, output_rx) = mpsc::channel();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let ext = Extension::new_with_scheduler(
        client.clone(),
        output_tx,
        Arc::new(StepScheduler {
            entered: entered_tx,
            release: Mutex::new(release_rx),
        }),
    );
    apply_test_config(&ext, proactive_cfg());
    let worker_released = send_worker_release_notification(&ext);
    let invoke = tool_call(
        SEND_TOOL_NAME,
        "agent-a",
        "late-retry",
        tau_proto::json_to_cbor(&serde_json::json!({"message":"late","destination":"team-ops"})),
    );
    assert!(ext.handle_send(invoke.clone()).is_none());
    entered_rx.recv().expect("retry wait");
    ext.state
        .lock()
        .expect("state")
        .sends
        .send_ledger
        .get_mut(&invoke.call_id)
        .expect("ledger")
        .prepared
        .retry_deadline = Instant::now() - Duration::from_secs(1);
    release_tx.send(()).expect("resume late retry");
    assert!(matches!(
        output_rx.recv().expect("terminal error"),
        HarnessInputMessage::Emit(emit)
            if matches!(emit.event.as_ref(), Event::ToolErrorReported(_))
    ));
    wait_for_send_worker(&worker_released);
    assert!(
        output_rx.try_recv().is_err(),
        "retry exhaustion owns one terminal report"
    );
    assert!(matches!(
        ext.state
            .lock()
            .expect("state")
            .sends
            .send_ledger
            .get(&invoke.call_id)
            .map(|entry| &entry.disposition),
        Some(SendLedgerDisposition::ExhaustedUnknown { .. })
    ));
    assert_eq!(client.bodies.lock().expect("bodies").len(), 1);
}

/// A definitive asynchronous provider failure must publish one typed error
/// report and only then commit the send ledger's terminal disposition.
#[test]
fn definitive_send_failure_publishes_one_report_before_terminalizing_ledger() {
    let client = ScriptedPostClient::new([PostAttemptOutcome::DefinitiveFailure(
        SendFailureCategory::PermissionDenied,
    )]);
    let (output_tx, output_rx) = mpsc::channel();
    let ext = Extension::new(client.clone(), output_tx);
    apply_test_config(&ext, proactive_cfg());
    let worker_released = send_worker_release_notification(&ext);
    let invoke = tool_call(
        SEND_TOOL_NAME,
        "agent-a",
        "definitive-failure",
        tau_proto::json_to_cbor(&serde_json::json!({"message":"denied","destination":"team-ops"})),
    );

    assert!(ext.handle_send(invoke.clone()).is_none());
    assert!(matches!(
        output_rx.recv().expect("typed error report"),
        HarnessInputMessage::Emit(emit)
            if !emit.persist
                && matches!(emit.event.as_ref(), Event::ToolErrorReported(error)
                    if error.call_id == invoke.call_id)
    ));
    wait_for_send_worker(&worker_released);
    assert!(
        output_rx.try_recv().is_err(),
        "one send owns one terminal report"
    );
    assert!(matches!(
        ext.state
            .lock()
            .expect("state")
            .sends
            .send_ledger
            .get(&invoke.call_id)
            .map(|entry| &entry.disposition),
        Some(SendLedgerDisposition::DefinitiveFailure { .. })
    ));
}

/// Lifecycle cancellation during a retry wait must publish one typed error
/// report rather than a peer-authored canonical error.
#[test]
fn lifecycle_cancelled_send_publishes_one_typed_error_report() {
    let client = ScriptedPostClient::new([PostAttemptOutcome::OutcomeUnknown(
        SendFailureCategory::Transport,
    )]);
    let (output_tx, output_rx) = mpsc::channel();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let ext = Extension::new_with_scheduler(
        client,
        output_tx,
        Arc::new(StepScheduler {
            entered: entered_tx,
            release: Mutex::new(release_rx),
        }),
    );
    apply_test_config(&ext, proactive_cfg());
    let worker_released = send_worker_release_notification(&ext);
    let invoke = tool_call(
        SEND_TOOL_NAME,
        "agent-a",
        "cancelled-send",
        tau_proto::json_to_cbor(
            &serde_json::json!({"message":"cancel me","destination":"team-ops"}),
        ),
    );

    assert!(ext.handle_send(invoke.clone()).is_none());
    entered_rx.recv().expect("retry wait");
    ext.unload_agent(&invoke.agent_id);
    release_tx.send(()).expect("release cancelled retry");
    assert!(matches!(
        output_rx.recv().expect("typed cancellation error report"),
        HarnessInputMessage::Emit(emit)
            if !emit.persist
                && matches!(emit.event.as_ref(), Event::ToolErrorReported(error)
                    if error.call_id == invoke.call_id)
    ));
    wait_for_send_worker(&worker_released);
    assert!(output_rx.try_recv().is_err(), "cancellation settles once");
    assert!(matches!(
        ext.state
            .lock()
            .expect("state")
            .sends
            .send_ledger
            .get(&invoke.call_id)
            .map(|entry| &entry.disposition),
        Some(SendLedgerDisposition::Cancelled { .. })
    ));
}

/// Revocation before the ordered terminal gate must reclassify a stale
/// provider failure as one lifecycle cancellation.
#[test]
fn lifecycle_revocation_before_terminal_gate_reclassifies_failure() {
    let client = ScriptedPostClient::new([PostAttemptOutcome::DefinitiveFailure(
        SendFailureCategory::PermissionDenied,
    )]);
    let (output_tx, output_rx) = mpsc::channel();
    let ext = Extension::new(client, output_tx);
    apply_test_config(&ext, proactive_cfg());
    let worker_released = send_worker_release_notification(&ext);
    let (reached_tx, reached_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    *ext.test_hooks
        .send_error_submission_boundary
        .lock()
        .expect("hook") = Some(BlockingTestHook {
        reached: reached_tx,
        release: release_rx,
    });
    let invoke = tool_call(
        SEND_TOOL_NAME,
        "agent-a",
        "reclassified-failure",
        tau_proto::json_to_cbor(&serde_json::json!({"message":"denied","destination":"team-ops"})),
    );

    assert!(ext.handle_send(invoke.clone()).is_none());
    reached_rx.recv().expect("terminal gate boundary");
    ext.unload_agent(&invoke.agent_id);
    release_tx.send(()).expect("release terminal publication");
    assert!(matches!(
        output_rx.recv().expect("typed cancellation"),
        HarnessInputMessage::Emit(emit)
            if matches!(emit.event.as_ref(), Event::ToolErrorReported(error)
                if error.call_id == invoke.call_id
                    && error.message.contains("cancelled"))
    ));
    wait_for_send_worker(&worker_released);
    assert!(output_rx.try_recv().is_err());
    assert!(matches!(
        ext.state
            .lock()
            .expect("state")
            .sends
            .send_ledger
            .get(&invoke.call_id)
            .map(|entry| &entry.disposition),
        Some(SendLedgerDisposition::Cancelled { .. })
    ));
}

/// Failed mandatory output must leave the live send ledger nonterminal until
/// fail-closed retirement ends the extension session.
#[test]
fn failed_send_terminal_preserves_ledger_ownership_until_retirement() {
    let client = ScriptedPostClient::new([PostAttemptOutcome::DefinitiveFailure(
        SendFailureCategory::PermissionDenied,
    )]);
    let (output_tx, output_rx) = mpsc::channel();
    drop(output_rx);
    let ext = Extension::new(client, output_tx);
    apply_test_config(&ext, proactive_cfg());
    let (failed_tx, failed_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (retired_tx, retired_rx) = mpsc::channel();
    *ext.test_hooks.output_failure_boundary.lock().expect("hook") = Some(BlockingTestHook {
        reached: failed_tx,
        release: release_rx,
    });
    let invoke = tool_call(
        SEND_TOOL_NAME,
        "agent-a",
        "failed-terminal-ownership",
        tau_proto::json_to_cbor(&serde_json::json!({"message":"denied","destination":"team-ops"})),
    );

    assert!(ext.handle_send(invoke.clone()).is_none());
    failed_rx.recv().expect("mandatory output failure");
    assert!(matches!(
        ext.state
            .lock()
            .expect("state")
            .sends
            .send_ledger
            .get(&invoke.call_id)
            .map(|entry| &entry.disposition),
        Some(SendLedgerDisposition::InFlight { .. })
    ));
    assert!(!ext.shutdown.is_requested());
    release_tx.send(()).expect("release retirement");
    let watching_shutdown = Arc::clone(&ext.shutdown);
    std::thread::spawn(move || {
        path_tokio_runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(watching_shutdown.wait());
        retired_tx.send(()).expect("announce retirement");
    });
    retired_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("fatal retirement deadline");
    assert!(ext.output_failed.load(Ordering::Acquire));
}

/// An asynchronous mandatory-output failure must wake and terminate the
/// production protocol loop while harness input remains open.
#[test]
fn asynchronous_terminal_failure_stops_production_protocol_loop() {
    let client = ScriptedPostClient::new([PostAttemptOutcome::DefinitiveFailure(
        SendFailureCategory::PermissionDenied,
    )]);
    let mut input = Vec::new();
    let mut input_writer = tau_proto::HarnessOutputWriter::new(&mut input);
    input_writer
        .write_message(&proactive_config_message())
        .expect("write config");
    input_writer
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(
            tool_call(
                SEND_TOOL_NAME,
                "agent-a",
                "async-output-failure",
                tau_proto::json_to_cbor(
                    &serde_json::json!({"message":"denied","destination":"team-ops"}),
                ),
            ),
        )))
        .expect("write send invocation");
    input_writer.flush().expect("flush fixture input");
    let gate = Arc::new((Mutex::new(true), Condvar::new()));
    let reader = BlockingAfterInputReader {
        input: path_std_io::Cursor::new(input),
        gate: Arc::clone(&gate),
    };
    let (finished_tx, finished_rx) = mpsc::channel();
    let runner = std::thread::spawn(move || {
        let result = run_with_client(reader, AsyncErrorReportFailureWriter, client);
        finished_tx
            .send(result.is_err())
            .expect("report runner exit");
    });

    assert!(
        finished_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("fatal output must stop the production loop"),
        "writer failure must surface from the runner"
    );
    let (lock, condvar) = &*gate;
    *lock.lock().expect("reader gate") = false;
    condvar.notify_all();
    runner.join().expect("runner thread");
}

#[test]
fn deletion_writer_failure_retires_a_queued_send_retry() {
    let client = ScriptedPostClient::new([
        PostAttemptOutcome::OutcomeUnknown(SendFailureCategory::Transport),
        PostAttemptOutcome::Accepted(PostedMessage {
            channel_id: "C456".to_owned(),
            ts: "2.0".to_owned(),
            thread_ts: None,
        }),
    ]);
    let (output_tx, output_rx) = mpsc::channel();
    drop(output_rx);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let ext = Extension::new_with_scheduler(
        client.clone(),
        output_tx,
        Arc::new(StepScheduler {
            entered: entered_tx,
            release: Mutex::new(release_rx),
        }),
    );
    apply_test_config(&ext, proactive_cfg());
    let invoke = tool_call(
        SEND_TOOL_NAME,
        "agent-a",
        "queued-before-fatal-output",
        tau_proto::json_to_cbor(
            &serde_json::json!({"message":"must not retry","destination":"team-ops"}),
        ),
    );
    assert!(ext.handle_send(invoke).is_none());
    entered_rx.recv().expect("retry wait");
    ext.remember_posted_message(
        SlackConversation {
            alias: "team-ops".to_owned(),
            channel_id: "C456".to_owned(),
            kind: ConversationPolicyKind::Channel,
            thread_ts: None,
        },
        PostedMessage {
            channel_id: "C456".to_owned(),
            ts: "9.0".to_owned(),
            thread_ts: None,
        },
        agent_id("agent-a"),
    );
    ext.process_slack_delete(SlackDelete {
        event_id: Some("fatal-delete-with-retry".to_owned()),
        channel_id: "C456".to_owned(),
        message_ts: "9.0".to_owned(),
        thread_ts: None,
    });
    release_tx.send(()).expect("release retired retry");
    let mut worker_exited = false;
    for _ in 0..100 {
        if ext.state.lock().expect("state").sends.active_send_workers == 0 {
            worker_exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(worker_exited, "retired retry worker did not terminate");
    assert_eq!(client.bodies.lock().expect("bodies").len(), 1);
    assert!(
        ext.state
            .lock()
            .expect("state")
            .sends
            .send_ledger
            .is_empty()
    );
}

#[test]
fn disconnect_and_eof_retire_before_a_reserved_initial_attempt() {
    for boundary in ["disconnect", "eof"] {
        let client = ScriptedPostClient::new([]);
        let (output_tx, _output_rx) = mpsc::channel();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let ext = Extension::new_with_scheduler(
            client.clone(),
            output_tx,
            Arc::new(StepScheduler {
                entered: entered_tx,
                release: Mutex::new(release_rx),
            }),
        );
        apply_test_config(&ext, proactive_cfg());
        ext.state
            .lock()
            .expect("state")
            .sends
            .channel_attempt_deadlines
            .insert("C456".to_owned(), Instant::now() + Duration::from_secs(10));
        assert!(
            ext.handle_send(tool_call(
                SEND_TOOL_NAME,
                "agent-a",
                &format!("{boundary}-call"),
                tau_proto::json_to_cbor(
                    &serde_json::json!({"message":"stop","destination":"team-ops"}),
                ),
            ))
            .is_none()
        );
        entered_rx.recv().expect("initial wait");
        if boundary == "disconnect" {
            apply_output_message(
                &HarnessOutputMessage::Disconnect(tau_proto::Disconnect {
                    reason: Some("test".to_owned()),
                }),
                &ext,
            );
        } else {
            let reader_boundary = SendReaderBoundary::default();
            reader_boundary.install(&ext);
            let mut reader = RetiringReader {
                inner: path_std_io::Cursor::new(Vec::<u8>::new()),
                boundary: Arc::new(reader_boundary),
            };
            let mut byte = [0_u8; 1];
            assert_eq!(reader.read(&mut byte).expect("EOF"), 0);
        }
        let released_generation = ext.send_wake.generation();
        release_tx.send(()).expect("release stale wait");
        assert!(
            ext.send_wake
                .wait(released_generation, Duration::from_secs(1)),
            "worker did not release promptly after {boundary}"
        );
        assert!(client.bodies.lock().expect("bodies").is_empty());
        assert_eq!(
            ext.state.lock().expect("state").sends.active_send_workers,
            0
        );
    }
}
