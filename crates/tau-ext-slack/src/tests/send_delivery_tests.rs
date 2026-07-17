//! Slack send-delivery state machine, retry, replay, and concurrency tests.

use super::*;
use crate::send_delivery::{InternalSourceMention, PostCompositionError};

/// Multi-step scheduler exposing every logical wait to a deterministic test.
struct StepScheduler {
    entered: mpsc::Sender<Duration>,
    release: Mutex<mpsc::Receiver<()>>,
}

impl SendScheduler for StepScheduler {
    fn wait(&self, wake: &SendWake, generation: u64, delay: Duration) -> bool {
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

/// The send tool schema and runtime validation must not allow model-selected
/// Slack destinations such as channel ids.
#[test]
fn slack_send_rejects_destination_arguments() {
    let (ext, _rx, _client) = extension();
    register_agent(&ext, "agent-a");
    let event = ext.handle_send(tool(
        SEND_TOOL_NAME,
        "agent-a",
        CborValue::Map(vec![
            (
                CborValue::Text("message".to_owned()),
                CborValue::Text("hi".to_owned()),
            ),
            (
                CborValue::Text("channel_id".to_owned()),
                CborValue::Text("C999".to_owned()),
            ),
        ]),
    ));
    let Some(Event::ToolError(err)) = event else {
        panic!("expected error");
    };
    assert!(err.message.contains("unknown argument `channel_id`"));
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
        state.bot_user_id = None;
        state.installation_team_id = None;
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
    assert!(!ext.state.lock().expect("state").config_frozen);
}

/// Proactive calls publish the sent fact before the correlated
/// ordinary tool result without a special harness messaging handshake.
#[test]
fn proactive_send_publishes_fact_before_tool_result() {
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
    let HarnessInputMessage::Emit(emit) = rx.recv().expect("sent fact") else {
        panic!("expected sent fact");
    };
    let Event::MessageSent(fact) = *emit.event else {
        panic!("expected sent fact");
    };
    assert_eq!(fact.text, "update");
    assert_eq!(fact.publisher_extension_id.as_str(), "std-slack");
    assert!(matches!(
        rx.recv().expect("tool result"),
        HarnessInputMessage::Emit(emit)
            if matches!(emit.event.as_ref(), Event::ToolResult(_))
    ));
    assert_eq!(client.sent_pairs().len(), 1);
}

/// Replaying a completed send returns its stable ordinary tool result without
/// posting or publishing another sent fact.
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
    let _fact = rx.recv().expect("sent fact");
    let HarnessInputMessage::Emit(first) = rx.recv().expect("first result") else {
        panic!("expected result emission");
    };
    let Some(replay) = ext.handle_send(invoke.clone()) else {
        panic!("completed replay must return a result");
    };
    assert_eq!(format!("{:?}", first.event), format!("{replay:?}"));
    assert!(rx.try_recv().is_err());
    assert_eq!(client.sent_pairs().len(), 1);
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
            .recv_timeout(Duration::from_millis(200))
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
        state.active_send_workers = ACTIVE_SEND_WORKER_LIMIT;
        state.config_frozen = false;
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
    assert!(!state.config_frozen);
    assert!(state.send_ledger.is_empty());
    assert!(client.sent_pairs().is_empty());
}

/// Retry parsing, jitter, diagnostics, and post composition stay deterministic,
/// bounded, and free of reflected Slack controls.
#[test]
fn typed_post_boundary_is_bounded_private_and_literal_safe() {
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
        .send_ledger
        .get_mut(&invoke.call_id)
        .expect("ledger")
        .prepared
        .retry_deadline = Instant::now() - Duration::from_secs(1);
    release_tx.send(()).expect("resume late retry");
    assert!(matches!(
        output_rx.recv().expect("terminal error"),
        HarnessInputMessage::Emit(emit) if matches!(emit.event.as_ref(), Event::ToolError(_))
    ));
    assert_eq!(client.bodies.lock().expect("bodies").len(), 1);
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
        if ext.state.lock().expect("state").active_send_workers == 0 {
            worker_exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(worker_exited, "retired retry worker did not terminate");
    assert_eq!(client.bodies.lock().expect("bodies").len(), 1);
    assert!(ext.state.lock().expect("state").send_ledger.is_empty());
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
                inner: std::io::Cursor::new(Vec::<u8>::new()),
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
        assert_eq!(ext.state.lock().expect("state").active_send_workers, 0);
    }
}
