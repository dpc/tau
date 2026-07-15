//! Slack send-delivery state machine, retry, replay, and concurrency tests.

use super::*;
use crate::send_delivery::{InternalSourceMention, PostCompositionError};

#[derive(Default)]
struct HoldingCompletionSpawner {
    tasks: Mutex<VecDeque<Box<dyn FnOnce() + Send>>>,
}

impl CompletionOutputSpawner for HoldingCompletionSpawner {
    fn spawn(&self, task: Box<dyn FnOnce() + Send>) -> std::io::Result<()> {
        self.tasks.lock().expect("tasks").push_back(task);
        Ok(())
    }
}

struct FailingCompletionSpawner;

impl CompletionOutputSpawner for FailingCompletionSpawner {
    fn spawn(&self, _task: Box<dyn FnOnce() + Send>) -> std::io::Result<()> {
        Err(std::io::Error::other("injected completion spawn failure"))
    }
}

/// First provider attempt blocks across retirement; later attempts complete.
struct StaleFirstPostClient {
    first_started: Mutex<Option<mpsc::Sender<()>>>,
    first_release: Mutex<mpsc::Receiver<()>>,
    attempts: Mutex<usize>,
}

impl SlackClient for StaleFirstPostClient {
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
        let attempt = {
            let mut attempts = self.attempts.lock().expect("attempts");
            *attempts += 1;
            *attempts
        };
        if attempt == 1 {
            if let Some(started) = self.first_started.lock().expect("started").take() {
                started.send(()).expect("announce stale attempt");
            }
            self.first_release
                .lock()
                .expect("release")
                .recv()
                .expect("release stale attempt");
        }
        PostAttemptOutcome::Accepted(PostedMessage {
            channel_id: body.channel_id().to_owned(),
            ts: format!("{attempt}.0"),
            thread_ts: body.thread_ts().map(str::to_owned),
        })
    }
}

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

/// Client that exposes provider-attempt order and blocks each attempt.
struct OrderedBlockingPostClient {
    started: mpsc::Sender<String>,
    release: Mutex<mpsc::Receiver<()>>,
    attempts: Mutex<usize>,
}

impl SlackClient for OrderedBlockingPostClient {
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
        let text = serde_json::from_str::<serde_json::Value>(body.wire_json())
            .expect("body")
            .get("text")
            .and_then(serde_json::Value::as_str)
            .expect("text")
            .to_owned();
        *self.attempts.lock().expect("attempts") += 1;
        self.started.send(text).expect("announce provider attempt");
        self.release
            .lock()
            .expect("release")
            .recv()
            .expect("release provider attempt");
        PostAttemptOutcome::Accepted(PostedMessage {
            channel_id: body.channel_id().to_owned(),
            ts: format!("{}.0", *self.attempts.lock().expect("attempts")),
            thread_ts: body.thread_ts().map(str::to_owned),
        })
    }
}

/// Fake whose post call blocks until the test releases a lifecycle race.
struct BlockingSendClient {
    /// Signals that Slack I/O has begun.
    started: Mutex<Option<mpsc::Sender<()>>>,
    /// Release signal consumed by the blocked call.
    release: Mutex<mpsc::Receiver<()>>,
}

/// Fake that blocks before returning one typed post outcome.
struct BlockingPostOutcomeClient {
    /// Signals that the provider attempt began.
    started: Mutex<Option<mpsc::Sender<()>>>,
    /// Releases the provider result.
    release: Mutex<mpsc::Receiver<()>>,
    /// Typed result returned after release.
    outcome: PostAttemptOutcome<PostedMessage>,
    /// Number of observed attempts.
    attempts: Mutex<usize>,
}

impl SlackClient for BlockingSendClient {
    fn open_socket(&self, _cfg: &RuntimeConfig) -> Result<String, SlackApiError> {
        unreachable!("not used")
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
        unreachable!("not used")
    }

    fn post_message(
        &self,
        _cfg: &RuntimeConfig,
        body: &FrozenPostBody,
    ) -> PostAttemptOutcome<PostedMessage> {
        if let Some(started) = self.started.lock().expect("started lock").take() {
            started.send(()).expect("signal started");
        }
        self.release
            .lock()
            .expect("release lock")
            .recv()
            .expect("release send");
        PostAttemptOutcome::Accepted(PostedMessage {
            channel_id: body.channel_id().to_owned(),
            ts: "1.0".to_owned(),
            thread_ts: body.thread_ts().map(str::to_owned),
        })
    }

    fn react(
        &self,
        _cfg: &RuntimeConfig,
        _action: ReactionActionKind,
        _channel_id: &str,
        _message_ts: &str,
        _emoji: &str,
    ) -> Result<(), ReactionApiError> {
        unreachable!("not used")
    }
}

impl SlackClient for BlockingPostOutcomeClient {
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
        _body: &FrozenPostBody,
    ) -> PostAttemptOutcome<PostedMessage> {
        *self.attempts.lock().expect("attempts") += 1;
        if let Some(started) = self.started.lock().expect("started").take() {
            started.send(()).expect("signal started");
        }
        self.release
            .lock()
            .expect("release")
            .recv()
            .expect("release outcome");
        self.outcome.clone()
    }
}

/// Scripted post client used to verify that Slack-accepted response metadata is
/// checked once and then retained as an idempotent failure.
struct MismatchedPostClient {
    /// Number of Slack post attempts.
    posts: Mutex<usize>,
    /// Returned conversation id.
    returned_channel: String,
    /// Returned thread root.
    returned_thread: Option<String>,
}

impl SlackClient for MismatchedPostClient {
    fn open_socket(&self, _cfg: &RuntimeConfig) -> Result<String, SlackApiError> {
        unreachable!("post-only test client")
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
        unreachable!("post-only test client")
    }

    fn post_message(
        &self,
        _cfg: &RuntimeConfig,
        _body: &FrozenPostBody,
    ) -> PostAttemptOutcome<PostedMessage> {
        *self.posts.lock().expect("posts") += 1;
        PostAttemptOutcome::Accepted(PostedMessage {
            channel_id: self.returned_channel.clone(),
            ts: "1.0".to_owned(),
            thread_ts: self.returned_thread.clone(),
        })
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

/// Immediate scheduler that records exact computed delays without sleeping.
#[derive(Default)]
struct RecordingScheduler {
    /// Ordered retry delays.
    delays: Mutex<Vec<Duration>>,
}

impl SendScheduler for RecordingScheduler {
    fn wait(&self, _wake: &SendWake, _generation: u64, delay: Duration) -> bool {
        self.delays.lock().expect("delays").push(delay);
        false
    }
}

/// Deterministic barrier scheduler used to hold one retry wait.
struct BlockingScheduler {
    /// Announces entry into the retry wait.
    entered: Mutex<Option<mpsc::Sender<Duration>>>,
    /// Releases the retry wait.
    release: Mutex<mpsc::Receiver<()>>,
}

impl SendScheduler for BlockingScheduler {
    fn wait(&self, wake: &SendWake, generation: u64, delay: Duration) -> bool {
        if let Some(entered) = self.entered.lock().expect("entered").take() {
            entered.send(delay).expect("announce retry wait");
        }
        self.release
            .lock()
            .expect("release")
            .recv()
            .expect("release retry wait");
        wake.generation() != generation
    }
}

/// A Slack post returning after shutdown cannot recreate a send attempt,
/// pending completion, posted-message owner, or reaction target.
#[test]
fn late_send_success_after_shutdown_cannot_restore_state() {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let client = Arc::new(BlockingSendClient {
        started: Mutex::new(Some(started_tx)),
        release: Mutex::new(release_rx),
    });
    let (output, output_rx) = mpsc::channel();
    let ext = Arc::new(Extension::new(client, output));
    apply_test_config(&ext, proactive_cfg());
    let worker = {
        let ext = ext.clone();
        std::thread::spawn(move || {
            ext.handle_send(tool_call(
                SEND_TOOL_NAME,
                "agent-a",
                "late-send",
                tau_proto::json_to_cbor(&serde_json::json!({
                    "message": "late",
                    "destination": "team-ops"
                })),
            ))
        })
    };
    started_rx.recv().expect("send started");
    {
        let mut state = ext.state.lock().expect("state");
        state.capability_active = false;
        state.pending_posts.clear();
        state.clear_send_ledger();
        state.clear_reaction_state();
    }
    ext.send_wake.notify_lifecycle_change();
    release_tx.send(()).expect("release send");
    assert!(worker.join().expect("worker").is_none());
    assert!(
        output_rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "retired session must receive no stale completion or tool error"
    );
    let state = ext.state.lock().expect("state");
    assert!(state.pending_posts.is_empty());
    assert!(state.send_ledger.is_empty());
    assert!(state.reaction_targets.is_empty());
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

/// Proactive aliases work without agent registration, preserve a configured
/// fixed thread, and expose no native destination in the model arguments.
#[test]
fn proactive_send_uses_exact_configured_route_without_registration() {
    let (ext, rx, client) = extension();
    apply_test_config(&ext, proactive_cfg());
    let event = ext.handle_send(tool(
        SEND_TOOL_NAME,
        "agent-a",
        tau_proto::json_to_cbor(&serde_json::json!({
            "message": "update",
            "destination": "incident-thread"
        })),
    ));
    assert!(event.is_none(), "completion is asynchronous");
    let completion = rx.recv().expect("background completion");
    assert_eq!(
        client.sent_pairs(),
        vec![("G789".to_owned(), "update".to_owned())]
    );
    assert_eq!(
        client.sent_thread_ids(),
        vec![Some("1720000000.123456".to_owned())]
    );
    let request = match completion {
        HarnessInputMessage::CompleteTransportSend(request) => request,
        other => panic!("unexpected frame: {other:?}"),
    };
    assert!(request.in_reply_to.is_none());
    assert!(matches!(
        request.authorization,
        tau_proto::TransportSendAuthorization::ConfiguredDestination { ref alias }
            if alias == "incident-thread"
    ));
    let message_ref = tau_proto::cbor_text_field(&request.tool_result.result, "message_ref")
        .expect("structured send message_ref")
        .to_owned();
    assert!(message_ref.starts_with("slack-msg-v1-"));
    assert!(!message_ref.contains("G789"));
    assert!(!message_ref.contains("1720000000"));
    assert!(
        !ext.state
            .lock()
            .expect("state")
            .reaction_targets
            .contains_key(&message_ref),
        "returned ref must fail closed before completion"
    );
    apply_output_message(
        &HarnessOutputMessage::CompleteTransportSendResult(
            tau_proto::CompleteTransportSendResult {
                request_id: request.request_id,
                message_id: Some(MessageId::new("msg-outgoing")),
                accepted: true,
                error: None,
            },
        ),
        &ext,
    );
    assert!(
        ext.state
            .lock()
            .expect("state")
            .reaction_targets
            .contains_key(&message_ref)
    );
}

/// Enabling `prefix_agent_id` retains the legacy prefix for both source replies
/// and proactive sends while leaving byte-limit validation on the model text.
#[test]
fn enabled_agent_id_prefix_formats_reply_and_proactive_text() {
    let (ext, rx, client) = extension();
    let message = "first line\nsecond 🦀";
    let mut secrets = BTreeMap::new();
    secrets.insert("app".to_owned(), tau_proto::SecretValue::new("xapp-test"));
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("xoxb-test"));
    let config = tau_proto::json_to_cbor(&serde_json::json!({
        "app_token_secret": "app",
        "bot_token_secret": "bot",
        "allowed_user_ids": ["U123"],
        "prefix_agent_id": true,
        "max_message_bytes": message.len(),
        "conversations": [{
            "alias": "team",
            "conversation_id": "C123",
            "kind": "channel",
            "receive": "mentions_only"
        }, {
            "alias": "team-ops",
            "conversation_id": "C456",
            "kind": "channel",
            "proactive_send": true
        }]
    }))
    .deserialized::<ExtConfig>()
    .expect("deserialize operator config")
    .validate(&secrets)
    .expect("validate operator config");
    apply_test_config(&ext, config);
    register_agent(&ext, "agent-a");
    ext.process_slack_message(slack_message("C123", None, "<@UBOT123> source"));
    let prompt = recv_prompt_request(&rx);
    activate_prompt_origin(&ext, &prompt);

    assert!(
        ext.handle_send(tool(SEND_TOOL_NAME, "agent-a", message_args(message)))
            .is_none()
    );
    rx.recv().expect("reply completion");
    let mut proactive = tool(
        SEND_TOOL_NAME,
        "agent-a",
        tau_proto::json_to_cbor(
            &serde_json::json!({"message": message, "destination": "team-ops"}),
        ),
    );
    proactive.call_id = "call-prefix-proactive".into();
    assert!(ext.handle_send(proactive).is_none());
    rx.recv().expect("proactive completion");

    let expected = format!("[agent-a] {message}");
    assert!(expected.len() > message.len());
    assert_eq!(
        client.sent_pairs(),
        [
            ("C123".to_owned(), expected.clone()),
            ("C456".to_owned(), expected)
        ]
    );
}

/// A proactive send resolves a known alias from the latest pre-freeze
/// configuration rather than retaining the route from an earlier configuration.
#[test]
fn proactive_send_resolves_current_alias_after_reconfiguration() {
    let (ext, rx, client) = extension();
    ext.apply_config(proactive_cfg()).expect("initial config");
    let mut current = proactive_cfg();
    current
        .conversations
        .get_mut("team-ops")
        .expect("team-ops")
        .conversation_id = "C999".to_owned();
    ext.apply_config(current)
        .expect("pre-freeze reconfiguration");
    ext.state.lock().expect("state").capability_active = true;

    assert!(
        ext.handle_send(tool(
            SEND_TOOL_NAME,
            "agent-a",
            tau_proto::json_to_cbor(
                &serde_json::json!({"message":"current","destination":"team-ops"})
            ),
        ))
        .is_none()
    );
    rx.recv().expect("background completion");
    assert_eq!(
        client.sent_pairs(),
        vec![("C999".to_owned(), "current".to_owned())]
    );
}

/// Proactive calls must not reach Slack until the harness accepts the current
/// session's exact transport capability.
#[test]
fn proactive_send_requires_active_capability_before_posting() {
    let (ext, _rx, client) = extension();
    ext.apply_config(proactive_cfg())
        .expect("configure aliases");
    let event = ext.handle_send(tool(
        SEND_TOOL_NAME,
        "agent-a",
        tau_proto::json_to_cbor(
            &serde_json::json!({"message": "update", "destination": "team-ops"}),
        ),
    ));
    assert!(matches!(event, Some(Event::ToolError(_))));
    assert!(client.sent_pairs().is_empty());
}

/// A fully authorized proactive attempt freezes configuration immediately
/// before Slack I/O even when the API result is an ambiguous failure.
#[test]
fn proactive_api_failure_still_freezes_configuration() {
    let (tx, rx) = mpsc::channel();
    let ext = Extension::new(Arc::new(FailingPostClient), tx);
    ext.apply_config(proactive_cfg()).expect("config");
    {
        let mut state = ext.state.lock().expect("state");
        state.capability_active = true;
        state.bot_user_id = Some("UBOT123".to_owned());
        state.installation_team_id = Some("T123".to_owned());
    }
    let result = ext.handle_send(tool(
        SEND_TOOL_NAME,
        "agent-a",
        tau_proto::json_to_cbor(&serde_json::json!({"message":"update","destination":"team-ops"})),
    ));
    assert!(result.is_none());
    assert!(matches!(
        rx.recv().expect("background failure"),
        HarnessInputMessage::Emit(emit) if matches!(emit.event.as_ref(), Event::ToolError(_))
    ));
    assert!(ext.state.lock().expect("state").config_frozen);
    assert!(ext.apply_config(proactive_cfg()).is_err());
}

/// Restart-required Configure after an accepted proactive post preserves the
/// pending completion, capability, late correlation, and no-repost disposition.
#[test]
fn frozen_proactive_pending_state_survives_reconfigure_and_late_completion() {
    let (ext, rx, client) = extension();
    apply_test_config(&ext, proactive_cfg());
    let invoke = tool(
        SEND_TOOL_NAME,
        "agent-a",
        tau_proto::json_to_cbor(&serde_json::json!({"message":"update","destination":"team-ops"})),
    );
    assert!(ext.handle_send(invoke.clone()).is_none());
    let first_completion = rx.recv().expect("background completion");
    assert!(ext.apply_config(cfg()).is_err());
    {
        let state = ext.state.lock().expect("state");
        assert!(state.capability_active);
        assert_eq!(state.pending_posts.len(), 1);
        assert_eq!(state.send_ledger.len(), 1);
        assert!(
            state
                .config
                .as_ref()
                .expect("config")
                .proactive_aliases
                .contains("team-ops")
        );
    }
    let HarnessInputMessage::CompleteTransportSend(first_completion) = first_completion else {
        panic!("expected completion");
    };
    apply_output_message(
        &HarnessOutputMessage::CompleteTransportSendResult(
            tau_proto::CompleteTransportSendResult {
                request_id: first_completion.request_id,
                message_id: Some(MessageId::new("msg-outgoing")),
                accepted: true,
                error: None,
            },
        ),
        &ext,
    );
    assert!(ext.state.lock().expect("state").pending_posts.is_empty());
    assert!(matches!(
        ext.handle_send(invoke),
        Some(Event::ToolResult(_))
    ));
    assert!(rx.try_recv().is_err());
    assert_eq!(client.sent_pairs().len(), 1);
}

/// Identical delivery awaiting Tau resubmits the typed completion without
/// posting again or fabricating a terminal tool result.
#[test]
fn accepted_proactive_replay_resubmits_completion_without_reposting() {
    let (ext, rx, client) = extension();
    apply_test_config(&ext, proactive_cfg());
    let invoke = tool(
        SEND_TOOL_NAME,
        "agent-a",
        tau_proto::json_to_cbor(
            &serde_json::json!({"message": "update", "destination": "team-ops"}),
        ),
    );
    let generation = ext.send_wake.generation();
    assert!(ext.handle_send(invoke.clone()).is_none());
    let first = rx.recv().expect("first completion");
    assert!(ext.send_wake.wait(generation, Duration::from_secs(1)));
    assert!(ext.handle_send(invoke.clone()).is_none());
    let replay = rx.recv().expect("replayed completion");
    assert_eq!(format!("{first:?}"), format!("{replay:?}"));
    assert_eq!(client.sent_pairs().len(), 1);
}

/// Tau acceptance is the durable terminal fact even when private reaction
/// authority became stale before the correlated result arrived.
#[test]
fn accepted_completion_replays_completed_result_after_local_authority_change() {
    let (ext, rx, client) = extension();
    apply_test_config(&ext, proactive_cfg());
    let invoke = tool_call(
        SEND_TOOL_NAME,
        "agent-a",
        "accepted-stale-local",
        tau_proto::json_to_cbor(&serde_json::json!({"message":"update","destination":"team-ops"})),
    );
    assert!(ext.handle_send(invoke.clone()).is_none());
    let HarnessInputMessage::CompleteTransportSend(completion) =
        rx.recv().expect("background completion")
    else {
        panic!("expected completion");
    };
    ext.unload_agent(&invoke.agent_id);
    assert_eq!(ext.state.lock().expect("state").pending_posts.len(), 1);
    apply_output_message(
        &HarnessOutputMessage::CompleteTransportSendResult(
            tau_proto::CompleteTransportSendResult {
                request_id: completion.request_id,
                message_id: Some(MessageId::new("accepted")),
                accepted: true,
                error: None,
            },
        ),
        &ext,
    );
    assert!(matches!(
        ext.state
            .lock()
            .expect("state")
            .send_ledger
            .get(&invoke.call_id)
            .map(|entry| &entry.disposition),
        Some(SendLedgerDisposition::Completed {
            result: _,
            copies: RemoteCopyPossibility::One
        })
    ));
    assert!(matches!(
        ext.handle_send(invoke),
        Some(Event::ToolResult(_))
    ));
    assert_eq!(client.sent_pairs().len(), 1);
}

#[test]
fn rejected_completion_after_agent_unload_replays_stable_rejection() {
    let (ext, rx, client) = extension();
    apply_test_config(&ext, proactive_cfg());
    let invoke = tool_call(
        SEND_TOOL_NAME,
        "agent-a",
        "rejected-after-unload",
        tau_proto::json_to_cbor(&serde_json::json!({"message":"update","destination":"team-ops"})),
    );
    assert!(ext.handle_send(invoke.clone()).is_none());
    let HarnessInputMessage::CompleteTransportSend(completion) =
        rx.recv().expect("background completion")
    else {
        panic!("expected completion");
    };
    ext.unload_agent(&invoke.agent_id);
    assert_eq!(ext.state.lock().expect("state").pending_posts.len(), 1);
    apply_output_message(
        &HarnessOutputMessage::CompleteTransportSendResult(
            tau_proto::CompleteTransportSendResult {
                request_id: completion.request_id,
                message_id: None,
                accepted: false,
                error: Some("rejected".to_owned()),
            },
        ),
        &ext,
    );
    assert!(matches!(
        ext.state
            .lock()
            .expect("state")
            .send_ledger
            .get(&invoke.call_id)
            .map(|entry| &entry.disposition),
        Some(SendLedgerDisposition::DefinitiveFailure {
            category: SendFailureCategory::CompletionRejected,
            copies: RemoteCopyPossibility::One
        })
    ));
    assert!(matches!(ext.handle_send(invoke), Some(Event::ToolError(_))));
    assert_eq!(client.sent_pairs().len(), 1);
}

#[test]
fn awaiting_completion_replay_respects_shared_worker_saturation() {
    let client = ScriptedPostClient::new([
        PostAttemptOutcome::Accepted(PostedMessage {
            channel_id: "C456".to_owned(),
            ts: "1.0".to_owned(),
            thread_ts: None,
        }),
        PostAttemptOutcome::Accepted(PostedMessage {
            channel_id: "G789".to_owned(),
            ts: "2.0".to_owned(),
            thread_ts: Some("1720000000.123456".to_owned()),
        }),
    ]);
    let (wait_entered_tx, wait_entered_rx) = mpsc::channel();
    let (wait_release_tx, wait_release_rx) = mpsc::channel();
    let (output_tx, rx) = mpsc::channel();
    let ext = Extension::new_with_scheduler(
        client.clone(),
        output_tx,
        Arc::new(StepScheduler {
            entered: wait_entered_tx,
            release: Mutex::new(wait_release_rx),
        }),
    );
    apply_test_config(&ext, proactive_cfg());
    let invokes = [
        tool_call(
            SEND_TOOL_NAME,
            "agent-a",
            "completion-worker-cap-a",
            tau_proto::json_to_cbor(
                &serde_json::json!({"message":"first","destination":"team-ops"}),
            ),
        ),
        tool_call(
            SEND_TOOL_NAME,
            "agent-b",
            "completion-worker-cap-b",
            tau_proto::json_to_cbor(
                &serde_json::json!({"message":"second","destination":"incident-thread"}),
            ),
        ),
    ];
    for invoke in &invokes {
        assert!(ext.handle_send(invoke.clone()).is_none());
        let _ = rx.recv().expect("initial completion");
    }
    for _ in 0..3 {
        let generation = ext.send_wake.generation();
        if ext
            .state
            .lock()
            .expect("state")
            .completion_resubmitting
            .is_empty()
        {
            break;
        }
        assert!(ext.send_wake.wait(generation, Duration::from_secs(1)));
    }
    {
        let mut state = ext.state.lock().expect("state");
        state.active_send_workers = ACTIVE_SEND_WORKER_LIMIT - 1;
        state
            .channel_attempt_deadlines
            .insert("D123".to_owned(), Instant::now() + Duration::from_secs(10));
        assert!(state.completion_resubmitting.is_empty());
    }
    let holder = tool_call(
        SEND_TOOL_NAME,
        "slot-holder",
        "completion-slot-holder",
        tau_proto::json_to_cbor(&serde_json::json!({"message":"holder","destination":"alice-dm"})),
    );
    assert!(ext.handle_send(holder).is_none());
    wait_entered_rx.recv().expect("worker owns final slot");
    for invoke in &invokes {
        assert!(ext.handle_send(invoke.clone()).is_none());
    }
    assert!(rx.try_recv().is_err());
    {
        let state = ext.state.lock().expect("state");
        assert_eq!(state.active_send_workers, ACTIVE_SEND_WORKER_LIMIT);
        assert_eq!(state.completion_resubmitting.len(), 2);
        assert_eq!(state.pending_completion_outputs.len(), 2);
    }
    {
        let mut state = ext.state.lock().expect("state");
        state.bump_send_agent_generation(&AgentId::parse("slot-holder").expect("agent id"));
    }
    ext.send_wake.notify_lifecycle_change();
    wait_release_tx.send(()).expect("release slot holder");
    let mut drained_call_ids = Vec::new();
    while drained_call_ids.len() < 2 {
        if let HarnessInputMessage::CompleteTransportSend(request) =
            rx.recv().expect("queued completion drains after capacity")
        {
            drained_call_ids.push(request.call_id);
        }
    }
    assert_eq!(
        drained_call_ids,
        invokes
            .iter()
            .map(|invoke| invoke.call_id.clone())
            .collect::<Vec<_>>()
    );
    let mut state = ext.state.lock().expect("state");
    state.active_send_workers = 0;
    drop(state);
    assert_eq!(client.bodies.lock().expect("bodies").len(), 2);
}

/// An ambiguous first attempt receives exactly one retry of the byte-identical
/// frozen route/body, and replay resubmits only the stable completion.
#[test]
fn ambiguous_send_retries_once_with_exact_frozen_body() {
    let client = ScriptedPostClient::new([
        PostAttemptOutcome::OutcomeUnknown(SendFailureCategory::Transport),
        PostAttemptOutcome::Accepted(PostedMessage {
            channel_id: "C123".to_owned(),
            ts: "1.0".to_owned(),
            thread_ts: None,
        }),
    ]);
    let scheduler = Arc::new(RecordingScheduler::default());
    let (tx, rx) = mpsc::channel();
    let ext = Extension::new_with_scheduler(client.clone(), tx, scheduler.clone());
    apply_test_config(&ext, proactive_cfg());
    register_agent(&ext, "agent-a");
    ext.state.lock().expect("state").insert_reply_route(
        MessageId::new("msg-source"),
        ReplyRoute {
            agent_id: agent_id("agent-a"),
            conversation: slack_conversation("C123", None),
            user_id: "U123".to_owned(),
            display_name: None,
            identity_alias: None,
            installation_team_id: "T123".to_owned(),
            policy_status: SenderPolicyStatus::Allowlisted,
        },
    );
    let invoke = tool_call(
        SEND_TOOL_NAME,
        "agent-a",
        "retry-exact",
        tau_proto::json_to_cbor(&serde_json::json!({
            "message":"bounded retry",
            "reply_to":"msg-source",
            "mention_source_user":true
        })),
    );

    let generation = ext.send_wake.generation();
    assert!(ext.handle_send(invoke.clone()).is_none());
    let first_completion = rx.recv().expect("retry completion");
    assert!(ext.send_wake.wait(generation, Duration::from_secs(1)));
    let HarnessInputMessage::CompleteTransportSend(first_completion) = first_completion else {
        panic!("expected completion");
    };
    assert_eq!(
        tau_proto::cbor_text_field(&first_completion.tool_result.result, "delivery_copies"),
        Some("one_or_two_possible".to_owned())
    );
    let bodies = client.bodies.lock().expect("bodies");
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0], bodies[1]);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&bodies[0]).expect("frozen body JSON")["text"],
        "<@U123> bounded retry"
    );
    drop(bodies);
    let delays = scheduler.delays.lock().expect("delays");
    assert_eq!(delays.len(), 1);
    assert!(delays[0] >= Duration::from_secs(1));
    assert!(delays[0] <= send_delivery::MAX_RETRY_AFTER);
    drop(delays);

    assert!(ext.handle_send(invoke).is_none());
    assert!(matches!(
        rx.recv().expect("replayed completion"),
        HarnessInputMessage::CompleteTransportSend(_)
    ));
    assert_eq!(client.bodies.lock().expect("bodies").len(), 2);
}

/// Two ambiguous outcomes exhaust the initial-plus-one budget, retain an
/// unknown terminal result, and document that zero, one, or two copies may
/// exist.
#[test]
fn ambiguous_send_exhausts_after_initial_plus_one() {
    let client = ScriptedPostClient::new([
        PostAttemptOutcome::OutcomeUnknown(SendFailureCategory::Timeout),
        PostAttemptOutcome::OutcomeUnknown(SendFailureCategory::MalformedResponse),
    ]);
    let scheduler = Arc::new(RecordingScheduler::default());
    let (tx, rx) = mpsc::channel();
    let ext = Extension::new_with_scheduler(client.clone(), tx, scheduler);
    apply_test_config(&ext, proactive_cfg());
    let invoke = tool_call(
        SEND_TOOL_NAME,
        "agent-a",
        "retry-exhausted",
        tau_proto::json_to_cbor(
            &serde_json::json!({"message":"ambiguous","destination":"team-ops"}),
        ),
    );

    assert!(ext.handle_send(invoke.clone()).is_none());
    let HarnessInputMessage::Emit(emit) = rx.recv().expect("terminal tool error") else {
        panic!("expected terminal tool error");
    };
    let Event::ToolError(error) = emit.event.as_ref() else {
        panic!("expected terminal tool error");
    };
    assert!(error.message.contains("zero, one, or two Slack copies"));
    assert_eq!(client.bodies.lock().expect("bodies").len(), 2);
    assert!(matches!(
        ext.state
            .lock()
            .expect("state")
            .send_ledger
            .get(&invoke.call_id)
            .map(|entry| &entry.disposition),
        Some(SendLedgerDisposition::ExhaustedUnknown {
            category: SendFailureCategory::MalformedResponse,
            copies: RemoteCopyPossibility::UpToTwo
        })
    ));
    assert!(matches!(
        ext.handle_send(invoke),
        Some(Event::ToolError(error)) if error.message.contains("zero, one, or two Slack copies")
    ));
    assert_eq!(client.bodies.lock().expect("bodies").len(), 2);
}

/// A definitive retry rejection cannot erase the possible copy from an
/// ambiguous initial attempt.
#[test]
fn ambiguous_then_definitive_retains_prior_copy_risk() {
    let client = ScriptedPostClient::new([
        PostAttemptOutcome::OutcomeUnknown(SendFailureCategory::Transport),
        PostAttemptOutcome::DefinitiveFailure(SendFailureCategory::PermissionDenied),
    ]);
    let (tx, rx) = mpsc::channel();
    let ext =
        Extension::new_with_scheduler(client.clone(), tx, Arc::new(RecordingScheduler::default()));
    apply_test_config(&ext, proactive_cfg());
    let invoke = tool_call(
        SEND_TOOL_NAME,
        "agent-a",
        "unknown-then-definitive",
        tau_proto::json_to_cbor(
            &serde_json::json!({"message":"ambiguous","destination":"team-ops"}),
        ),
    );

    assert!(ext.handle_send(invoke.clone()).is_none());
    let HarnessInputMessage::Emit(emit) = rx.recv().expect("stable failure") else {
        panic!("expected stable failure");
    };
    assert!(matches!(
        emit.event.as_ref(),
        Event::ToolError(error)
            if error.message.contains("permission")
                && error.message.contains("zero or one Slack copies")
    ));
    assert_eq!(client.bodies.lock().expect("bodies").len(), 2);
    assert!(matches!(
        ext.state
            .lock()
            .expect("state")
            .send_ledger
            .get(&invoke.call_id)
            .map(|entry| &entry.disposition),
        Some(SendLedgerDisposition::ExhaustedUnknown {
            category: SendFailureCategory::PermissionDenied,
            copies: RemoteCopyPossibility::UpToOne
        })
    ));
}

/// Lifecycle revocation wakes a scheduled retry, performs no second I/O, and
/// retains the cancelled call id so replay cannot regain authority.
#[test]
fn scheduled_retry_is_cancelled_by_capability_revocation() {
    let client = ScriptedPostClient::new([PostAttemptOutcome::OutcomeUnknown(
        SendFailureCategory::Transport,
    )]);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let scheduler = Arc::new(BlockingScheduler {
        entered: Mutex::new(Some(entered_tx)),
        release: Mutex::new(release_rx),
    });
    let (tx, rx) = mpsc::channel();
    let ext = Extension::new_with_scheduler(client.clone(), tx, scheduler);
    apply_test_config(&ext, proactive_cfg());
    let invoke = tool_call(
        SEND_TOOL_NAME,
        "agent-a",
        "retry-cancel",
        tau_proto::json_to_cbor(
            &serde_json::json!({"message":"cancel me","destination":"team-ops"}),
        ),
    );

    assert!(ext.handle_send(invoke.clone()).is_none());
    assert!(entered_rx.recv().expect("retry wait") >= Duration::from_secs(1));
    {
        let mut state = ext.state.lock().expect("state");
        state.capability_active = false;
        state.capability_generation = state.capability_generation.wrapping_add(1);
    }
    ext.send_wake.notify_lifecycle_change();
    release_tx.send(()).expect("release retry");
    let HarnessInputMessage::Emit(emit) = rx.recv().expect("cancelled tool error") else {
        panic!("expected cancelled tool error");
    };
    assert!(
        matches!(emit.event.as_ref(), Event::ToolError(error) if error.message.contains("zero or one Slack copies"))
    );
    assert_eq!(client.bodies.lock().expect("bodies").len(), 1);
    assert!(matches!(
        ext.state
            .lock()
            .expect("state")
            .send_ledger
            .get(&invoke.call_id)
            .map(|entry| &entry.disposition),
        Some(SendLedgerDisposition::Cancelled {
            copies: RemoteCopyPossibility::UpToOne
        })
    ));
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

/// Retry-After beyond the remaining 60-second logical-call horizon is retained
/// as a stable rate-limit failure without sleeping or issuing a second request.
#[test]
fn retry_after_cannot_escape_logical_call_horizon() {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let client = Arc::new(BlockingPostOutcomeClient {
        started: Mutex::new(Some(started_tx)),
        release: Mutex::new(release_rx),
        outcome: PostAttemptOutcome::RateLimited(send_delivery::MAX_RETRY_AFTER),
        attempts: Mutex::new(0),
    });
    let scheduler = Arc::new(RecordingScheduler::default());
    let (tx, rx) = mpsc::channel();
    let ext = Extension::new_with_scheduler(client.clone(), tx, scheduler.clone());
    apply_test_config(&ext, proactive_cfg());
    let invoke = tool_call(
        SEND_TOOL_NAME,
        "agent-a",
        "retry-horizon",
        tau_proto::json_to_cbor(
            &serde_json::json!({"message":"rate limited","destination":"team-ops"}),
        ),
    );

    assert!(ext.handle_send(invoke.clone()).is_none());
    started_rx.recv().expect("initial attempt started");
    ext.state
        .lock()
        .expect("state")
        .send_ledger
        .get_mut(&invoke.call_id)
        .expect("ledger entry")
        .prepared
        .retry_deadline = Instant::now();
    release_tx.send(()).expect("release rate limit");

    let HarnessInputMessage::Emit(emit) = rx.recv().expect("rate-limit failure") else {
        panic!("expected tool error");
    };
    assert!(matches!(
        emit.event.as_ref(),
        Event::ToolError(error) if error.message.contains("rate limit")
    ));
    assert_eq!(*client.attempts.lock().expect("attempts"), 1);
    assert!(scheduler.delays.lock().expect("delays").is_empty());
    assert!(matches!(
        ext.state
            .lock()
            .expect("state")
            .send_ledger
            .get(&invoke.call_id)
            .map(|entry| &entry.disposition),
        Some(SendLedgerDisposition::DefinitiveFailure {
            category: SendFailureCategory::RateLimited,
            copies: RemoteCopyPossibility::None
        })
    ));
}

/// Initial attempts reserve FIFO one-second spacing per channel while another
/// channel remains immediately independent.
#[test]
fn channel_attempts_are_spaced_without_global_blocking() {
    let client = ScriptedPostClient::new([
        PostAttemptOutcome::Accepted(PostedMessage {
            channel_id: "C456".to_owned(),
            ts: "1.0".to_owned(),
            thread_ts: None,
        }),
        PostAttemptOutcome::Accepted(PostedMessage {
            channel_id: "C456".to_owned(),
            ts: "2.0".to_owned(),
            thread_ts: None,
        }),
        PostAttemptOutcome::Accepted(PostedMessage {
            channel_id: "G789".to_owned(),
            ts: "3.0".to_owned(),
            thread_ts: Some("1720000000.123456".to_owned()),
        }),
    ]);
    let scheduler = Arc::new(RecordingScheduler::default());
    let (tx, rx) = mpsc::channel();
    let ext = Extension::new_with_scheduler(client, tx, scheduler.clone());
    apply_test_config(&ext, proactive_cfg());
    for (call_id, destination) in [
        ("channel-first", "team-ops"),
        ("channel-second", "team-ops"),
        ("other-channel", "incident-thread"),
    ] {
        assert!(
            ext.handle_send(tool_call(
                SEND_TOOL_NAME,
                "agent-a",
                call_id,
                tau_proto::json_to_cbor(
                    &serde_json::json!({"message":call_id,"destination":destination}),
                ),
            ))
            .is_none()
        );
        assert!(matches!(
            rx.recv().expect("completion"),
            HarnessInputMessage::CompleteTransportSend(_)
        ));
    }
    let delays = scheduler.delays.lock().expect("delays");
    assert_eq!(delays.len(), 1);
    assert!(delays[0] >= Duration::from_millis(900));
    assert!(delays[0] <= Duration::from_secs(1));
}

/// A blocked initial HTTP call runs in a delivery worker, so unregister can
/// revoke it immediately without waiting for network completion.
#[test]
fn blocked_initial_post_does_not_block_unregister() {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let client = Arc::new(BlockingSendClient {
        started: Mutex::new(Some(started_tx)),
        release: Mutex::new(release_rx),
    });
    let (tx, rx) = mpsc::channel();
    let ext = Extension::new(client, tx);
    apply_test_config(&ext, proactive_cfg());
    register_agent(&ext, "agent-a");
    let invoke = tool_call(
        SEND_TOOL_NAME,
        "agent-a",
        "blocked-initial",
        tau_proto::json_to_cbor(&serde_json::json!({"message":"blocked","destination":"team-ops"})),
    );

    assert!(ext.handle_send(invoke.clone()).is_none());
    started_rx.recv().expect("HTTP started");
    let unregister = ext.handle_register(tool(REGISTER_TOOL_NAME, "agent-a", bool_args(false)));
    assert!(matches!(unregister, Event::ToolResult(_)));
    release_tx.send(()).expect("release HTTP");
    let HarnessInputMessage::Emit(emit) = rx.recv().expect("cancelled tool error") else {
        panic!("expected cancellation");
    };
    assert!(
        matches!(emit.event.as_ref(), Event::ToolError(error) if error.message.contains("zero or one Slack copies"))
    );
    assert!(matches!(
        ext.state
            .lock()
            .expect("state")
            .send_ledger
            .get(&invoke.call_id)
            .map(|entry| &entry.disposition),
        Some(SendLedgerDisposition::Cancelled {
            copies: RemoteCopyPossibility::UpToOne
        })
    ));
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

/// The real tau-client serialized reader continues handling later tools and
/// session shutdown while the initial Slack HTTP call is blocked.
#[test]
fn serial_reader_progresses_during_blocked_send_http() {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let client = Arc::new(BlockingSendClient {
        started: Mutex::new(Some(started_tx)),
        release: Mutex::new(release_rx),
    });
    let (input_tx, input_rx) = mpsc::channel();
    let reader = StagedReader {
        chunks: input_rx,
        current: Vec::new(),
        offset: 0,
    };
    let output = NotifyingWriter::default();
    let observed = output.clone();
    let runner = std::thread::spawn(move || {
        run_with_client(reader, output, client).map_err(|error| error.to_string())
    });

    input_tx
        .send(encode_output_frames(&[
            proactive_config_message(),
            HarnessOutputMessage::RegisterTransportCapabilityResult(
                tau_proto::RegisterTransportCapabilityResult {
                    request_id: format!("{CAPABILITY_REQUEST_PREFIX}1"),
                    accepted: true,
                    error: None,
                },
            ),
            HarnessOutputMessage::deliver(Event::ToolStarted(tool_call(
                SEND_TOOL_NAME,
                "agent-a",
                "reader-blocked-send",
                tau_proto::json_to_cbor(
                    &serde_json::json!({"message":"blocked","destination":"team-ops"}),
                ),
            ))),
        ]))
        .expect("send first input stage");
    started_rx.recv().expect("HTTP attempt started");

    input_tx
        .send(encode_output_frames(&[
            HarnessOutputMessage::deliver(Event::ToolStarted(tool_call(
                SEND_TOOL_NAME,
                "agent-a",
                "reader-follow-up",
                tau_proto::json_to_cbor(&serde_json::json!({"message":"bad","channel_id":"C999"})),
            ))),
            HarnessOutputMessage::deliver(Event::ToolStarted(tool(
                REGISTER_TOOL_NAME,
                "agent-a",
                bool_args(false),
            ))),
            HarnessOutputMessage::deliver(Event::SessionShutdown(tau_proto::SessionShutdown {
                session_id: "reader-session".into(),
            })),
        ]))
        .expect("send lifecycle stage");
    observed.wait_for(b"unknown argument `channel_id`");

    release_tx.send(()).expect("release blocked HTTP");
    drop(input_tx);
    runner
        .join()
        .expect("runner thread")
        .expect("runner should shut down cleanly");
}

/// The real tau-client serialized reader continues handling later tools and
/// shutdown while the sole retry is parked in its event-driven wait.
#[test]
fn serial_reader_progresses_during_retry_wait() {
    let client = ScriptedPostClient::new([PostAttemptOutcome::OutcomeUnknown(
        SendFailureCategory::Transport,
    )]);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let scheduler = Arc::new(BlockingScheduler {
        entered: Mutex::new(Some(entered_tx)),
        release: Mutex::new(release_rx),
    });
    let (input_tx, input_rx) = mpsc::channel();
    let reader = StagedReader {
        chunks: input_rx,
        current: Vec::new(),
        offset: 0,
    };
    let output = NotifyingWriter::default();
    let observed = output.clone();
    let runner = std::thread::spawn(move || {
        run_with_client_and_scheduler(reader, output, client, scheduler)
            .map_err(|error| error.to_string())
    });

    input_tx
        .send(encode_output_frames(&[
            proactive_config_message(),
            HarnessOutputMessage::RegisterTransportCapabilityResult(
                tau_proto::RegisterTransportCapabilityResult {
                    request_id: format!("{CAPABILITY_REQUEST_PREFIX}1"),
                    accepted: true,
                    error: None,
                },
            ),
            HarnessOutputMessage::deliver(Event::ToolStarted(tool_call(
                SEND_TOOL_NAME,
                "agent-a",
                "reader-retry-wait",
                tau_proto::json_to_cbor(
                    &serde_json::json!({"message":"retry","destination":"team-ops"}),
                ),
            ))),
        ]))
        .expect("send first input stage");
    assert!(entered_rx.recv().expect("retry wait") >= Duration::from_secs(1));

    input_tx
        .send(encode_output_frames(&[
            HarnessOutputMessage::deliver(Event::ToolStarted(tool_call(
                SEND_TOOL_NAME,
                "agent-a",
                "reader-retry-follow-up",
                tau_proto::json_to_cbor(&serde_json::json!({"message":"bad","channel_id":"C999"})),
            ))),
            HarnessOutputMessage::deliver(Event::SessionShutdown(tau_proto::SessionShutdown {
                session_id: "reader-retry-session".into(),
            })),
        ]))
        .expect("send lifecycle stage");
    observed.wait_for(b"unknown argument `channel_id`");

    release_tx.send(()).expect("release retry");
    drop(input_tx);
    runner
        .join()
        .expect("runner thread")
        .expect("runner should shut down cleanly");
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

/// Slack-accepted channel and fixed-thread mismatches become stable failures;
/// replaying the same call id must neither repost nor leak native identifiers.
#[test]
fn accepted_response_route_mismatches_replay_without_reposting() {
    for (channel, thread) in [("C999", None), ("G789", Some("9.9"))] {
        let client = Arc::new(MismatchedPostClient {
            posts: Mutex::new(0),
            returned_channel: channel.to_owned(),
            returned_thread: thread.map(str::to_owned),
        });
        let (tx, rx) = mpsc::channel();
        let ext = Extension::new(client.clone(), tx);
        apply_test_config(&ext, proactive_cfg());
        ext.state.lock().expect("state").capability_active = true;
        let invoke = tool(
            SEND_TOOL_NAME,
            "agent-a",
            tau_proto::json_to_cbor(
                &serde_json::json!({"message": "update", "destination": "incident-thread"}),
            ),
        );
        assert!(ext.handle_send(invoke.clone()).is_none());
        let HarnessInputMessage::Emit(first) = rx.recv().expect("background mismatch") else {
            panic!("expected background tool error");
        };
        let Event::ToolError(first_error) = first.event.as_ref() else {
            panic!("expected background tool error");
        };
        for error in [
            first_error,
            match ext.handle_send(invoke) {
                Some(Event::ToolError(error)) => Box::leak(Box::new(error)),
                _ => panic!("expected stable replay mismatch"),
            },
        ] {
            assert!(error.message.contains("conflicting route metadata"));
            assert!(!error.message.contains(channel));
            assert!(!error.message.contains("9.9"));
        }
        assert_eq!(*client.posts.lock().expect("posts"), 1);
    }
}

/// Accepted-attempt retention rejects conflicting replay, remains bounded, and
/// survives restart-required reconfiguration after an authorized post freezes
/// policy.
#[test]
fn accepted_send_attempts_conflict_bound_and_freeze() {
    let (ext, rx, client) = extension();
    apply_test_config(&ext, proactive_cfg());
    let first = tool(
        SEND_TOOL_NAME,
        "agent-a",
        tau_proto::json_to_cbor(
            &serde_json::json!({"message": "first", "destination": "team-ops"}),
        ),
    );
    assert!(ext.handle_send(first.clone()).is_none());
    rx.recv().expect("first completion");
    let mut conflict = first;
    conflict.arguments = tau_proto::json_to_cbor(
        &serde_json::json!({"message": "changed", "destination": "team-ops"}),
    );
    assert!(matches!(
        ext.handle_send(conflict),
        Some(Event::ToolError(_))
    ));
    assert_eq!(client.sent_pairs().len(), 1);

    let mut state = ext.state.lock().expect("state");
    let template = state
        .send_ledger
        .values()
        .next()
        .expect("retained first intent")
        .clone();
    state.clear_send_ledger();
    for index in 0..SEND_LEDGER_LIMIT {
        let call_id = tau_proto::ToolCallId::new(format!("bounded-{index}"));
        let mut entry = template.clone();
        entry.prepared.invoke.call_id = call_id.clone();
        if let SendLedgerDisposition::AwaitingCompletion { request, copies: _ } =
            &mut entry.disposition
        {
            request.call_id = call_id.clone();
            request.request_id = format!("bounded-completion-{index}");
        }
        state.send_ledger.insert(call_id, entry);
    }
    state.config_frozen = false;
    assert_eq!(state.send_ledger.len(), SEND_LEDGER_LIMIT);
    assert!(
        state
            .send_ledger
            .contains_key(&tau_proto::ToolCallId::from("bounded-0"))
    );
    drop(state);
    let oldest = ToolStarted {
        call_id: "bounded-0".into(),
        ..tool(
            SEND_TOOL_NAME,
            "agent-a",
            tau_proto::json_to_cbor(
                &serde_json::json!({"message": "first", "destination": "team-ops"}),
            ),
        )
    };
    assert!(ext.handle_send(oldest).is_none());
    assert!(matches!(
        rx.recv().expect("oldest replay"),
        HarnessInputMessage::CompleteTransportSend(_)
    ));
    let fresh = tool_call(
        SEND_TOOL_NAME,
        "agent-a",
        "bounded-new",
        tau_proto::json_to_cbor(&serde_json::json!({"message": "new", "destination": "team-ops"})),
    );
    assert!(matches!(
        ext.handle_send(fresh),
        Some(Event::ToolError(error)) if error.message.contains("ledger is full")
    ));
    assert_eq!(client.sent_pairs().len(), 1);
    assert!(!ext.state.lock().expect("state").config_frozen);
    ext.handle_register(tool(REGISTER_TOOL_NAME, "agent-a", bool_args(false)));
    assert_eq!(
        ext.state.lock().expect("state").send_ledger.len(),
        SEND_LEDGER_LIMIT
    );
    ext.retire_send_authority();
    assert!(ext.state.lock().expect("state").send_ledger.is_empty());
}

#[test]
fn unrelated_authority_wake_preserves_initial_wait_and_delivery() {
    let client = ScriptedPostClient::new([PostAttemptOutcome::Accepted(PostedMessage {
        channel_id: "C456".to_owned(),
        ts: "1.0".to_owned(),
        thread_ts: None,
    })]);
    let (output_tx, output_rx) = mpsc::channel();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let scheduler = Arc::new(StepScheduler {
        entered: entered_tx,
        release: Mutex::new(release_rx),
    });
    let ext = Extension::new_with_scheduler(client.clone(), output_tx, scheduler);
    apply_test_config(&ext, proactive_cfg());
    ext.state
        .lock()
        .expect("state")
        .channel_attempt_deadlines
        .insert("C456".to_owned(), Instant::now() + Duration::from_secs(10));
    let invoke = tool_call(
        SEND_TOOL_NAME,
        "agent-a",
        "initial-wake",
        tau_proto::json_to_cbor(&serde_json::json!({"message":"initial","destination":"team-ops"})),
    );
    assert!(ext.handle_send(invoke).is_none());
    entered_rx.recv().expect("first wait");
    {
        let mut state = ext.state.lock().expect("state");
        state.bump_send_agent_generation(&agent_id("unrelated-agent"));
    }
    ext.send_wake.notify_lifecycle_change();
    release_tx.send(()).expect("release notified wait");
    entered_rx.recv().expect("remaining wait");
    release_tx.send(()).expect("release remaining wait");
    assert!(matches!(
        output_rx.recv().expect("completion"),
        HarnessInputMessage::CompleteTransportSend(_)
    ));
    assert_eq!(client.bodies.lock().expect("bodies").len(), 1);
}

#[test]
fn unrelated_authority_wake_preserves_retry_wait_and_exact_retry() {
    let client = ScriptedPostClient::new([
        PostAttemptOutcome::OutcomeUnknown(SendFailureCategory::Transport),
        PostAttemptOutcome::Accepted(PostedMessage {
            channel_id: "C456".to_owned(),
            ts: "1.0".to_owned(),
            thread_ts: None,
        }),
    ]);
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
        "retry-wake",
        tau_proto::json_to_cbor(&serde_json::json!({"message":"retry","destination":"team-ops"})),
    );
    assert!(ext.handle_send(invoke).is_none());
    entered_rx.recv().expect("first retry wait");
    ext.send_wake.notify_lifecycle_change();
    release_tx.send(()).expect("release notified retry");
    entered_rx.recv().expect("remaining retry wait");
    release_tx.send(()).expect("release remaining retry");
    assert!(matches!(
        output_rx.recv().expect("completion"),
        HarnessInputMessage::CompleteTransportSend(_)
    ));
    let bodies = client.bodies.lock().expect("bodies");
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0], bodies[1]);
}

#[test]
fn cancelled_front_retry_does_not_publish_a_future_channel_barrier() {
    let client = ScriptedPostClient::new([
        PostAttemptOutcome::RateLimited(Duration::from_secs(30)),
        PostAttemptOutcome::Accepted(PostedMessage {
            channel_id: "C456".to_owned(),
            ts: "2.0".to_owned(),
            thread_ts: None,
        }),
    ]);
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
    let first = tool_call(
        SEND_TOOL_NAME,
        "agent-a",
        "cancel-front-a",
        tau_proto::json_to_cbor(&serde_json::json!({"message":"first","destination":"team-ops"})),
    );
    let second = tool_call(
        SEND_TOOL_NAME,
        "agent-b",
        "cancel-front-b",
        tau_proto::json_to_cbor(&serde_json::json!({"message":"second","destination":"team-ops"})),
    );
    assert!(ext.handle_send(first.clone()).is_none());
    let first_delay = entered_rx.recv().expect("front retry wait");
    assert!(first_delay >= Duration::from_secs(30));
    assert!(ext.handle_send(second).is_none());
    {
        let mut state = ext.state.lock().expect("state");
        state.bump_send_agent_generation(&first.agent_id);
    }
    ext.send_wake.notify_lifecycle_change();
    release_tx.send(()).expect("release cancelled retry");
    let successor_delay = entered_rx.recv().expect("successor pacing wait");
    assert!(
        successor_delay <= send_delivery::MIN_RETRY_DELAY,
        "cancelled future retry leaked a {successor_delay:?} barrier"
    );
    release_tx.send(()).expect("release successor pacing");
    let outputs = [
        output_rx.recv().expect("cancelled front result"),
        output_rx.recv().expect("successor completion"),
    ];
    assert!(
        outputs
            .iter()
            .any(|output| matches!(output, HarnessInputMessage::CompleteTransportSend(_)))
    );
    assert_eq!(client.bodies.lock().expect("bodies").len(), 2);
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
fn same_channel_calls_hold_a_live_fifo_turn_across_provider_io() {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let client = Arc::new(OrderedBlockingPostClient {
        started: started_tx,
        release: Mutex::new(release_rx),
        attempts: Mutex::new(0),
    });
    let (output_tx, output_rx) = mpsc::channel();
    let ext = Extension::new(client.clone(), output_tx);
    apply_test_config(&ext, proactive_cfg());
    for (call_id, message) in [("fifo-a", "first"), ("fifo-b", "second")] {
        assert!(
            ext.handle_send(tool_call(
                SEND_TOOL_NAME,
                "agent-a",
                call_id,
                tau_proto::json_to_cbor(
                    &serde_json::json!({"message":message,"destination":"team-ops"}),
                ),
            ))
            .is_none()
        );
    }
    assert_eq!(started_rx.recv().expect("first start"), "first");
    assert!(
        started_rx.recv_timeout(Duration::from_millis(20)).is_err(),
        "second call started before the front call released its channel"
    );
    release_tx.send(()).expect("release first");
    assert!(matches!(
        output_rx.recv().expect("first completion"),
        HarnessInputMessage::CompleteTransportSend(_)
    ));
    assert_eq!(started_rx.recv().expect("second start"), "second");
    release_tx.send(()).expect("release second");
    assert!(matches!(
        output_rx.recv().expect("second completion"),
        HarnessInputMessage::CompleteTransportSend(_)
    ));
}

#[test]
fn stale_worker_release_cannot_remove_reused_call_reservation() {
    let (started_tx, started_rx) = mpsc::channel();
    let (post_release_tx, post_release_rx) = mpsc::channel();
    let client = Arc::new(OrderedBlockingPostClient {
        started: started_tx,
        release: Mutex::new(post_release_rx),
        attempts: Mutex::new(0),
    });
    let (wait_entered_tx, wait_entered_rx) = mpsc::channel();
    let (wait_release_tx, wait_release_rx) = mpsc::channel();
    let (output_tx, output_rx) = mpsc::channel();
    let ext = Extension::new_with_scheduler(
        client,
        output_tx,
        Arc::new(StepScheduler {
            entered: wait_entered_tx,
            release: Mutex::new(wait_release_rx),
        }),
    );
    apply_test_config(&ext, proactive_cfg());
    let invoke = tool_call(
        SEND_TOOL_NAME,
        "agent-a",
        "reused-across-session",
        tau_proto::json_to_cbor(&serde_json::json!({"message":"same","destination":"team-ops"})),
    );
    assert!(ext.handle_send(invoke.clone()).is_none());
    assert_eq!(started_rx.recv().expect("old attempt"), "same");

    ext.retire_send_authority();
    {
        let mut state = ext.state.lock().expect("state");
        state.capability_active = true;
        state
            .channel_attempt_deadlines
            .insert("C456".to_owned(), Instant::now() + Duration::from_secs(10));
    }
    assert!(ext.handle_send(invoke).is_none());
    wait_entered_rx.recv().expect("new reservation wait");

    let release_generation = ext.send_wake.generation();
    post_release_tx.send(()).expect("release old attempt");
    assert!(
        ext.send_wake
            .wait(release_generation, Duration::from_secs(1)),
        "old worker did not release"
    );
    wait_release_tx
        .send(())
        .expect("release interrupted new-attempt wait");
    wait_entered_rx.recv().expect("remaining new-attempt wait");
    wait_release_tx
        .send(())
        .expect("release remaining new-attempt wait");
    assert_eq!(
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("new attempt retained its token-scoped FIFO reservation"),
        "same"
    );
    post_release_tx.send(()).expect("release new attempt");
    assert!(matches!(
        output_rx.recv().expect("new completion"),
        HarnessInputMessage::CompleteTransportSend(_)
    ));
}

#[test]
fn stale_worker_release_cannot_remove_reused_completion_owner() {
    let (first_started_tx, first_started_rx) = mpsc::channel();
    let (first_release_tx, first_release_rx) = mpsc::channel();
    let client = Arc::new(StaleFirstPostClient {
        first_started: Mutex::new(Some(first_started_tx)),
        first_release: Mutex::new(first_release_rx),
        attempts: Mutex::new(0),
    });
    let spawner = Arc::new(HoldingCompletionSpawner::default());
    let (output_tx, output_rx) = mpsc::channel();
    let ext = Extension::new_with_scheduler_and_completion_spawner(
        client,
        output_tx,
        Arc::new(RecordingScheduler::default()),
        spawner.clone(),
    );
    apply_test_config(&ext, proactive_cfg());
    let invoke = tool_call(
        SEND_TOOL_NAME,
        "agent-a",
        "reused-completion-owner",
        tau_proto::json_to_cbor(&serde_json::json!({"message":"same","destination":"team-ops"})),
    );
    assert!(ext.handle_send(invoke.clone()).is_none());
    first_started_rx.recv().expect("old attempt");

    ext.retire_send_authority();
    ext.state.lock().expect("state").capability_active = true;
    assert!(ext.handle_send(invoke.clone()).is_none());
    assert!(matches!(
        output_rx.recv().expect("new completion"),
        HarnessInputMessage::CompleteTransportSend(_)
    ));
    for _ in 0..3 {
        let generation = ext.send_wake.generation();
        if ext
            .state
            .lock()
            .expect("state")
            .completion_resubmitting
            .is_empty()
        {
            break;
        }
        assert!(ext.send_wake.wait(generation, Duration::from_secs(1)));
    }

    assert!(ext.handle_send(invoke.clone()).is_none());
    assert_eq!(spawner.tasks.lock().expect("tasks").len(), 1);
    let new_owner = ext
        .state
        .lock()
        .expect("state")
        .completion_resubmitting
        .iter()
        .next()
        .expect("new completion owner")
        .clone();

    let generation = ext.send_wake.generation();
    first_release_tx.send(()).expect("release stale attempt");
    assert!(ext.send_wake.wait(generation, Duration::from_secs(1)));
    assert!(
        ext.state
            .lock()
            .expect("state")
            .completion_resubmitting
            .contains(&new_owner)
    );
    assert!(ext.handle_send(invoke.clone()).is_none());
    assert_eq!(
        spawner.tasks.lock().expect("tasks").len(),
        1,
        "replay must coalesce behind the exact new-session owner"
    );

    ext.retire_send_authority();
    ext.state.lock().expect("state").capability_active = true;
    assert!(ext.handle_send(invoke.clone()).is_none());
    assert!(matches!(
        output_rx.recv().expect("reused-call completion"),
        HarnessInputMessage::CompleteTransportSend(request)
            if request.call_id == invoke.call_id
    ));
    let stale_task = spawner
        .tasks
        .lock()
        .expect("tasks")
        .pop_front()
        .expect("held old-session completion");
    stale_task();
    assert!(
        output_rx.try_recv().is_err(),
        "retired completion owner published into a reused call"
    );
}

#[test]
fn replay_completion_spawn_failure_retires_without_holding_state_lock() {
    let client = ScriptedPostClient::new([PostAttemptOutcome::Accepted(PostedMessage {
        channel_id: "C456".to_owned(),
        ts: "1.0".to_owned(),
        thread_ts: None,
    })]);
    let (output_tx, output_rx) = mpsc::channel();
    let ext = Arc::new(Extension::new_with_scheduler_and_completion_spawner(
        client.clone(),
        output_tx,
        Arc::new(RecordingScheduler::default()),
        Arc::new(FailingCompletionSpawner),
    ));
    apply_test_config(&ext, proactive_cfg());
    let invoke = tool_call(
        SEND_TOOL_NAME,
        "agent-a",
        "completion-spawn-failure",
        tau_proto::json_to_cbor(&serde_json::json!({"message":"same","destination":"team-ops"})),
    );
    assert!(ext.handle_send(invoke.clone()).is_none());
    let _ = output_rx.recv().expect("initial completion");

    let (done_tx, done_rx) = mpsc::channel();
    let replay_ext = Arc::clone(&ext);
    std::thread::spawn(move || {
        let result = replay_ext.handle_send(invoke);
        done_tx.send(result).expect("report replay result");
    });
    assert!(
        done_rx.recv_timeout(Duration::from_secs(1)).is_ok(),
        "synchronous spawn failure deadlocked while reacquiring send state"
    );
    let state = ext.state.lock().expect("state");
    assert!(!state.capability_active);
    assert!(state.send_ledger.is_empty());
    drop(state);
    assert_eq!(client.bodies.lock().expect("bodies").len(), 1);
}

#[test]
fn socket_worker_retirement_shares_completion_publication_gate() {
    let client = ScriptedPostClient::new([]);
    let (output_tx, _output_rx) = mpsc::channel();
    let primary = Extension::new(client.clone(), output_tx);
    apply_test_config(&primary, proactive_cfg());
    let worker = Extension::new_socket_worker_view(
        SendRetirement {
            state: Arc::clone(&primary.state),
            completion_publication_gate: Arc::clone(&primary.completion_publication_gate),
            wake: Arc::clone(&primary.send_wake),
        },
        client,
        primary.output.clone(),
        Arc::clone(&primary.shutdown),
    );
    assert!(Arc::ptr_eq(
        &primary.completion_publication_gate,
        &worker.completion_publication_gate
    ));
    assert!(Arc::ptr_eq(&primary.send_wake, &worker.send_wake));

    let wake_generation = primary.send_wake.generation();
    let publication = primary
        .completion_publication_gate
        .lock()
        .expect("publication gate");
    let (retired_tx, retired_rx) = mpsc::channel();
    std::thread::spawn(move || {
        drop(worker);
        retired_tx.send(()).expect("announce retirement");
    });
    assert!(
        retired_rx.recv_timeout(Duration::from_millis(20)).is_err(),
        "Socket worker retirement bypassed publication admission"
    );
    assert!(primary.state.lock().expect("state").capability_active);
    drop(publication);
    retired_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("retirement after publication admission");
    assert!(primary.send_wake.generation() > wake_generation);
    assert!(!primary.state.lock().expect("state").capability_active);
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

#[test]
fn completion_writer_failure_retires_initial_and_replayed_authority() {
    for failure_point in ["initial", "replay"] {
        let client = ScriptedPostClient::new([PostAttemptOutcome::Accepted(PostedMessage {
            channel_id: "C456".to_owned(),
            ts: "1.0".to_owned(),
            thread_ts: None,
        })]);
        let (output_tx, output_rx) = mpsc::channel();
        let ext = Extension::new(client.clone(), output_tx);
        apply_test_config(&ext, proactive_cfg());
        let invoke = tool_call(
            SEND_TOOL_NAME,
            "agent-a",
            &format!("writer-{failure_point}"),
            tau_proto::json_to_cbor(
                &serde_json::json!({"message":"writer","destination":"team-ops"}),
            ),
        );
        if failure_point == "initial" {
            drop(output_rx);
            assert!(ext.handle_send(invoke.clone()).is_none());
            for _ in 0..3 {
                let generation = ext.send_wake.generation();
                if !ext.state.lock().expect("state").capability_active {
                    break;
                }
                assert!(ext.send_wake.wait(generation, Duration::from_secs(1)));
            }
        } else {
            assert!(ext.handle_send(invoke.clone()).is_none());
            let _ = output_rx.recv().expect("initial completion");
            drop(output_rx);
            assert!(ext.handle_send(invoke.clone()).is_none());
            for _ in 0..3 {
                let generation = ext.send_wake.generation();
                if !ext.state.lock().expect("state").capability_active {
                    break;
                }
                assert!(ext.send_wake.wait(generation, Duration::from_secs(1)));
            }
        }
        let state = ext.state.lock().expect("state");
        assert!(!state.capability_active);
        assert!(state.send_ledger.is_empty());
        drop(state);
        assert!(ext.handle_send(invoke).is_some());
        assert_eq!(client.bodies.lock().expect("bodies").len(), 1);
    }
}
