//! Local-fake and loopback lifecycle coverage follows
//! `testing.md`; route/security matrices follow
//! `SPEC-tau-ext-slack-conversation-routing`.

use std::collections::hash_map as path_std_collections_hash_map;
use std::{io as path_std_io, net as path_std_net};

use tokio::{net as path_tokio_net, sync as path_tokio_sync, time as path_tokio_time};

mod send_delivery_tests;

use std::io::{Read, Write};
use std::sync::{Condvar, Mutex};

use tau_proto::{ContentPart, HarnessInputMessage, HarnessOutputMessage, ToolStarted};
use tokio_tungstenite::tungstenite::protocol::frame::Frame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::{Data, OpCode};

use super::reactions::{
    ReactionActionKind, ReactionApiError, ReactionAttemptDisposition, ReactionKey, ReactionOwner,
    ReactionReservation, reaction_error_message, valid_outbound_emoji,
};
use super::send_delivery::ImmediateSendScheduler;
use super::*;

impl Extension {
    /// Create a test extension whose logical waits advance without real sleeps.
    fn new(client: Arc<dyn SlackClient>, output: impl Into<Output>) -> Self {
        Self::new_with_scheduler(client, output, Arc::new(ImmediateSendScheduler))
    }

    /// Create a test extension with a separately supplied reaction client.
    fn new_with_reaction_client(
        client: Arc<dyn SlackClient>,
        reaction_client: Arc<dyn ReactionClient>,
        output: impl Into<Output>,
    ) -> Self {
        Self::new_with_clients_and_scheduler(
            client,
            reaction_client,
            output,
            Arc::new(ImmediateSendScheduler),
        )
    }
}

#[derive(Clone, Default)]
struct SharedWriter {
    /// Shared byte buffer written by the runner's writer thread.
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

/// One complete fake Slack post recorded atomically for assertions.
#[derive(Clone, Debug, Eq, PartialEq)]
struct SentMessage {
    /// Destination conversation.
    channel_id: String,
    /// Posted text.
    text: String,
    /// Optional originating thread root.
    thread_ts: Option<String>,
}

/// Decode one frozen test body without giving production code a reverse parser.
fn sent_message(body: &FrozenPostBody) -> SentMessage {
    let value: serde_json::Value =
        serde_json::from_str(body.wire_json()).expect("valid frozen post JSON");
    SentMessage {
        channel_id: value["channel"]
            .as_str()
            .expect("frozen channel")
            .to_owned(),
        text: value["text"].as_str().expect("frozen text").to_owned(),
        thread_ts: value["thread_ts"].as_str().map(str::to_owned),
    }
}

/// Build one agent-mode post body for wire-shape assertions.
fn post_message_body(channel_id: &str, text: &str, thread_ts: Option<&str>) -> serde_json::Value {
    let mode = SlackPostMode::agent(text.to_owned(), None).expect("test fixture is safe");
    serde_json::from_str(FrozenPostBody::new(channel_id, thread_ts, &mode).wire_json())
        .expect("frozen body is JSON")
}

/// One exact outbound reaction call recorded by the fake.
#[derive(Clone, Debug, Eq, PartialEq)]
struct RecordedReaction {
    /// Explicit operation.
    action: ReactionActionKind,
    /// Native target conversation kept test-private.
    channel_id: String,
    /// Native exact item timestamp.
    message_ts: String,
    /// Strict emoji name.
    emoji: String,
}

struct FakeClient {
    /// Recorded outbound messages.
    sent: Mutex<Vec<SentMessage>>,
    /// Socket-open call count.
    open_count: Mutex<usize>,
    /// Authentication call count.
    auth_count: Mutex<usize>,
    /// Live-human identity lookup count.
    identity_count: Mutex<usize>,
    /// Recorded outbound reaction calls.
    reactions: Mutex<Vec<RecordedReaction>>,
    /// Scripted typed reaction outcomes.
    reaction_results: Mutex<VecDeque<Result<(), ReactionApiError>>>,
    /// Set after one reaction call reaches its typed remote outcome.
    reaction_completed: Arc<AtomicBool>,
    /// Optional one-shot event-driven completion signal.
    reaction_completion_signal: Mutex<Option<mpsc::Sender<()>>>,
}

impl FakeClient {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            sent: Mutex::new(Vec::new()),
            open_count: Mutex::new(0),
            auth_count: Mutex::new(0),
            identity_count: Mutex::new(0),
            reactions: Mutex::new(Vec::new()),
            reaction_results: Mutex::new(VecDeque::new()),
            reaction_completed: Arc::new(AtomicBool::new(false)),
            reaction_completion_signal: Mutex::new(None),
        })
    }

    /// Return channel/text pairs for assertions that predate thread routing.
    fn sent_pairs(&self) -> Vec<(String, String)> {
        self.sent
            .lock()
            .expect("lock")
            .iter()
            .map(|message| (message.channel_id.clone(), message.text.clone()))
            .collect()
    }

    /// Return thread destinations from the same atomic post records.
    fn sent_thread_ids(&self) -> Vec<Option<String>> {
        self.sent
            .lock()
            .expect("lock")
            .iter()
            .map(|message| message.thread_ts.clone())
            .collect()
    }

    /// Queue one typed reaction outcome for the next fake API call.
    fn push_reaction_result(&self, result: Result<(), ReactionApiError>) {
        self.reaction_results
            .lock()
            .expect("lock")
            .push_back(result);
    }
}

/// Build one presentation-free test identity from a single typed lookup result.
fn test_verified_human(user_id: &str, human: bool) -> Option<VerifiedSlackHuman> {
    human.then(|| VerifiedSlackHuman {
        user_id: user_id.to_owned(),
        display_name: None,
    })
}

impl SlackClient for FakeClient {
    fn open_socket(&self, _cfg: &RuntimeConfig) -> Result<String, SlackApiError> {
        *self.open_count.lock().expect("lock") += 1;
        Ok("ws://127.0.0.1:9/socket-ticket".to_owned())
    }

    fn auth_test(&self, _cfg: &RuntimeConfig) -> Result<SlackInstallationIdentity, SlackApiError> {
        *self.auth_count.lock().expect("lock") += 1;
        Ok(SlackInstallationIdentity {
            bot_user_id: "UBOT123".to_owned(),
            team_id: "T123".to_owned(),
        })
    }

    fn verified_human_identity(
        &self,
        _cfg: &RuntimeConfig,
        user_id: &str,
    ) -> Result<Option<VerifiedSlackHuman>, SlackApiError> {
        *self.identity_count.lock().expect("lock") += 1;
        Ok(test_verified_human(user_id, user_id != "UBOT999"))
    }

    fn post_message(
        &self,
        _cfg: &RuntimeConfig,
        body: &FrozenPostBody,
    ) -> PostAttemptOutcome<PostedMessage> {
        let mut sent = self.sent.lock().expect("lock");
        sent.push(sent_message(body));
        PostAttemptOutcome::Accepted(PostedMessage {
            channel_id: body.channel_id().to_owned(),
            ts: format!("{}.0", sent.len()),
            thread_ts: body.thread_ts().map(str::to_owned),
        })
    }
}

impl ReactionClient for FakeClient {
    fn react(
        &self,
        _cfg: &RuntimeConfig,
        action: ReactionActionKind,
        channel_id: &str,
        message_ts: &str,
        emoji: &str,
    ) -> Result<(), ReactionApiError> {
        self.reactions.lock().expect("lock").push(RecordedReaction {
            action,
            channel_id: channel_id.to_owned(),
            message_ts: message_ts.to_owned(),
            emoji: emoji.to_owned(),
        });
        let result = self
            .reaction_results
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or(Ok(()));
        self.reaction_completed.store(true, Ordering::Release);
        if let Some(signal) = self
            .reaction_completion_signal
            .lock()
            .expect("reaction completion signal")
            .take()
        {
            signal.send(()).expect("signal reaction completion");
        }
        result
    }
}

/// Reaction-only client proving reaction lifecycle tests need no transport,
/// identity, or message-posting implementation.
struct BlockingReactionClient {
    started: Mutex<Option<mpsc::Sender<()>>>,
    release: Mutex<mpsc::Receiver<()>>,
}

impl ReactionClient for BlockingReactionClient {
    fn react(
        &self,
        _cfg: &RuntimeConfig,
        _action: ReactionActionKind,
        _channel_id: &str,
        _message_ts: &str,
        _emoji: &str,
    ) -> Result<(), ReactionApiError> {
        if let Some(started) = self.started.lock().expect("started").take() {
            started.send(()).expect("announce blocked reaction");
        }
        self.release
            .lock()
            .expect("release")
            .recv()
            .expect("release blocked reaction");
        Ok(())
    }
}

/// Scripted identity client used to verify fail-closed outage and recovery
/// behavior.
struct IdentitySequenceClient {
    /// Ordered users.info results consumed by calls.
    results: Mutex<VecDeque<Result<bool, SlackApiError>>>,
}
/// Identity fake whose first lookup blocks behind a deterministic barrier.
struct BlockingIdentityClient {
    /// Signals when the first users.info call begins.
    started: Mutex<Option<mpsc::Sender<()>>>,
    /// Releases the first users.info call.
    release: Mutex<mpsc::Receiver<()>>,
}

impl SlackClient for BlockingIdentityClient {
    fn open_socket(&self, _cfg: &RuntimeConfig) -> Result<String, SlackApiError> {
        unreachable!("socket URL supplied by test")
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
        user_id: &str,
    ) -> Result<Option<VerifiedSlackHuman>, SlackApiError> {
        if let Some(started) = self.started.lock().expect("started lock").take() {
            started.send(()).expect("signal identity start");
            self.release
                .lock()
                .expect("release lock")
                .recv()
                .expect("release identity");
        }
        Ok(test_verified_human(user_id, true))
    }

    fn post_message(
        &self,
        _cfg: &RuntimeConfig,
        _body: &FrozenPostBody,
    ) -> PostAttemptOutcome<PostedMessage> {
        unreachable!("routed fixtures do not post locally")
    }
}

impl SlackClient for IdentitySequenceClient {
    fn open_socket(&self, _cfg: &RuntimeConfig) -> Result<String, SlackApiError> {
        unreachable!("identity-only test client")
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
        user_id: &str,
    ) -> Result<Option<VerifiedSlackHuman>, SlackApiError> {
        self.results
            .lock()
            .expect("lock identity sequence")
            .pop_front()
            .expect("scripted identity result")
            .map(|human| test_verified_human(user_id, human))
    }

    fn post_message(
        &self,
        _cfg: &RuntimeConfig,
        _body: &FrozenPostBody,
    ) -> PostAttemptOutcome<PostedMessage> {
        unreachable!("identity-only test client")
    }
}

fn cfg() -> RuntimeConfig {
    let policy = ConversationPolicy {
        alias: "team".to_owned(),
        conversation_id: "C123".to_owned(),
        kind: ConversationPolicyKind::Channel,
        receive: Some(ReceiveMode::MentionsOnly),
        description: None,
        thread_ts: None,
    };
    RuntimeConfig {
        app_token: "xapp-test".to_owned(),
        bot_token: "xoxb-test".to_owned(),
        allowed_user_ids: ["U123".to_owned()].into_iter().collect(),
        sender_aliases: HashMap::new(),
        security_mode: SecurityMode::Strict,
        conversations: [("team".to_owned(), policy.clone())].into_iter().collect(),
        parent_receives: [("C123".to_owned(), policy.alias.clone())]
            .into_iter()
            .collect(),
        thread_receives: HashMap::new(),
        proactive_aliases: BTreeSet::new(),
        dynamic_direct_messages: None,
        prefix_agent_id: false,
        api_base: DEFAULT_API_BASE.to_owned(),
        max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
    }
}

fn dm_cfg() -> RuntimeConfig {
    RuntimeConfig {
        conversations: BTreeMap::new(),
        parent_receives: HashMap::new(),
        thread_receives: HashMap::new(),
        dynamic_direct_messages: Some(DynamicDirectMessages {
            receive: ReceiveMode::AllMessages,
        }),
        ..cfg()
    }
}

fn fixed_thread_cfg(kind: ConversationPolicyKind, conversation_id: &str) -> RuntimeConfig {
    let mut config = cfg();
    config.conversations.clear();
    config.conversations.insert(
        "fixed".to_owned(),
        ConversationPolicy {
            alias: "fixed".to_owned(),
            conversation_id: conversation_id.to_owned(),
            kind,
            receive: Some(if kind == ConversationPolicyKind::Dm {
                ReceiveMode::AllMessages
            } else {
                ReceiveMode::MentionsOnly
            }),
            description: None,
            thread_ts: Some("7.0".to_owned()),
        },
    );
    reindex_receive_routes(&mut config);
    config
}

fn reindex_receive_routes(config: &mut RuntimeConfig) {
    config.parent_receives.clear();
    config.thread_receives.clear();
    for policy in config
        .conversations
        .values()
        .filter(|policy| policy.receive.is_some())
    {
        if let Some(root) = &policy.thread_ts {
            config.thread_receives.insert(
                (policy.conversation_id.clone(), root.clone()),
                policy.alias.clone(),
            );
        } else {
            config
                .parent_receives
                .insert(policy.conversation_id.clone(), policy.alias.clone());
        }
    }
}

fn proactive_cfg() -> RuntimeConfig {
    let destinations: BTreeMap<String, ConversationPolicy> = [
        ("team-ops", "C456", ConversationPolicyKind::Channel, None),
        (
            "incident-thread",
            "G789",
            ConversationPolicyKind::Mpim,
            Some("1720000000.123456"),
        ),
        ("alice-dm", "D123", ConversationPolicyKind::Dm, None),
    ]
    .into_iter()
    .map(|(alias, conversation_id, kind, thread_ts)| {
        (
            alias.to_owned(),
            ConversationPolicy {
                alias: alias.to_owned(),
                conversation_id: conversation_id.to_owned(),
                kind,
                receive: None,
                description: (alias == "team-ops").then(|| "Trusted ops hint".to_owned()),
                thread_ts: thread_ts.map(str::to_owned),
            },
        )
    })
    .collect();
    RuntimeConfig {
        proactive_aliases: destinations.keys().cloned().collect(),
        conversations: cfg()
            .conversations
            .into_iter()
            .chain(destinations)
            .collect(),
        ..cfg()
    }
}

fn agent_id(text: &str) -> AgentId {
    AgentId::parse(text).expect("agent id")
}

fn tool(name: &str, agent: &str, args: CborValue) -> ToolStarted {
    ToolStarted {
        call_id: format!("call-{name}").into(),
        tool_name: tau_proto::ToolName::new(name),
        arguments: args,
        agent_id: agent_id(agent),
        originator: tau_proto::PromptOriginator::User,
    }
}

fn bool_args(value: bool) -> CborValue {
    CborValue::Map(vec![(
        CborValue::Text("enabled".to_owned()),
        CborValue::Bool(value),
    )])
}

fn message_args(value: &str) -> CborValue {
    CborValue::Map(vec![
        (
            CborValue::Text("message".to_owned()),
            CborValue::Text(value.to_owned()),
        ),
        (
            CborValue::Text("reply_to".to_owned()),
            CborValue::Text("msg-test".to_owned()),
        ),
    ])
}

/// Build strict outbound reaction arguments.
fn reaction_args(message_ref: &str, emoji: &str, action: &str) -> CborValue {
    CborValue::Map(vec![
        example_field("message_ref", example_text(message_ref)),
        example_field("emoji", example_text(emoji)),
        example_field("action", example_text(action)),
    ])
}

/// Create an invocation with an explicit call id for replay tests.
fn tool_call(name: &str, agent: &str, call_id: &str, args: CborValue) -> ToolStarted {
    let mut invoke = tool(name, agent, args);
    invoke.call_id = call_id.into();
    invoke
}

fn valid_config_message() -> HarnessOutputMessage {
    let mut secrets = BTreeMap::new();
    secrets.insert("app".to_owned(), tau_proto::SecretValue::new("xapp-test"));
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("xoxb-test"));
    HarnessOutputMessage::Configure(tau_proto::Configure {
        tool_prefix: None,
        instance_name: tau_proto::ExtensionName::parse("test-extension")
            .expect("test extension name must satisfy the identifier grammar"),
        config: tau_proto::json_to_cbor(&serde_json::json!({
            "app_token_secret": "app",
            "bot_token_secret": "bot",
            "allowed_user_ids": ["U123"],
            "conversations": [{
                "alias": "team",
                "conversation_id": "C123",
                "kind": "channel",
                "receive": "mentions_only"
            }],
            "api_base": "http://127.0.0.1:8080/api",
            "max_message_bytes": 16384,
        })),
        state_dir: None,
        secrets,
        settings_files: Default::default(),
    })
}

fn proactive_config_message() -> HarnessOutputMessage {
    let HarnessOutputMessage::Configure(mut configure) = valid_config_message() else {
        unreachable!("config helper")
    };
    configure.config = tau_proto::json_to_cbor(&serde_json::json!({
        "app_token_secret": "app",
        "bot_token_secret": "bot",
        "allowed_user_ids": ["U123"],
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
    }));
    HarnessOutputMessage::Configure(configure)
}

fn malformed_config_message() -> HarnessOutputMessage {
    HarnessOutputMessage::Configure(tau_proto::Configure {
        tool_prefix: None,
        instance_name: tau_proto::ExtensionName::parse("test-extension")
            .expect("test extension name must satisfy the identifier grammar"),
        config: tau_proto::json_to_cbor(&serde_json::json!({
            "unknown_field": true,
        })),
        state_dir: None,
        secrets: BTreeMap::new(),
        settings_files: Default::default(),
    })
}

/// Two configured Slack instances expose disjoint structural names while
/// retaining semantic Slack tags.
#[test]
fn generic_prefixes_scope_slack_instances() {
    for prefix in ["personal", "work"] {
        let HarnessOutputMessage::Configure(mut configure) = valid_config_message() else {
            unreachable!()
        };
        configure.tool_prefix = Some(tau_proto::ToolNamePrefix::parse(prefix).expect("prefix"));
        let frames = run_protocol_messages(
            &[
                HarnessOutputMessage::Configure(configure),
                HarnessOutputMessage::Disconnect(Default::default()),
            ],
            FakeClient::new(),
        );
        let registrations = frames
            .iter()
            .filter_map(|frame| match frame {
                HarnessInputMessage::Emit(emit) => match emit.event.as_ref() {
                    Event::ToolRegistrationDeclared(register) => Some(register),
                    _ => None,
                },
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(registrations.iter().any(|registration| {
            registration.tool.name.as_str() == format!("{prefix}_{REGISTER_TOOL_NAME}")
                && registration.tool_group.as_ref().is_some_and(|group| {
                    group.name.as_str() == format!("{prefix}_{TOOL_GROUP_NAME}")
                })
                && registration
                    .tool
                    .tags
                    .iter()
                    .any(|tag| tag.as_str() == REGISTER_TOOL_TAG)
        }));
        assert!(registrations.iter().any(|registration| {
            registration.tool.name.as_str() == format!("{prefix}_{REACT_TOOL_NAME}")
                && !registration.tool.enabled_by_default
                && registration.tool_group.as_ref().is_some_and(|group| {
                    group.name.as_str() == format!("{prefix}_{TOOL_GROUP_NAME}")
                })
                && registration
                    .tool
                    .tags
                    .iter()
                    .any(|tag| tag.as_str() == REACT_TOOL_TAG)
                && registration
                    .tool
                    .description
                    .as_deref()
                    .is_some_and(|description| {
                        description.contains(&format!("{prefix}_{SEND_TOOL_NAME}"))
                    })
        }));
        assert!(registrations.iter().any(|registration| {
            registration.tool.name.as_str() == format!("{prefix}_{SEND_TOOL_NAME}")
                && registration
                    .tool
                    .description
                    .as_deref()
                    .is_some_and(|description| {
                        description.contains(&format!("{prefix}_{CONVERSATIONS_TOOL_NAME}"))
                    })
        }));
        assert!(registrations.iter().any(|registration| {
            registration.tool.name.as_str() == format!("{prefix}_{CONVERSATIONS_TOOL_NAME}")
                && registration.tool_group.as_ref().is_some_and(|group| {
                    group.name.as_str() == format!("{prefix}_{TOOL_GROUP_NAME}")
                })
                && registration
                    .tool
                    .tags
                    .iter()
                    .any(|tag| tag.as_str() == CONVERSATIONS_TOOL_TAG)
                && registration
                    .tool
                    .description
                    .as_deref()
                    .is_some_and(|description| {
                        description.contains(&format!("{prefix}_{SEND_TOOL_NAME}"))
                    })
        }));
    }
}

fn run_protocol_messages(
    messages: &[HarnessOutputMessage],
    client: Arc<FakeClient>,
) -> Vec<HarnessInputMessage> {
    let mut input = Vec::new();
    let mut writer = tau_proto::HarnessOutputWriter::new(&mut input);
    for message in messages {
        writer.write_message(message).expect("write input");
    }
    writer.flush().expect("flush input");

    let output = SharedWriter::default();
    let written = output.clone();
    run_with_client(path_std_io::Cursor::new(input), output, client).expect("run");

    let mut frames = Vec::new();
    let mut reader = tau_proto::HarnessInputReader::new(path_std_io::Cursor::new(written.bytes()));
    while let Some(frame) = reader.read_message().expect("read output") {
        frames.push(frame);
    }
    frames
}

fn extension() -> (
    Extension,
    mpsc::Receiver<HarnessInputMessage>,
    Arc<FakeClient>,
) {
    let (tx, rx) = mpsc::channel();
    let client = FakeClient::new();
    let ext = Extension::new_with_reaction_client(client.clone(), client.clone(), tx);
    ext.apply_config(cfg()).expect("config");
    {
        let mut state = ext.state.lock().expect("lock");
        state.bot_user_id = Some("UBOT123".to_owned());
        state.installation_team_id = Some("T123".to_owned());
        state.instance_name = Some(test_extension_name("std-slack"));
    }
    (ext, rx, client)
}

/// Capture the lifecycle fields attached to one ACK-admitted Socket occurrence.
fn admission_context(ext: &Extension) -> AdmissionContext {
    let state = ext.state.lock().expect("state");
    AdmissionContext {
        trace: LatencyTrace {
            connection_generation: 1,
            trace_seq: 1,
            event_class: EventClass::Delete,
        },
        received_at: Instant::now(),
        ingress_epoch: state.ingress_epoch,
        config_generation: state.config_generation,
        agent_generation: state.agent_generation,
        installation_team_id: state
            .installation_team_id
            .clone()
            .expect("installation identity"),
        queue_wait_us: 0,
        identity_us: Cell::new(0),
        outcome: Cell::new(AdmissionOutcome::RejectedRoute),
        permit: RefCell::new(None),
    }
}

fn slack_message(channel_id: &str, channel_type: Option<&str>, text: &str) -> SlackMessage {
    use std::hash::{Hash, Hasher};
    let mut hasher = path_std_collections_hash_map::DefaultHasher::new();
    (channel_id, text).hash(&mut hasher);
    let ts = format!("{}.0", hasher.finish());
    SlackMessage {
        event_id: Some(format!("EV-{channel_id}-{text}")),
        channel_id: channel_id.to_owned(),
        channel_type: Some(channel_type.unwrap_or("channel").to_owned()),
        user_id: "U123".to_owned(),
        text: text.to_owned(),
        event_type: if channel_type == Some("im") {
            "message"
        } else {
            "app_mention"
        }
        .to_owned(),
        subtype: None,
        bot_id: None,
        ts: Some(ts),
        thread_ts: None,
    }
}

/// Exact own-bot entities normalize only outside complete equal-length
/// backtick code ranges; unmatched delimiters remain ordinary text.
#[test]
fn transport_mention_normalization_is_exact_and_code_aware() {
    let cases = [
        ("<@UBOT123> hello", "hello", true),
        (
            "hello <@UBOT123> <@UBOT123>",
            "hello @slack_bridge @slack_bridge",
            false,
        ),
        ("hello <@UBOT123>", "hello @slack_bridge", false),
        ("<@UBOT123><@UBOT123>", "@slack_bridge", true),
        ("`<@UBOT123>`", "`<@UBOT123>`", false),
        ("``<@UBOT123>``", "``<@UBOT123>``", false),
        (
            "```text\n<@UBOT123>\n``` after <@UBOT123>",
            "```text\n<@UBOT123>\n``` after @slack_bridge",
            false,
        ),
        ("` unmatched <@UBOT123>", "` unmatched @slack_bridge", false),
        ("&lt;@UBOT123&gt;", "&lt;@UBOT123&gt;", false),
        ("<@UOTHER>", "<@UOTHER>", false),
        ("<@UBOT123|bridge>", "<@UBOT123|bridge>", false),
        ("<@UBOT123", "<@UBOT123", false),
        ("<@ubot123>", "<@ubot123>", false),
        ("<@UBOT12>", "<@UBOT12>", false),
        ("＠slack_bridge", "＠slack_bridge", false),
        ("@slack_bridge", "@slack_bridge", false),
    ];
    for (input, expected, leading) in cases {
        let normalized = normalize_transport_mentions(input, "UBOT123");
        assert_eq!(normalized.text, expected, "{input}");
        assert_eq!(normalized.leading, leading, "{input}");
    }
}

fn dual_slack_messages(text: &str) -> (SlackMessage, SlackMessage) {
    let mut message = slack_message("C123", Some("channel"), text);
    message.event_type = "message".to_owned();
    let mut mention = message.clone();
    mention.event_type = "app_mention".to_owned();
    mention.event_id = Some(format!(
        "mention-{}",
        message.ts.as_deref().unwrap_or("none")
    ));
    (message, mention)
}

fn slack_conversation(channel_id: &str, thread_ts: Option<&str>) -> SlackConversation {
    SlackConversation {
        channel_id: channel_id.to_owned(),
        thread_ts: thread_ts.map(str::to_owned),
        kind: ConversationPolicyKind::Channel,
        alias: "team".to_owned(),
    }
}

/// Dynamic DM routes have no configured alias, so incoming and sent projections
/// must omit `conversation` rather than expose the internal fallback label.
#[test]
fn dynamic_dm_message_facts_omit_synthesized_conversation_alias() {
    let conversation = SlackConversation {
        channel_id: "D123".to_owned(),
        thread_ts: None,
        kind: ConversationPolicyKind::Dm,
        alias: DYNAMIC_DM_LABEL.to_owned(),
    };
    let metadata = message_fact_conversation(&conversation);
    assert_eq!(metadata.alias, None);
    let fact = Event::MessageDelivered(MessageDelivered::new(
        tau_proto::MessagePublisherId::parse("std-slack")
            .expect("canonical publisher id must satisfy the identifier grammar"),
        MessageAgentTarget::new("agent-a"),
        slack_message_fact_id("D123", "1.0"),
        MessageParty {
            stable_id: slack_sender_ref("T123", "U123"),
            display_name: None,
            sender_auth: Some(MessageSenderAuth::VerifiedAllowlisted),
        },
        Some(metadata),
        "hello",
    ));
    let projection = tau_proto::project_message_fact(&fact)
        .expect("message fact")
        .expect("valid projection");
    let (ContentPart::Text { text } | ContentPart::HarnessInternalText { text }) =
        &projection.item.content[0];
    assert!(!text.contains(" conversation="), "{text}");
    assert!(!text.contains(DYNAMIC_DM_LABEL), "{text}");

    let sent = Event::MessageSent(MessageSent::new(
        tau_proto::MessagePublisherId::parse("std-slack")
            .expect("canonical publisher id must satisfy the identifier grammar"),
        MessageAgentTarget::new("agent-a"),
        slack_message_fact_id("D123", "2.0"),
        Some(MessageParty {
            stable_id: slack_sender_ref("T123", "U123"),
            display_name: None,
            sender_auth: None,
        }),
        Some(message_fact_conversation(&conversation)),
        "reply",
    ));
    let projection = tau_proto::project_message_fact(&sent)
        .expect("message fact")
        .expect("valid projection");
    let (ContentPart::Text { text } | ContentPart::HarnessInternalText { text }) =
        &projection.item.content[0];
    assert!(!text.contains(" conversation="), "{text}");
    assert!(!text.contains(DYNAMIC_DM_LABEL), "{text}");
}

/// Slack references are stable opaque values whose visible spelling neither
/// contains native coordinates nor collides across relevant identity inputs.
#[test]
fn slack_prompt_references_are_opaque_and_domain_separated() {
    let message = slack_message_fact_id("C-NATIVE-CHANNEL", "native.timestamp");
    assert_eq!(
        message,
        slack_message_fact_id("C-NATIVE-CHANNEL", "native.timestamp")
    );
    assert_ne!(
        message,
        slack_message_fact_id("C-OTHER-CHANNEL", "native.timestamp")
    );
    assert_ne!(
        message,
        slack_message_fact_id("C-NATIVE-CHANNEL", "other.timestamp")
    );
    assert!(message.as_str().starts_with("slack-message:"));
    assert!(message.as_str().len() <= 128);
    assert!(!message.as_str().contains("NATIVE"));
    assert!(!message.as_str().contains("timestamp"));

    let sender = slack_sender_ref("T123", "U123");
    assert_eq!(sender, slack_sender_ref("T123", "U123"));
    assert_ne!(sender, slack_sender_ref("T999", "U123"));
    assert_ne!(sender, slack_sender_ref("T123", "U999"));
    assert!(sender.starts_with("slack-sender:"));
    assert_eq!(sender.len(), "slack-sender:".len() + 64);
}

fn slack_reaction(
    event_id: &str,
    event_type: &str,
    channel_id: &str,
    message_ts: &str,
) -> SlackReaction {
    SlackReaction {
        event_id: Some(event_id.to_owned()),
        event_type: if event_type == "reaction_added" {
            ReactionKind::Added
        } else {
            ReactionKind::Removed
        },
        user_id: "U123".to_owned(),
        reaction: "thumbsup".to_owned(),
        channel_id: channel_id.to_owned(),
        message_ts: message_ts.to_owned(),
        thread_ts: None,
    }
}

fn slack_edit(
    event_id: &str,
    channel_id: &str,
    message_ts: &str,
    thread_ts: Option<&str>,
    text: &str,
) -> SlackEdit {
    SlackEdit {
        event_id: Some(event_id.to_owned()),
        channel_id: channel_id.to_owned(),
        editor_user_id: "U123".to_owned(),
        text: text.to_owned(),
        message_ts: message_ts.to_owned(),
        thread_ts: thread_ts.map(str::to_owned),
        revision_ts: "2.0".to_owned(),
    }
}

fn register_agent(ext: &Extension, agent: &str) {
    {
        let mut state = ext.state.lock().expect("lock");
        state.registered_agents.insert(agent_id(agent));
        state.agent_labels.insert(agent_id(agent), agent.to_owned());
    }
}

fn apply_test_config(ext: &Extension, mut config: RuntimeConfig) {
    reindex_receive_routes(&mut config);
    ext.apply_config(config).expect("test config");
    let mut state = ext.state.lock().expect("state");
    state.bot_user_id = Some("UBOT123".to_owned());
    state.installation_team_id = Some("T123".to_owned());
    state
        .instance_name
        .get_or_insert_with(|| test_extension_name("std-slack"));
}

/// Return sorted text keys from one structured CBOR object.
fn cbor_map_keys(value: &CborValue) -> Vec<&str> {
    let CborValue::Map(fields) = value else {
        panic!("expected object, got {value:?}");
    };
    let mut keys = fields
        .iter()
        .map(|(key, _)| match key {
            CborValue::Text(key) => key.as_str(),
            other => panic!("expected text key, got {other:?}"),
        })
        .collect::<Vec<_>>();
    keys.sort_unstable();
    keys
}

/// Return aliases from one discovery result page.
fn discovery_aliases(value: &CborValue) -> Vec<String> {
    tau_proto::cbor_array_field(value, "conversations")
        .expect("conversation array")
        .iter()
        .map(|record| tau_proto::cbor_text_field(record, "alias").expect("alias"))
        .collect()
}

/// Receive the next submitted delivered-message report text.
fn recv_prompt(rx: &mpsc::Receiver<HarnessInputMessage>) -> String {
    loop {
        if let HarnessInputMessage::Emit(emit) = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("message report")
            && {
                assert!(!emit.persist);
                true
            }
            && let Event::MessageDeliveredReported(report) = *emit.event
        {
            return report.text;
        }
    }
}

/// Receive the next transient message report.
fn recv_message_report(rx: &mpsc::Receiver<HarnessInputMessage>, expected: &str) -> Event {
    loop {
        if let HarnessInputMessage::Emit(emit) = rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap_or_else(|error| panic!("expected {expected} report: {error}"))
            && matches!(
                emit.event.as_ref(),
                Event::MessageDeliveredReported(_)
                    | Event::MessageEditedReported(_)
                    | Event::MessageDeletedReported(_)
                    | Event::MessageReactionAddedReported(_)
                    | Event::MessageReactionRemovedReported(_)
            )
        {
            assert!(!emit.persist);
            return *emit.event;
        }
    }
}

/// Convert one submitted Slack report through the real canonical payload
/// transformation and deliver it on the extension's production live-event path.
fn acknowledge_message_report(ext: &Extension, report: &Event) -> Event {
    let canonical = report
        .clone()
        .into_stamped_canonical_message_fact(
            tau_proto::MessagePublisherId::parse("std-slack").expect("publisher"),
        )
        .expect("matching Slack report canonicalizes");
    ext.apply_live_event(&canonical);
    canonical
}

/// Build one pending delivered report that owns a real admission permit.
fn pending_report_fixture() -> (
    Extension,
    Event,
    MessageFactId,
    Arc<AdmissionQueue<AdmissionWork>>,
) {
    let (ext, rx, _client) = extension();
    register_agent(&ext, "agent-a");
    ext.state.lock().expect("state").session_active = true;
    let queue = AdmissionQueue::<AdmissionWork>::new();
    let context = admission_context(&ext);
    context
        .permit
        .borrow_mut()
        .replace(queue.retain_test_permit());
    ext.process_slack_message_admitted(
        slack_message("C123", Some("channel"), "<@UBOT123> hello"),
        Some(&context),
    );
    let report = recv_message_report(&rx, "pending delivered");
    let Event::MessageDeliveredReported(delivered) = &report else {
        panic!("expected delivered report");
    };
    let message_id = delivered.message_id.clone();
    let canonical = report
        .into_stamped_canonical_message_fact(
            tau_proto::MessagePublisherId::parse("std-slack").expect("publisher"),
        )
        .expect("canonical delivered fact");
    (ext, canonical, message_id, queue)
}

/// Verify one teardown releases pending capacity and rejects a delayed echo.
fn assert_pending_teardown(name: &str, teardown: impl FnOnce(&Extension)) {
    let (ext, canonical, message_id, queue) = pending_report_fixture();
    let held = (1..admission::CAPACITY)
        .map(|_| queue.reserve().expect("remaining admission slot"))
        .collect::<Vec<_>>();
    assert!(matches!(queue.reserve(), Err(ReserveError::Full)));
    teardown(&ext);
    assert!(
        ext.state.lock().expect("state").pending_ingress.is_empty(),
        "{name} must clear pending ingress"
    );
    let released = queue.reserve().expect("teardown releases pending slot");
    ext.apply_live_event(&canonical);
    assert!(
        !ext.state
            .lock()
            .expect("state")
            .reply_routes
            .contains_key(&message_id)
    );
    drop(released);
    drop(held);
}

/// Lifecycle action raced against one gate-held ingress report submission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PendingTeardownRace {
    /// Harness transport disconnect.
    Disconnect,
    /// Session shutdown event.
    SessionShutdown,
    /// Agent unload event.
    AgentUnload,
    /// Fatal protocol writer retirement.
    WriterFailure,
}

/// Prove one teardown cannot pass the report output/lifecycle barrier.
fn assert_teardown_waits_for_ingress_output(action: PendingTeardownRace, replay: bool) {
    let (ext, rx, _client) = extension();
    register_agent(&ext, "agent-a");
    let message = slack_message("C123", Some("channel"), "<@UBOT123> hello");
    if replay {
        ext.process_slack_message(message.clone());
        let _original = recv_message_report(&rx, "original delivered");
    }
    let (reached_tx, reached_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let hook = Some(BlockingTestHook {
        reached: reached_tx,
        release: release_rx,
    });
    if replay {
        *ext.test_hooks.ingress_replay_boundary.lock().expect("hook") = hook;
    } else {
        *ext.test_hooks
            .ingress_submission_boundary
            .lock()
            .expect("hook") = hook;
    }
    let ext = Arc::new(ext);
    let submitting = Arc::clone(&ext);
    let worker = std::thread::spawn(move || submitting.process_slack_message(message));
    reached_rx.recv().expect("pending submission boundary");
    let (done_tx, done_rx) = mpsc::channel();
    let (attempt_tx, attempt_rx) = mpsc::channel();
    *ext.test_hooks
        .lifecycle_gate_attempt
        .lock()
        .expect("gate hook") = Some(attempt_tx);
    let retiring = Arc::clone(&ext);
    let teardown = std::thread::spawn(move || {
        match action {
            PendingTeardownRace::Disconnect => retiring.retire_send_authority(),
            PendingTeardownRace::SessionShutdown => {
                retiring.apply_live_event(&Event::SessionShutdown(tau_proto::SessionShutdown {
                    session_id: "s1".parse().expect("session id"),
                }));
            }
            PendingTeardownRace::AgentUnload => {
                retiring.unload_agent(&agent_id("agent-a"));
            }
            PendingTeardownRace::WriterFailure => retiring.retire_after_output_failure(),
        }
        done_tx.send(()).expect("teardown completion");
    });
    attempt_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("teardown reached gate acquisition");
    assert!(
        done_rx.try_recv().is_err(),
        "a gate acquisition attempt cannot finish while output owns the gate (replay={replay})"
    );
    release_tx.send(()).expect("release report submission");
    worker.join().expect("submission worker");
    done_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("teardown finishes after report submission");
    teardown.join().expect("teardown worker");
    if action == PendingTeardownRace::WriterFailure {
        assert!(rx.try_recv().is_err());
    } else {
        let _report = recv_message_report(&rx, "serialized delivered");
    }
    assert!(
        ext.state.lock().expect("state").pending_ingress.is_empty(),
        "{action:?} must clear pending ingress (replay={replay})"
    );
}

/// Assert that no message report is queued.
fn assert_no_ingress(rx: &mpsc::Receiver<HarnessInputMessage>) {
    while let Ok(message) = rx.try_recv() {
        if let HarnessInputMessage::Emit(emit) = message {
            assert!(
                !matches!(
                    emit.event.as_ref(),
                    Event::MessageDeliveredReported(_)
                        | Event::MessageEditedReported(_)
                        | Event::MessageDeletedReported(_)
                        | Event::MessageReactionAddedReported(_)
                        | Event::MessageReactionRemovedReported(_)
                ),
                "unexpected message report"
            );
        }
    }
}

/// Slack submits create, edit, reaction, and delete occurrences as reports that
/// retain one publisher-scoped target identity.
#[test]
fn message_report_lifecycle_preserves_target_identity() {
    let (ext, rx, _client) = extension();
    register_agent(&ext, "agent-a");
    let message = slack_message("C123", Some("channel"), "<@UBOT123> hello");
    let native_id = message.ts.clone().expect("native message id");

    ext.process_slack_message(message);
    let Event::MessageDeliveredReported(delivered) = recv_message_report(&rx, "delivered") else {
        panic!("expected delivered report");
    };
    assert_eq!(delivered.publisher_extension_id.as_str(), "std-slack");
    assert_eq!(delivered.text, "hello");
    assert_eq!(
        delivered.message_id.as_str(),
        slack_message_fact_id("C123", &native_id).as_str()
    );
    assert_eq!(
        delivered.sender.sender_auth,
        Some(MessageSenderAuth::VerifiedAllowlisted)
    );
    assert!(delivered.sender.stable_id.starts_with("slack-sender:"));
    assert_eq!(
        delivered
            .conversation
            .as_ref()
            .and_then(|value| value.alias.as_deref()),
        Some("team")
    );
    {
        let state = ext.state.lock().expect("state");
        assert!(!state.reply_routes.contains_key(&delivered.message_id));
        assert_eq!(state.pending_ingress.len(), 1);
    }
    acknowledge_message_report(&ext, &Event::MessageDeliveredReported(delivered.clone()));
    assert!(
        ext.state
            .lock()
            .expect("state")
            .reply_routes
            .contains_key(&delivered.message_id)
    );

    ext.process_slack_edit(slack_edit("edit-1", "C123", &native_id, None, "updated"));
    let Event::MessageEditedReported(edited) = recv_message_report(&rx, "edited") else {
        panic!("expected edited report");
    };
    assert_eq!(edited.target.message_id, delivered.message_id);
    assert_eq!(edited.text, "updated");
    assert_eq!(edited.publisher_extension_id.as_str(), "std-slack");
    acknowledge_message_report(&ext, &Event::MessageEditedReported(edited.clone()));

    ext.state.lock().expect("state").posted_messages.insert(
        PostedMessageKey::new("C123", "9.0"),
        PostedMessageOwner {
            agent_id: agent_id("agent-a"),
            message_id: delivered.message_id.clone(),
            thread_ts: None,
            conversation: slack_conversation("C123", None),
            installation_team_id: "T123".to_owned(),
        },
    );
    ext.process_slack_reaction(slack_reaction(
        "reaction-1",
        "reaction_added",
        "C123",
        "9.0",
    ));
    let Event::MessageReactionAddedReported(reaction) = recv_message_report(&rx, "reaction-added")
    else {
        panic!("expected reaction-added report");
    };
    assert_eq!(reaction.target.message_id, delivered.message_id);
    assert_eq!(reaction.reaction, "thumbsup");
    assert_eq!(reaction.publisher_extension_id.as_str(), "std-slack");
    acknowledge_message_report(&ext, &Event::MessageReactionAddedReported(reaction.clone()));
    ext.process_slack_reaction(slack_reaction(
        "reaction-2",
        "reaction_removed",
        "C123",
        "9.0",
    ));
    let Event::MessageReactionRemovedReported(removed) =
        recv_message_report(&rx, "reaction-removed")
    else {
        panic!("expected reaction-removed report");
    };
    assert_eq!(removed.target.message_id, delivered.message_id);
    assert_eq!(removed.publisher_extension_id.as_str(), "std-slack");
    acknowledge_message_report(
        &ext,
        &Event::MessageReactionRemovedReported(removed.clone()),
    );

    let reaction_key = ReactionKey {
        channel_id: "C123".to_owned(),
        message_ts: native_id.clone(),
        emoji: "eyes".to_owned(),
    };
    {
        let mut state = ext.state.lock().expect("state");
        state.reactions.owners.insert(
            reaction_key.clone(),
            ReactionOwner {
                agent_id: agent_id("agent-a"),
                message_ref: delivered.message_id.clone(),
            },
        );
        state.reactions.in_flight.insert(
            reaction_key,
            ReactionReservation {
                agent_id: agent_id("agent-a"),
                token: 7,
                message_ref: delivered.message_id.clone(),
                unowned_add: false,
            },
        );
    }
    ext.process_slack_delete(SlackDelete {
        event_id: Some("delete-1".to_owned()),
        channel_id: "C123".to_owned(),
        message_ts: native_id,
        thread_ts: None,
    });
    let Event::MessageDeletedReported(deleted) = recv_message_report(&rx, "deleted") else {
        panic!("expected deleted report");
    };
    assert_eq!(deleted.publisher_extension_id.as_str(), "std-slack");
    assert_eq!(deleted.target.message_id, delivered.message_id);
    assert!(deleted.actor.is_none());
    {
        let state = ext.state.lock().expect("state");
        assert!(!state.reply_routes.contains_key(&delivered.message_id));
        assert!(!state.reactions.targets.contains_key(&delivered.message_id));
        assert!(state.reactions.owners.is_empty());
        assert!(state.reactions.in_flight.is_empty());
        assert_eq!(state.pending_ingress.len(), 1);
    }
    acknowledge_message_report(&ext, &Event::MessageDeletedReported(deleted));
    assert!(ext.state.lock().expect("state").pending_ingress.is_empty());
}

/// Ingress authority requires exact event type, target agent, configured
/// publisher, message identity, and extension-generated report identity.
#[test]
fn ingress_canonical_correlation_rejects_every_mismatch() {
    let (ext, rx, _client) = extension();
    register_agent(&ext, "agent-a");
    ext.process_slack_message(slack_message("C123", Some("channel"), "<@UBOT123> hello"));
    let Event::MessageDeliveredReported(report) = recv_message_report(&rx, "delivered") else {
        panic!("expected delivered report");
    };
    let Event::MessageDelivered(canonical) = Event::MessageDeliveredReported(report.clone())
        .into_stamped_canonical_message_fact(
            tau_proto::MessagePublisherId::parse("std-slack").expect("publisher"),
        )
        .expect("canonical delivered fact")
    else {
        panic!("expected canonical delivered fact");
    };

    let mut wrong_publisher = canonical.clone();
    wrong_publisher.publisher_extension_id =
        tau_proto::MessagePublisherId::parse("other-slack").expect("publisher");
    ext.apply_live_event(&Event::MessageDelivered(wrong_publisher));
    let mut wrong_agent = canonical.clone();
    wrong_agent.agent_id = MessageAgentTarget::new("agent-b");
    ext.apply_live_event(&Event::MessageDelivered(wrong_agent));
    let mut wrong_message = canonical.clone();
    wrong_message.message_id = MessageFactId::new("slack-message:wrong");
    ext.apply_live_event(&Event::MessageDelivered(wrong_message));
    let mut wrong_report = canonical.clone();
    wrong_report.extension_data = SlackReportId::from_occurrence("wrong").extension_data();
    ext.apply_live_event(&Event::MessageDelivered(wrong_report));
    let mut wrong_type = MessageEdited::new(
        canonical.publisher_extension_id.clone(),
        canonical.agent_id.clone(),
        MessageFactRef {
            publisher_extension_id: tau_proto::RawMessagePublisherId::new("std-slack"),
            message_id: canonical.message_id.clone(),
        },
        None,
        canonical.conversation.clone(),
        "wrong type",
    );
    wrong_type.extension_data = canonical.extension_data.clone();
    ext.apply_live_event(&Event::MessageEdited(wrong_type));

    {
        let state = ext.state.lock().expect("state");
        assert_eq!(state.pending_ingress.len(), 1);
        assert!(!state.reply_routes.contains_key(&canonical.message_id));
    }
    ext.apply_live_event(&Event::MessageDelivered(canonical.clone()));
    let state = ext.state.lock().expect("state");
    assert!(state.pending_ingress.is_empty());
    assert!(state.reply_routes.contains_key(&canonical.message_id));
}

/// A report keeps its pre-ACK admission slot until canonical confirmation, so
/// missing echoes apply Socket Mode backpressure instead of dropping newer
/// input.
#[test]
fn pending_ingress_holds_admission_capacity_until_canonical_echo() {
    let (ext, rx, _client) = extension();
    register_agent(&ext, "agent-a");
    ext.state.lock().expect("state").session_active = true;
    let queue = AdmissionQueue::<AdmissionWork>::new();
    let context = admission_context(&ext);
    context
        .permit
        .borrow_mut()
        .replace(queue.retain_test_permit());
    ext.process_slack_message_admitted(
        slack_message("C123", Some("channel"), "<@UBOT123> hello"),
        Some(&context),
    );
    let report = recv_message_report(&rx, "delivered");
    let held = (1..admission::CAPACITY)
        .map(|_| queue.reserve().expect("remaining admission slot"))
        .collect::<Vec<_>>();
    assert!(matches!(queue.reserve(), Err(ReserveError::Full)));
    acknowledge_message_report(&ext, &report);
    let released = queue.reserve().expect("canonical echo releases slot");
    drop(released);
    drop(held);
}

/// Agent unload, disconnect, session shutdown, and fatal writer retirement all
/// release pending permits and make delayed canonical echoes inert.
#[test]
fn pending_ingress_teardown_releases_capacity_and_rejects_late_echoes() {
    assert_pending_teardown("agent unload", |ext| {
        ext.unload_agent(&agent_id("agent-a"));
    });
    assert_pending_teardown("disconnect", Extension::retire_send_authority);
    assert_pending_teardown("session shutdown", |ext| {
        ext.apply_live_event(&Event::SessionShutdown(tau_proto::SessionShutdown {
            session_id: "s1".parse().expect("session id"),
        }));
    });
    assert_pending_teardown("writer failure", Extension::retire_after_output_failure);
}

/// Disconnect, shutdown, unload, and fatal writer retirement cannot overtake a
/// gate-held new report or pending replay and permit stale post-retirement
/// output.
#[test]
fn ingress_submission_serializes_all_teardown_paths() {
    for action in [
        PendingTeardownRace::Disconnect,
        PendingTeardownRace::SessionShutdown,
        PendingTeardownRace::AgentUnload,
        PendingTeardownRace::WriterFailure,
    ] {
        assert_teardown_waits_for_ingress_output(action, false);
        assert_teardown_waits_for_ingress_output(action, true);
    }
}

/// Lax static-route ingress reports its existing verified conversation
/// admission without upgrading the sender to allowlisted.
#[test]
fn lax_static_ingress_reports_conversation_authorization() {
    let (ext, rx, _client) = extension();
    let mut config = cfg();
    config.security_mode = SecurityMode::Lax;
    apply_test_config(&ext, config);
    register_agent(&ext, "agent-a");
    let mut message = slack_message("C123", Some("channel"), "<@UBOT123> hello");
    message.user_id = "U999".to_owned();
    ext.process_slack_message(message);
    let Event::MessageDeliveredReported(delivered) = recv_message_report(&rx, "delivered") else {
        panic!("expected delivered report");
    };
    assert_eq!(
        delivered.sender.sender_auth,
        Some(MessageSenderAuth::VerifiedConversationAuthorized)
    );
}

/// Deleting a bridge-authored post submits against its sent report and revokes
/// all retained post and reaction authority.
#[test]
fn outgoing_message_delete_revokes_post_authority() {
    let (ext, rx, _client) = extension();
    let conversation = slack_conversation("C123", None);
    let message_id = slack_message_fact_id("C123", "9.0");
    ext.remember_posted_message(
        conversation.clone(),
        PostedMessage {
            channel_id: "C123".to_owned(),
            ts: "9.0".to_owned(),
            thread_ts: None,
        },
        agent_id("agent-a"),
    );
    {
        let mut state = ext.state.lock().expect("state");
        assert!(state.reactions.insert_target(
            message_id.clone(),
            ReactionTarget {
                agent_id: agent_id("agent-a"),
                conversation,
                message_ts: "9.0".to_owned(),
                installation_team_id: "T123".to_owned(),
                authority: ReactionAuthority::ConfiguredDestination {
                    alias: "team".to_owned(),
                },
            },
        ));
        state.reactions.owners.insert(
            ReactionKey {
                channel_id: "C123".to_owned(),
                message_ts: "9.0".to_owned(),
                emoji: "eyes".to_owned(),
            },
            ReactionOwner {
                agent_id: agent_id("agent-a"),
                message_ref: message_id.clone(),
            },
        );
    }

    ext.process_slack_delete(SlackDelete {
        event_id: Some("delete-outgoing".to_owned()),
        channel_id: "C123".to_owned(),
        message_ts: "9.0".to_owned(),
        thread_ts: None,
    });
    let Event::MessageDeletedReported(deleted) = recv_message_report(&rx, "outgoing deleted")
    else {
        panic!("expected deleted report");
    };
    assert_eq!(deleted.target.message_id, message_id);
    let state = ext.state.lock().expect("state");
    assert!(
        state
            .posted_messages
            .get(&PostedMessageKey::new("C123", "9.0"))
            .is_none()
    );
    assert!(!state.reactions.targets.contains_key(&message_id));
    assert!(state.reactions.owners.is_empty());
}

/// Unregistering receive authority preserves proactive post provenance so a
/// later Slack deletion can still revoke the sent target.
#[test]
fn outgoing_message_delete_survives_receive_unregister() {
    let (ext, rx, _client) = extension();
    register_agent(&ext, "agent-a");
    ext.remember_posted_message(
        slack_conversation("C123", None),
        PostedMessage {
            channel_id: "C123".to_owned(),
            ts: "10.0".to_owned(),
            thread_ts: None,
        },
        agent_id("agent-a"),
    );
    assert!(matches!(
        ext.handle_register(tool(REGISTER_TOOL_NAME, "agent-a", bool_args(false))),
        Event::ToolResult(_)
    ));
    assert!(
        ext.state
            .lock()
            .expect("state")
            .posted_messages
            .get(&PostedMessageKey::new("C123", "10.0"))
            .is_some()
    );

    ext.process_slack_delete(SlackDelete {
        event_id: Some("delete-after-unregister".to_owned()),
        channel_id: "C123".to_owned(),
        message_ts: "10.0".to_owned(),
        thread_ts: None,
    });
    let Event::MessageDeletedReported(deleted) =
        recv_message_report(&rx, "unregistered outgoing deleted")
    else {
        panic!("expected deleted report");
    };
    assert_eq!(
        deleted.target.message_id,
        slack_message_fact_id("C123", "10.0")
    );
}

/// Receive-registration generation changes after ACK do not invalidate exact
/// proactive post provenance, including changes for unrelated agents.
#[test]
fn admitted_outgoing_delete_ignores_receive_registration_churn() {
    let (ext, rx, _client) = extension();
    register_agent(&ext, "agent-a");
    ext.state.lock().expect("state").session_active = true;
    ext.remember_posted_message(
        slack_conversation("C123", None),
        PostedMessage {
            channel_id: "C123".to_owned(),
            ts: "11.0".to_owned(),
            thread_ts: None,
        },
        agent_id("agent-a"),
    );
    let admission = admission_context(&ext);
    assert!(matches!(
        ext.handle_register(tool(REGISTER_TOOL_NAME, "agent-a", bool_args(false))),
        Event::ToolResult(_)
    ));
    register_agent(&ext, "agent-b");

    ext.process_slack_delete_admitted(
        SlackDelete {
            event_id: Some("delete-after-agent-churn".to_owned()),
            channel_id: "C123".to_owned(),
            message_ts: "11.0".to_owned(),
            thread_ts: None,
        },
        Some(&admission),
    );
    let Event::MessageDeletedReported(deleted) =
        recv_message_report(&rx, "churned outgoing deleted")
    else {
        panic!("expected deleted report");
    };
    assert_eq!(
        deleted.target.message_id,
        slack_message_fact_id("C123", "11.0")
    );
}

/// An ACK-era incoming owner cannot submit a deletion report after unregister
/// removes its exact source ownership.
#[test]
fn admitted_incoming_delete_fails_closed_after_unregister() {
    let (ext, rx, _client) = extension();
    register_agent(&ext, "agent-a");
    let message = slack_message("C123", Some("channel"), "<@UBOT123> hello");
    let message_ts = message.ts.clone().expect("native Slack timestamp");
    ext.process_slack_message(message);
    let delivered = recv_message_report(&rx, "delivered");
    let Event::MessageDeliveredReported(_) = &delivered else {
        panic!("expected delivered report");
    };
    acknowledge_message_report(&ext, &delivered);
    assert!(
        ext.state
            .lock()
            .expect("state")
            .incoming_messages
            .contains_key(&PostedMessageKey::new("C123", &message_ts))
    );
    ext.state.lock().expect("state").session_active = true;
    let admission = admission_context(&ext);
    assert!(matches!(
        ext.handle_register(tool(REGISTER_TOOL_NAME, "agent-a", bool_args(false))),
        Event::ToolResult(_)
    ));

    ext.process_slack_delete_admitted(
        SlackDelete {
            event_id: Some("stale-incoming-delete".to_owned()),
            channel_id: "C123".to_owned(),
            message_ts,
            thread_ts: None,
        },
        Some(&admission),
    );
    assert_no_ingress(&rx);
}

/// Confirmed deletion-output failure enters the same synchronous fatal barrier
/// as send submission failure before any later remote effect can start.
#[test]
fn deletion_writer_failure_retires_all_remote_effect_authority() {
    let (output_tx, output_rx) = mpsc::channel();
    drop(output_rx);
    let client = FakeClient::new();
    let ext = Extension::new(client.clone(), output_tx);
    ext.apply_config(cfg()).expect("config");
    {
        let mut state = ext.state.lock().expect("state");
        state.bot_user_id = Some("UBOT123".to_owned());
        state.installation_team_id = Some("T123".to_owned());
        state.instance_name = Some(test_extension_name("std-slack"));
        state.session_active = true;
        state.registered_agents.insert(agent_id("agent-a"));
    }
    ext.remember_posted_message(
        slack_conversation("C123", None),
        PostedMessage {
            channel_id: "C123".to_owned(),
            ts: "12.0".to_owned(),
            thread_ts: None,
        },
        agent_id("agent-a"),
    );
    {
        let mut state = ext.state.lock().expect("state");
        assert!(state.reactions.insert_target(
            MessageFactId::new("slack-message:test-c123-12.0"),
            ReactionTarget {
                agent_id: agent_id("agent-a"),
                conversation: slack_conversation("C123", None),
                message_ts: "12.0".to_owned(),
                installation_team_id: "T123".to_owned(),
                authority: ReactionAuthority::ConfiguredDestination {
                    alias: "team".to_owned(),
                },
            },
        ));
    }

    ext.process_slack_delete(SlackDelete {
        event_id: Some("delete-writer-failure".to_owned()),
        channel_id: "C123".to_owned(),
        message_ts: "12.0".to_owned(),
        thread_ts: None,
    });

    assert!(ext.output_failed.load(Ordering::Acquire));
    assert!(ext.shutdown.is_requested());
    let state = ext.state.lock().expect("state");
    assert!(!state.session_active);
    assert!(state.send_ledger.is_empty());
    assert!(state.reply_routes.is_empty());
    assert!(state.reactions.targets.is_empty());
    assert!(state.reactions.in_flight.is_empty());
    drop(state);
    let result = ext.handle_send(tool(
        SEND_TOOL_NAME,
        "agent-a",
        tau_proto::json_to_cbor(
            &serde_json::json!({"message":"must not post","destination":"team"}),
        ),
    ));
    assert!(matches!(result, Some(Event::ToolError(_))));
    assert!(client.sent.lock().expect("sent").is_empty());
}

/// A reaction that returns after fatal output retirement cannot recreate target
/// ownership cleared by the barrier.
#[test]
fn deletion_writer_failure_rejects_late_reaction_completion() {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let reaction_client = Arc::new(BlockingReactionClient {
        started: Mutex::new(Some(started_tx)),
        release: Mutex::new(release_rx),
    });
    let (output_tx, output_rx) = mpsc::channel();
    drop(output_rx);
    let ext = Arc::new(Extension::new_with_reaction_client(
        FakeClient::new(),
        reaction_client,
        output_tx,
    ));
    ext.apply_config(proactive_cfg()).expect("config");
    {
        let mut state = ext.state.lock().expect("state");
        state.bot_user_id = Some("UBOT123".to_owned());
        state.installation_team_id = Some("T123".to_owned());
        state.instance_name = Some(test_extension_name("std-slack"));
        state.session_active = true;
        assert!(state.reactions.insert_target(
            MessageFactId::new("slack-message:test-c456-1.0"),
            ReactionTarget {
                agent_id: agent_id("agent-a"),
                conversation: SlackConversation {
                    alias: "team-ops".to_owned(),
                    channel_id: "C456".to_owned(),
                    kind: ConversationPolicyKind::Channel,
                    thread_ts: None,
                },
                message_ts: "1.0".to_owned(),
                installation_team_id: "T123".to_owned(),
                authority: ReactionAuthority::ConfiguredDestination {
                    alias: "team-ops".to_owned(),
                },
            },
        ));
    }
    ext.remember_posted_message(
        SlackConversation {
            alias: "team-ops".to_owned(),
            channel_id: "C456".to_owned(),
            kind: ConversationPolicyKind::Channel,
            thread_ts: None,
        },
        PostedMessage {
            channel_id: "C456".to_owned(),
            ts: "2.0".to_owned(),
            thread_ts: None,
        },
        agent_id("agent-a"),
    );

    let reacting = Arc::clone(&ext);
    let reaction = std::thread::spawn(move || {
        reacting.handle_react_event(tool(
            REACT_TOOL_NAME,
            "agent-a",
            tau_proto::json_to_cbor(
                &serde_json::json!({"message_ref":"slack-message:test-c456-1.0","emoji":"eyes","action":"add"}),
            ),
        ))
    });
    started_rx.recv().expect("reaction started");
    ext.process_slack_delete(SlackDelete {
        event_id: Some("fatal-delete-with-reaction".to_owned()),
        channel_id: "C456".to_owned(),
        message_ts: "2.0".to_owned(),
        thread_ts: None,
    });
    release_tx.send(()).expect("release reaction");
    assert!(matches!(
        reaction.join().expect("reaction worker"),
        Event::ToolError(_)
    ));
    let state = ext.state.lock().expect("state");
    assert!(state.reactions.targets.is_empty());
    assert!(state.reactions.owners.is_empty());
    assert!(state.reactions.in_flight.is_empty());
}

/// The early output-failure latch closes reaction and local-reply admission
/// even while fatal retirement is blocked behind an in-progress submission.
#[test]
fn output_failure_latch_blocks_new_remote_effects_before_retirement_lock() {
    let (output_tx, output_rx) = mpsc::channel();
    drop(output_rx);
    let client = FakeClient::new();
    let ext = Extension::new(client.clone(), output_tx);
    ext.apply_config(cfg()).expect("config");
    {
        let mut state = ext.state.lock().expect("state");
        state.bot_user_id = Some("UBOT123".to_owned());
        state.installation_team_id = Some("T123".to_owned());
        state.instance_name = Some(test_extension_name("std-slack"));
    }
    register_agent(&ext, "agent-a");
    let message_ref = "slack-message:test-c123-13.0";
    install_source_reaction_target(&ext, message_ref);
    ext.remember_posted_message(
        slack_conversation("C123", None),
        PostedMessage {
            channel_id: "C123".to_owned(),
            ts: "13.5".to_owned(),
            thread_ts: None,
        },
        agent_id("agent-a"),
    );
    let (reached_tx, reached_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    *ext.test_hooks.output_failure_boundary.lock().expect("hook") = Some(BlockingTestHook {
        reached: reached_tx,
        release: release_rx,
    });
    let ext = Arc::new(ext);
    let deleting = Arc::clone(&ext);
    let deletion = std::thread::spawn(move || {
        deleting.process_slack_delete(SlackDelete {
            event_id: Some("writer-failure-boundary".to_owned()),
            channel_id: "C123".to_owned(),
            message_ts: "13.5".to_owned(),
            thread_ts: None,
        });
    });
    reached_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("writer failure latched under gate");
    assert!(ext.output_failed.load(Ordering::Acquire));
    assert!(matches!(
        ext.handle_react_event(tool(
            REACT_TOOL_NAME,
            "agent-a",
            tau_proto::json_to_cbor(
                &serde_json::json!({"message_ref":message_ref,"emoji":"eyes","action":"add"}),
            ),
        )),
        Event::ToolError(_)
    ));
    ext.reply(&cfg(), "C123", None, "must not post", None);
    assert!(client.reactions.lock().expect("reactions").is_empty());
    assert!(client.sent.lock().expect("sent").is_empty());
    release_tx.send(()).expect("release writer failure");
    deletion.join().expect("deletion");
}

/// Session retirement while deletion waits at the submission boundary is
/// observed by the final owner/lifecycle validation and suppresses the report.
#[test]
fn delete_submission_gate_revalidates_after_session_retirement() {
    let (ext, rx, _client) = extension();
    ext.state.lock().expect("state").session_active = true;
    ext.remember_posted_message(
        slack_conversation("C123", None),
        PostedMessage {
            channel_id: "C123".to_owned(),
            ts: "14.0".to_owned(),
            thread_ts: None,
        },
        agent_id("agent-a"),
    );
    let admission = admission_context(&ext);
    let ext = Arc::new(ext);
    let (reached_tx, reached_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    *ext.test_hooks
        .delete_submission_boundary
        .lock()
        .expect("hook") = Some(BlockingTestHook {
        reached: reached_tx,
        release: release_rx,
    });
    let submission_gate = Arc::clone(&ext.output_submission_gate);
    let submission = submission_gate.lock().expect("submission gate");
    let deleting = Arc::clone(&ext);
    let deletion = std::thread::spawn(move || {
        deleting.process_slack_delete_admitted(
            SlackDelete {
                event_id: Some("delete-at-session-retirement".to_owned()),
                channel_id: "C123".to_owned(),
                message_ts: "14.0".to_owned(),
                thread_ts: None,
            },
            Some(&admission),
        );
    });
    reached_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("deletion reached submission boundary");
    {
        let mut state = ext.state.lock().expect("state");
        state.ingress_epoch = state.ingress_epoch.wrapping_add(1);
        state.session_active = false;
    }
    release_tx.send(()).expect("release deletion");
    drop(submission);
    deletion.join().expect("deletion");
    assert_no_ingress(&rx);
}

/// Lax mode cannot use an unlinked DM even to trigger validation replies or
/// bridge commands; an allowlisted user must establish the link first.
#[test]
fn lax_unlinked_dm_has_no_ingress_or_reply_side_effects() {
    let (ext, rx, client) = extension();
    ext.state.lock().expect("lock").config = Some(dm_cfg());
    ext.state
        .lock()
        .expect("lock")
        .config
        .as_mut()
        .expect("config")
        .security_mode = SecurityMode::Lax;
    register_agent(&ext, "agent-a");
    for text in ["", "start", "/select agent-a", "x"] {
        let mut message = slack_message("D999", Some("im"), text);
        message.user_id = "U999".to_owned();
        ext.process_slack_message(message);
    }
    assert_no_ingress(&rx);
    assert!(client.sent.lock().expect("lock").is_empty());
}

/// Ensures the shared shutdown signal wakes async waiters immediately,
/// preventing regressions to periodic shutdown polling in the Slack worker.
#[tokio::test]
async fn shutdown_signal_wait_wakes_after_request() {
    let shutdown = Arc::new(ShutdownSignal::new());
    let waiter_shutdown = Arc::clone(&shutdown);
    let waiter = tokio::spawn(async move {
        waiter_shutdown.wait().await;
    });

    tokio::task::yield_now().await;
    shutdown.request();

    tokio::time::timeout(Duration::from_millis(75), waiter)
        .await
        .expect("shutdown waiter should wake promptly")
        .expect("shutdown waiter should not panic");
}

/// Ensures reconnect backoff waits are interruptible by notification rather
/// than sleeping in fixed polling chunks for the full delay.
#[tokio::test]
async fn shutdown_signal_wait_timeout_wakes_before_long_backoff() {
    let shutdown = Arc::new(ShutdownSignal::new());
    let waiter_shutdown = Arc::clone(&shutdown);
    let waiter =
        tokio::spawn(async move { waiter_shutdown.wait_timeout(Duration::from_secs(60)).await });

    tokio::task::yield_now().await;
    shutdown.request();

    let interrupted = tokio::time::timeout(Duration::from_millis(75), waiter)
        .await
        .expect("backoff wait should wake promptly")
        .expect("backoff waiter should not panic");
    assert!(interrupted, "wait_timeout should report requested shutdown");
}

/// Drive one production Socket Mode worker from finite loopback server frames.
async fn socket_worker_result_for_frames(
    frames: Vec<Message>,
) -> (
    Result<WorkerOutcome, String>,
    mpsc::Receiver<HarnessInputMessage>,
) {
    let listener = path_tokio_net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback websocket listener");
    let socket_url = format!(
        "ws://{}/socket-ticket",
        listener.local_addr().expect("listener local address")
    );
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept websocket client");
        let mut ws = tokio_tungstenite::accept_async(stream)
            .await
            .expect("complete websocket handshake");
        for frame in frames {
            if ws.send(frame).await.is_err() {
                return;
            }
        }
        let _ = ws.send(Message::Close(None)).await;
    });
    let (output, receiver) = mpsc::channel();
    let ext = Extension::new(FakeClient::new(), output);
    let result = socket_worker_once(
        &ext,
        &cfg(),
        Some(WorkerStartup {
            bot_user_id: "UBOT123".to_owned(),
            installation_team_id: "T123".to_owned(),
            socket_url,
        }),
        &AdmissionQueue::new(),
        1,
    )
    .await;
    server.await.expect("loopback websocket server");
    (result, receiver)
}

/// Split one payload into two raw text frames to exercise production
/// reassembly.
fn fragmented_text_frames(payload: Vec<u8>) -> Vec<Message> {
    let midpoint = payload.len() / 2;
    vec![
        Message::Frame(Frame::message(
            payload[..midpoint].to_vec(),
            OpCode::Data(Data::Text),
            false,
        )),
        Message::Frame(Frame::message(
            payload[midpoint..].to_vec(),
            OpCode::Data(Data::Continue),
            true,
        )),
    ]
}

/// Build one valid Socket Mode hello envelope padded to an exact byte length.
fn padded_socket_hello(length: usize) -> String {
    let hello = r#"{"type":"hello"}"#;
    assert!(hello.len() <= length);
    let mut frame = String::with_capacity(length);
    frame.push_str(hello);
    frame.extend(std::iter::repeat_n(' ', length - hello.len()));
    frame
}

/// Tau owns its 256 KiB transport bound rather than accepting Tungstenite's
/// larger defaults for either individual frames or reassembled messages.
#[test]
fn socket_websocket_config_caps_frames_and_messages_at_256_kibibytes() {
    let config = socket_websocket_config();
    assert_eq!(config.max_frame_size, Some(MAX_SOCKET_FRAME_BYTES));
    assert_eq!(config.max_message_size, Some(MAX_SOCKET_FRAME_BYTES));
}

/// One raw text frame reaches the normal Socket Mode path at the exact limit,
/// while byte 256 KiB + 1 fails before decoding and retires the connection.
#[tokio::test]
async fn socket_worker_caps_unfragmented_text_at_first_excess_byte() {
    let (exact_result, exact_output) = socket_worker_result_for_frames(vec![Message::Text(
        padded_socket_hello(MAX_SOCKET_FRAME_BYTES).into(),
    )])
    .await;
    assert_eq!(
        exact_result.expect("exact frame limit"),
        WorkerOutcome::ReconnectNow
    );
    assert_no_ingress(&exact_output);

    let (excess_result, excess_output) = socket_worker_result_for_frames(vec![Message::Text(
        padded_socket_hello(MAX_SOCKET_FRAME_BYTES + 1).into(),
    )])
    .await;
    assert_eq!(
        excess_result.expect_err("first excess byte must retire the connection"),
        "Slack websocket frame failed"
    );
    assert_no_ingress(&excess_output);
}

/// Raw fragmented text remains accepted at an exact 256 KiB aggregate, while
/// the next byte fails during transport reassembly before Slack can decode it.
#[tokio::test]
async fn socket_worker_caps_fragmented_text_at_first_excess_byte() {
    let (exact_result, exact_output) = socket_worker_result_for_frames(fragmented_text_frames(
        padded_socket_hello(MAX_SOCKET_FRAME_BYTES).into_bytes(),
    ))
    .await;
    assert_eq!(
        exact_result.expect("exact complete-message limit"),
        WorkerOutcome::ReconnectNow
    );
    assert_no_ingress(&exact_output);

    let (excess_result, excess_output) = socket_worker_result_for_frames(fragmented_text_frames(
        padded_socket_hello(MAX_SOCKET_FRAME_BYTES + 1).into_bytes(),
    ))
    .await;
    assert_eq!(
        excess_result.expect_err("first aggregate excess byte must retire the connection"),
        "Slack websocket frame failed"
    );
    assert_no_ingress(&excess_output);
}

/// Binary messages share the production transport cap even though exact-size
/// binary payloads remain ignored by Slack's application-level frame handler.
#[tokio::test]
async fn socket_worker_caps_binary_at_first_excess_byte() {
    let (exact_result, exact_output) = socket_worker_result_for_frames(vec![Message::Binary(
        vec![b'x'; MAX_SOCKET_FRAME_BYTES].into(),
    )])
    .await;
    assert_eq!(
        exact_result.expect("exact binary frame limit"),
        WorkerOutcome::ReconnectNow
    );
    assert_no_ingress(&exact_output);

    let (excess_result, excess_output) = socket_worker_result_for_frames(vec![Message::Binary(
        vec![b'x'; MAX_SOCKET_FRAME_BYTES + 1].into(),
    )])
    .await;
    assert_eq!(
        excess_result.expect_err("first binary excess byte must retire the connection"),
        "Slack websocket frame failed"
    );
    assert_no_ingress(&excess_output);
}

/// Ensures a Socket Mode worker blocked on websocket receive exits promptly
/// when shutdown is requested, preserving shutdown latency without a receive
/// timeout.
#[tokio::test]
async fn socket_worker_once_shutdown_interrupts_idle_websocket_receive() {
    let listener = path_tokio_net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback websocket listener");
    let socket_url = format!(
        "ws://{}/socket-ticket",
        listener.local_addr().expect("listener local address")
    );
    let (accepted_tx, accepted_rx) = path_tokio_sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept websocket client");
        let _ws = tokio_tungstenite::accept_async(stream)
            .await
            .expect("complete websocket handshake");
        let _ = accepted_tx.send(());
        std::future::pending::<()>().await;
    });

    let (tx, _rx) = mpsc::channel();
    let ext = Extension::new(FakeClient::new(), tx);
    let shutdown = Arc::clone(&ext.shutdown);
    let worker_cfg = cfg();
    let worker = tokio::spawn(async move {
        socket_worker_once(
            &ext,
            &worker_cfg,
            Some(WorkerStartup {
                bot_user_id: "UBOT123".to_owned(),
                installation_team_id: "T123".to_owned(),
                socket_url,
            }),
            &AdmissionQueue::new(),
            1,
        )
        .await
    });

    accepted_rx.await.expect("websocket should connect");
    shutdown.request();

    let outcome = tokio::time::timeout(Duration::from_millis(150), worker)
        .await
        .expect("socket worker should stop promptly")
        .expect("socket worker should not panic")
        .expect("socket worker should exit cleanly");
    assert_eq!(outcome, WorkerOutcome::Shutdown);
    server.abort();
}

/// A peer that leaves its TCP connection open but never processes WebSocket
/// heartbeats must return a reconnectable error instead of silently losing
/// ingress forever.
#[tokio::test(start_paused = true)]
async fn socket_worker_once_reconnects_after_missing_heartbeat_pong() {
    let listener = path_tokio_net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback websocket listener");
    let socket_url = format!(
        "ws://{}/socket-ticket",
        listener.local_addr().expect("listener local address")
    );
    let (accepted_tx, accepted_rx) = path_tokio_sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept websocket client");
        let _ws = tokio_tungstenite::accept_async(stream)
            .await
            .expect("complete websocket handshake");
        let _ = accepted_tx.send(());
        std::future::pending::<()>().await;
    });

    let (tx, _rx) = mpsc::channel();
    let ext = Extension::new(FakeClient::new(), tx);
    let worker_cfg = cfg();
    let worker = tokio::spawn(async move {
        socket_worker_once_with_heartbeat(
            &ext,
            &worker_cfg,
            Some(WorkerStartup {
                bot_user_id: "UBOT123".to_owned(),
                installation_team_id: "T123".to_owned(),
                socket_url,
            }),
            &AdmissionQueue::new(),
            1,
            SocketHeartbeat {
                ping_interval: Duration::from_millis(10),
                pong_timeout: Duration::from_millis(40),
            },
        )
        .await
    });

    accepted_rx.await.expect("websocket should connect");
    let error = tokio::time::timeout(Duration::from_millis(250), worker)
        .await
        .expect("stale websocket should be detected promptly")
        .expect("socket worker should not panic")
        .expect_err("missing heartbeat pong must reconnect");
    assert_eq!(error, SOCKET_HEARTBEAT_TIMEOUT_ERROR);
    server.abort();
}

/// Regular Pong responses must keep an otherwise idle Socket Mode connection
/// alive beyond its stale deadline, proving the heartbeat does not force
/// periodic reconnects while the peer remains responsive.
#[tokio::test(start_paused = true)]
async fn socket_worker_once_keeps_responsive_idle_connection() {
    let listener = path_tokio_net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback websocket listener");
    let socket_url = format!(
        "ws://{}/socket-ticket",
        listener.local_addr().expect("listener local address")
    );
    let (accepted_tx, accepted_rx) = path_tokio_sync::oneshot::channel();
    let (responsive_tx, responsive_rx) = path_tokio_sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept websocket client");
        let mut ws = tokio_tungstenite::accept_async(stream)
            .await
            .expect("complete websocket handshake");
        let _ = accepted_tx.send(());
        let mut ping_count = 0;
        let mut responsive_tx = Some(responsive_tx);
        while let Some(frame) = ws.next().await {
            match frame.expect("read client heartbeat") {
                Message::Ping(payload) => {
                    ws.send(Message::Pong(payload))
                        .await
                        .expect("answer client heartbeat");
                    ping_count += 1;
                    if ping_count == 12 {
                        let _ = responsive_tx.take().expect("single signal").send(());
                        std::future::pending::<()>().await;
                    }
                }
                Message::Close(_) => return,
                other => panic!("unexpected client frame: {other:?}"),
            }
        }
    });

    let (tx, _rx) = mpsc::channel();
    let ext = Arc::new(Extension::new(FakeClient::new(), tx));
    let worker_ext = Arc::clone(&ext);
    let worker_cfg = cfg();
    let worker = tokio::spawn(async move {
        socket_worker_once_with_heartbeat(
            &worker_ext,
            &worker_cfg,
            Some(WorkerStartup {
                bot_user_id: "UBOT123".to_owned(),
                installation_team_id: "T123".to_owned(),
                socket_url,
            }),
            &AdmissionQueue::new(),
            1,
            SocketHeartbeat {
                ping_interval: Duration::from_millis(5),
                pong_timeout: Duration::from_millis(50),
            },
        )
        .await
    });

    accepted_rx.await.expect("websocket should connect");
    tokio::time::timeout(Duration::from_secs(1), responsive_rx)
        .await
        .expect("connection should survive beyond its stale deadline")
        .expect("server should observe enough heartbeats");
    ext.shutdown.request();
    let outcome = tokio::time::timeout(Duration::from_millis(250), worker)
        .await
        .expect("responsive worker should honor shutdown")
        .expect("socket worker should not panic")
        .expect("socket worker should exit cleanly");
    assert_eq!(outcome, WorkerOutcome::Shutdown);
    server.abort();
}

/// A Pong arriving between Ping phases resets one independent deadline exactly;
/// later non-Pong application traffic must neither refresh that deadline nor
/// leave the connection marked online after timeout.
#[tokio::test(start_paused = true)]
async fn socket_worker_once_times_out_from_off_phase_pong_despite_other_traffic() {
    let listener = path_tokio_net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback websocket listener");
    let socket_url = format!(
        "ws://{}/socket-ticket",
        listener.local_addr().expect("listener local address")
    );
    let (accepted_tx, accepted_rx) = path_tokio_sync::oneshot::channel();
    let (pong_tx, pong_rx) = path_tokio_sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept websocket client");
        let mut ws = tokio_tungstenite::accept_async(stream)
            .await
            .expect("complete websocket handshake");
        let _ = accepted_tx.send(());
        let hello = Message::Text(r#"{"type":"hello"}"#.to_owned().into());
        ws.send(hello.clone()).await.expect("send initial hello");
        tokio::time::sleep(Duration::from_secs(15)).await;
        ws.send(Message::Pong(Vec::new().into()))
            .await
            .expect("send off-phase pong");
        let _ = pong_tx.send(path_tokio_time::Instant::now());
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            ws.send(hello.clone())
                .await
                .expect("send non-pong application traffic");
        }
    });

    let (tx, _rx) = mpsc::channel();
    let ext = Arc::new(Extension::new(FakeClient::new(), tx));
    let worker_ext = Arc::clone(&ext);
    let worker = tokio::spawn(async move {
        socket_worker_once_with_heartbeat(
            &worker_ext,
            &cfg(),
            Some(WorkerStartup {
                bot_user_id: "UBOT123".to_owned(),
                installation_team_id: "T123".to_owned(),
                socket_url,
            }),
            &AdmissionQueue::new(),
            1,
            SocketHeartbeat {
                ping_interval: Duration::from_secs(10),
                pong_timeout: Duration::from_secs(40),
            },
        )
        .await
    });

    accepted_rx.await.expect("websocket should connect");
    let pong_at = pong_rx.await.expect("off-phase pong should be sent");
    let error = tokio::time::timeout(Duration::from_secs(60), worker)
        .await
        .expect("independent pong deadline should win before outer timeout")
        .expect("socket worker should not panic")
        .expect_err("non-pong traffic must not keep the connection alive");
    assert_eq!(error, SOCKET_HEARTBEAT_TIMEOUT_ERROR);
    assert_eq!(pong_at.elapsed(), Duration::from_secs(40));
    assert!(!ext.state.lock().expect("state").worker_online);
    server.abort();
}

/// A WebSocket write that remains pending must still observe extension shutdown
/// immediately rather than pinning the worker on TCP/TLS writability.
#[tokio::test(start_paused = true)]
async fn blocked_socket_write_remains_shutdown_interruptible() {
    let shutdown = Arc::new(ShutdownSignal::new());
    let request = Arc::clone(&shutdown);
    tokio::spawn(async move {
        tokio::task::yield_now().await;
        request.request();
    });
    let pong_deadline = tokio::time::sleep(Duration::from_secs(60));
    tokio::pin!(pong_deadline);

    let outcome = await_socket_write(
        &shutdown,
        pong_deadline.as_mut(),
        std::future::pending::<Result<(), ()>>(),
        "write failed",
    )
    .await
    .expect("shutdown is not a write failure");
    assert_eq!(outcome, Some(WorkerOutcome::Shutdown));
}

/// A WebSocket write that remains pending must be preempted at the exact Pong
/// deadline, preserving stale-connection recovery under outbound backpressure.
#[tokio::test(start_paused = true)]
async fn blocked_socket_write_remains_pong_deadline_bounded() {
    let shutdown = ShutdownSignal::new();
    let started_at = path_tokio_time::Instant::now();
    let pong_deadline = tokio::time::sleep(Duration::from_secs(40));
    tokio::pin!(pong_deadline);

    let error = await_socket_write(
        &shutdown,
        pong_deadline.as_mut(),
        std::future::pending::<Result<(), ()>>(),
        "write failed",
    )
    .await
    .expect_err("pong deadline must preempt the blocked write");
    assert_eq!(error, SOCKET_HEARTBEAT_TIMEOUT_ERROR);
    assert_eq!(started_at.elapsed(), Duration::from_secs(40));
}

/// Startup and reconnect failures share one process-lifetime bounded notice
/// latch, so repeated degraded attempts do not spam the operator.
#[test]
fn worker_connection_failure_notice_is_bounded_and_one_shot() {
    let (ext, rx, _client) = extension();
    ext.report_worker_connection_failure_once(SOCKET_HEARTBEAT_TIMEOUT_ERROR);
    ext.report_worker_connection_failure_once("Slack websocket connection failed");

    let notices = rx
        .try_iter()
        .filter_map(|message| match message {
            HarnessInputMessage::ExtensionNoticeRequest(request) => Some(request),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(notices.len(), 1);
    let notice = &notices[0];
    assert_eq!(notice.level, NoticeLevel::Warning);
    assert!(notice.message.contains(SOCKET_HEARTBEAT_TIMEOUT_ERROR));
    assert!(!notice.message.contains("xapp-test"));
    assert!(!notice.message.contains("xoxb-test"));
    assert!(notice.message.len() <= MAX_DIAGNOSTIC_BYTES + 3);
    assert!(
        ext.state
            .lock()
            .expect("state")
            .worker_connection_failure_reported
    );
}

/// A blocked users.info call runs only on the serial actor: the reader still
/// ACKs a later envelope, answers Ping, and honors shutdown before release.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_identity_does_not_block_reader_ack_pong_or_shutdown() {
    let listener = path_tokio_net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback websocket listener");
    let socket_url = format!(
        "ws://{}/socket-ticket",
        listener.local_addr().expect("listener local address")
    );
    let event = |envelope: &str, ts: &str| {
        Message::Text(
            serde_json::json!({
                "type": "events_api",
                "envelope_id": envelope,
                "payload": {
                    "type": "event_callback",
                    "context_team_id": "T123",
                    "event_id": format!("Ev-{ts}"),
                    "event": {
                        "type": "app_mention",
                        "channel": "C123",
                        "channel_type": "channel",
                        "user": "U123",
                        "text": "<@UBOT123> hello",
                        "ts": ts
                    }
                }
            })
            .to_string()
            .into(),
        )
    };
    let (reader_live_tx, reader_live_rx) = path_tokio_sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept websocket client");
        let mut ws = tokio_tungstenite::accept_async(stream)
            .await
            .expect("complete websocket handshake");
        ws.send(event("env-1", "1.0")).await.expect("send first");
        let first = ws
            .next()
            .await
            .expect("first ACK frame")
            .expect("first ACK");
        assert!(matches!(first, Message::Text(_)));
        ws.send(event("env-2", "2.0")).await.expect("send second");
        ws.send(Message::Ping(vec![7].into()))
            .await
            .expect("send ping");
        let mut saw_ack = false;
        let mut saw_pong = false;
        for _ in 0..2 {
            match ws.next().await.expect("reader response").expect("response") {
                Message::Text(_) => saw_ack = true,
                Message::Pong(payload) if payload.as_ref() == [7] => saw_pong = true,
                other => panic!("unexpected reader response: {other:?}"),
            }
        }
        assert!(saw_ack && saw_pong);
        let _ = reader_live_tx.send(());
        std::future::pending::<()>().await;
    });

    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let client = Arc::new(BlockingIdentityClient {
        started: Mutex::new(Some(started_tx)),
        release: Mutex::new(release_rx),
    });
    let (output_tx, _output_rx) = mpsc::channel();
    let ext = Arc::new(Extension::new(client, output_tx));
    {
        let mut state = ext.state.lock().expect("state lock");
        state.config = Some(cfg());
        state.bot_user_id = Some("UBOT123".to_owned());
        state.installation_team_id = Some("T123".to_owned());
        state.registered_agents.insert(agent_id("agent-a"));
        state.session_active = true;
    }
    let queue = AdmissionQueue::new();
    let actor_ext = Arc::clone(&ext);
    let actor_queue = Arc::clone(&queue);
    let actor = std::thread::spawn(move || admission_worker_loop(actor_ext, actor_queue));
    let worker_ext = Arc::clone(&ext);
    let worker_queue = Arc::clone(&queue);
    let worker_cfg = cfg();
    let worker = tokio::spawn(async move {
        socket_worker_once(
            &worker_ext,
            &worker_cfg,
            Some(WorkerStartup {
                bot_user_id: "UBOT123".to_owned(),
                installation_team_id: "T123".to_owned(),
                socket_url,
            }),
            &worker_queue,
            1,
        )
        .await
    });

    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("identity barrier reached");
    tokio::time::timeout(Duration::from_secs(1), reader_live_rx)
        .await
        .expect("later ACK and Pong remain responsive")
        .expect("server signal");
    ext.shutdown.request();
    let outcome = tokio::time::timeout(Duration::from_millis(200), worker)
        .await
        .expect("shutdown must not wait for users.info")
        .expect("reader task")
        .expect("reader outcome");
    assert_eq!(outcome, WorkerOutcome::Shutdown);
    release_tx.send(()).expect("release identity");
    queue.close();
    actor.join().expect("serial actor exits");
    server.abort();
}

/// A 65th supported occurrence is not ACKed: the reader degrades the connection
/// so Slack can retry after an outstanding terminal outcome releases capacity.
#[tokio::test]
async fn saturated_admission_does_not_ack_supported_envelope() {
    let listener = path_tokio_net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback websocket listener");
    let socket_url = format!(
        "ws://{}/socket-ticket",
        listener.local_addr().expect("listener local address")
    );
    let (result_tx, result_rx) = path_tokio_sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept websocket client");
        let mut ws = tokio_tungstenite::accept_async(stream)
            .await
            .expect("complete websocket handshake");
        ws.send(Message::Text(
            serde_json::json!({
                "type": "events_api",
                "envelope_id": "env-overflow",
                "payload": {
                    "type": "event_callback",
                    "context_team_id": "T123",
                    "event": {
                        "type": "app_mention",
                        "channel": "C123",
                        "channel_type": "channel",
                        "user": "U123",
                        "text": "<@UBOT123> overflow",
                        "ts": "65.0"
                    }
                }
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send overflow");
        let received_ack = matches!(
            tokio::time::timeout(Duration::from_millis(200), ws.next()).await,
            Ok(Some(Ok(Message::Text(_))))
        );
        let _ = result_tx.send(received_ack);
    });
    let (tx, _rx) = mpsc::channel();
    let ext = Extension::new(FakeClient::new(), tx);
    ext.state.lock().expect("state").session_active = true;
    let queue = AdmissionQueue::new();
    let reservations = (0..admission::CAPACITY)
        .map(|_| queue.reserve().expect("fill admission"))
        .collect::<Vec<_>>();
    let result = socket_worker_once(
        &ext,
        &cfg(),
        Some(WorkerStartup {
            bot_user_id: "UBOT123".to_owned(),
            installation_team_id: "T123".to_owned(),
            socket_url,
        }),
        &queue,
        1,
    )
    .await;
    assert!(result.is_err(), "saturation must degrade the connection");
    assert!(
        !result_rx.await.expect("server result"),
        "must not ACK overflow"
    );
    drop(reservations);
    queue.reserve().expect("terminal release permits retry");
    server.await.expect("server task");
}

/// Long successful Slack Web API JSON responses must be parsed from the raw
/// body rather than from bounded diagnostic text, otherwise successful sends
/// can be reported as false JSON errors.
#[test]
fn long_successful_slack_api_response_still_parses() {
    let long_text = "x".repeat(MAX_DIAGNOSTIC_BYTES + 200);
    let body = serde_json::json!({
        "ok": true,
        "url": "wss://wss-primary.slack.com/link",
        "message": { "text": long_text }
    })
    .to_string();
    let value = parse_slack_api_response(200, &body).expect("long ok response parses");
    assert_eq!(
        value.get("ok").and_then(|value| value.as_bool()),
        Some(true)
    );
}

/// Slack API failures collapse hostile remote diagnostics to a typed allowlist.
#[test]
fn slack_api_error_response_is_redacted_and_bounded() {
    let cfg = cfg();
    let body = serde_json::json!({
        "ok": false,
        "error": format!(
            "{} {} {}",
            cfg.app_token,
            cfg.bot_token,
            "x".repeat(MAX_DIAGNOSTIC_BYTES * 2)
        )
    })
    .to_string();
    let err = parse_slack_api_response(200, &body).expect_err("slack error");
    assert_eq!(err, SlackApiError::RemoteFailure);
    let display = err.to_string();
    assert!(!display.contains(&cfg.app_token));
    assert!(!display.contains(&cfg.bot_token));
    assert!(display.len() < 128);
}

/// Slack bridge tools are disabled by default because roles must explicitly opt
/// into an external chat bridge before the model can use it.
#[test]
fn slack_tools_are_role_opt_in() {
    assert!(!register_tool_spec().enabled_by_default);
    assert!(!conversations_tool_spec().enabled_by_default);
    assert!(!send_tool_spec().enabled_by_default);
}

/// Tool group and tag metadata let role policy enable all Slack tools or only
/// registration/sending capability.
#[test]
fn slack_tools_have_group_and_tags() {
    assert_eq!(slack_tool_group().name.as_str(), TOOL_GROUP_NAME);
    assert!(
        register_tool_spec()
            .tags
            .iter()
            .any(|tag| tag.as_str() == REGISTER_TOOL_TAG)
    );
    assert!(
        conversations_tool_spec()
            .tags
            .iter()
            .any(|tag| tag.as_str() == CONVERSATIONS_TOOL_TAG)
    );
    assert!(
        send_tool_spec()
            .tags
            .iter()
            .any(|tag| tag.as_str() == SEND_TOOL_TAG)
    );
    let react = react_tool_spec();
    assert!(!react.enabled_by_default);
    assert!(react.tags.iter().any(|tag| tag.as_str() == REACT_TOOL_TAG));
}

/// Provider-owned repair examples must remain schema-valid as Slack tool
/// argument shapes evolve.
#[test]
fn slack_tool_examples_are_schema_valid() {
    for spec in [
        register_tool_spec(),
        conversations_tool_spec(),
        send_tool_spec(),
        react_tool_spec(),
    ] {
        tau_core::validate_tool_examples(&spec)
            .unwrap_or_else(|error| panic!("invalid examples for {}: {error}", spec.name));
    }
}

/// Outbound emoji parsing is strict and never normalizes model input.
#[test]
fn outbound_reaction_emoji_validation_is_strict() {
    for valid in ["eyes", "thumbsup", "+1", "wave::skin-tone-3"] {
        assert!(valid_outbound_emoji(valid), "{valid}");
    }
    for invalid in [
        "",
        ":eyes:",
        "Eyes",
        "👀",
        "wave::skin-tone-1",
        "wave::skin-tone-7",
        "wave:skin-tone-3",
        "a::skin-tone-3::skin-tone-4",
    ] {
        assert!(!valid_outbound_emoji(invalid), "{invalid}");
    }
    assert!(!valid_outbound_emoji(&"a".repeat(65)));
}

/// Install one exact source-authorized reaction target for tool behavior tests.
fn install_source_reaction_target(ext: &Extension, message_ref: &str) {
    let message_id = MessageFactId::new(message_ref);
    let conversation = slack_conversation("C123", None);
    let mut state = ext.state.lock().expect("state");
    state.session_active = true;
    state.insert_reply_route(
        message_id.clone(),
        ReplyRoute {
            agent_id: agent_id("agent-a"),
            conversation: conversation.clone(),
            user_id: "U123".to_owned(),
            display_name: None,
            identity_alias: None,
            installation_team_id: "T123".to_owned(),
        },
    );
    assert!(state.reactions.insert_target(
        MessageFactId::new(message_ref),
        ReactionTarget {
            agent_id: agent_id("agent-a"),
            conversation,
            message_ts: "1.0".to_owned(),
            installation_team_id: "T123".to_owned(),
            authority: ReactionAuthority::Source {
                message_id,
                user_id: "U123".to_owned(),
            },
        },
    ));
}

/// Shared runtime handles retained when the real tau-client runner returns an
/// output error instead of its extension state.
struct ReactionRunnerProbe {
    /// Reaction and lifecycle state.
    state: Arc<Mutex<State>>,
    /// Fatal output latch.
    output_failed: Arc<AtomicBool>,
    /// Whole-extension shutdown signal.
    shutdown: Arc<ShutdownSignal>,
}

/// Minimal runner declaration that drives production reaction completion
/// through a real [`ClientHandle`] without unrelated Slack startup behavior.
struct ReactionRunnerExtension {
    /// Records whether detached progress actually exhausted its bounded FIFO.
    overloaded: Arc<AtomicBool>,
}

impl TauExtension for ReactionRunnerExtension {
    type State = Extension;

    fn name(&self) -> &'static str {
        "slack-reaction-writer-test"
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        let overloaded = self.overloaded;
        let mut spam = react_tool_spec();
        spam.name = tau_proto::ToolName::new("spam");
        builder
            .configure_raw(|_| Ok(()))
            .tool(spam, move |cx| {
                let invoke = cx.invoke();
                let progress = ToolProgress {
                    call_id: invoke.call_id.clone(),
                    tool_name: invoke.tool_name.clone(),
                    message: Some("fill detached output".to_owned()),
                    progress: None,
                    display: None,
                };
                if matches!(
                    cx.handle().report_tool_progress_detached(progress),
                    Err(ClientError::Overloaded)
                ) {
                    overloaded.store(true, Ordering::Release);
                }
                Ok(())
            })
            .tool(react_tool_spec(), |cx| {
                cx.state
                    .dispatch_scoped_tool(cx.local_tool_name(), cx.invoke().clone());
                Ok(())
            })
            .ready_message("ready");
    }
}

/// Writer that fails the first flush after Slack returns a reaction outcome.
struct ReactionFlushFailureWriter {
    /// Remote-completion signal set by the fake reaction API.
    reaction_completed: Arc<AtomicBool>,
}

impl Write for ReactionFlushFailureWriter {
    fn write(&mut self, buffer: &[u8]) -> path_std_io::Result<usize> {
        Ok(buffer.len())
    }

    fn flush(&mut self) -> path_std_io::Result<()> {
        if self.reaction_completed.swap(false, Ordering::AcqRel) {
            return Err(path_std_io::Error::other(
                "forced reaction terminal flush failure",
            ));
        }
        Ok(())
    }
}

/// Writer that blocks on the first detached spam frame so the test can fill
/// tau-client's real bounded detached FIFO.
struct ReactionOverloadWriter {
    /// Gate released after the runner observes actual overload.
    gate: Arc<(Mutex<bool>, Condvar)>,
    /// Announces the first blocked spam write.
    entered: mpsc::Sender<()>,
    /// Prevents repeated announcements after release.
    announced: bool,
    /// Number of frames containing the spam tool name; the first is startup
    /// registration and the second is detached progress.
    spam_frames: usize,
}

impl Write for ReactionOverloadWriter {
    fn write(&mut self, buffer: &[u8]) -> path_std_io::Result<usize> {
        if buffer.windows(4).any(|window| window == b"spam") {
            self.spam_frames += 1;
            if !self.announced && 2 <= self.spam_frames {
                self.announced = true;
                self.entered.send(()).expect("announce blocked writer");
                let (lock, condvar) = &*self.gate;
                let mut blocked = lock.lock().expect("writer gate");
                while *blocked {
                    blocked = condvar.wait(blocked).expect("wait writer gate");
                }
            }
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> path_std_io::Result<()> {
        Ok(())
    }
}

/// Run one reaction against the actual tau-client writer and retain state even
/// when that writer returns an error.
fn run_reaction_writer_fixture(
    writer: impl Write + Send + 'static,
    client: Arc<FakeClient>,
    action: ReactionActionKind,
    spam_count: usize,
    overloaded: Arc<AtomicBool>,
    probe_slot: Arc<Mutex<Option<ReactionRunnerProbe>>>,
) -> Result<(), String> {
    let message_ref = "slack-message:test-c123-writer";
    let mut input = Vec::new();
    let mut input_writer = tau_proto::HarnessOutputWriter::new(&mut input);
    input_writer
        .write_message(&valid_config_message())
        .expect("write config");
    for index in 0..spam_count {
        input_writer
            .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(
                tool_call(
                    "spam",
                    "agent-a",
                    &format!("spam-{index}"),
                    CborValue::Map(Vec::new()),
                ),
            )))
            .expect("write spam invocation");
    }
    input_writer
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(
            tool_call(
                REACT_TOOL_NAME,
                "agent-a",
                "writer-reaction",
                reaction_args(
                    message_ref,
                    "eyes",
                    match action {
                        ReactionActionKind::Add => "add",
                        ReactionActionKind::Remove => "remove",
                    },
                ),
            ),
        )))
        .expect("write reaction invocation");
    input_writer.flush().expect("flush protocol input");

    let runner_client = Arc::clone(&client);
    tau_client::TauExtensionRunner::new(ReactionRunnerExtension { overloaded })
        .run_detached_writer_with_state(path_std_io::Cursor::new(input), writer, move |handle| {
            let ext =
                Extension::new_with_reaction_client(runner_client.clone(), runner_client, handle);
            ext.apply_config(cfg()).expect("fixture config");
            {
                let mut state = ext.state.lock().expect("state");
                state.bot_user_id = Some("UBOT123".to_owned());
                state.installation_team_id = Some("T123".to_owned());
                state.instance_name = Some(test_extension_name("std-slack"));
            }
            register_agent(&ext, "agent-a");
            install_source_reaction_target(&ext, message_ref);
            if action == ReactionActionKind::Remove {
                ext.state.lock().expect("state").reactions.owners.insert(
                    ReactionKey {
                        channel_id: "C123".to_owned(),
                        message_ts: "1.0".to_owned(),
                        emoji: "eyes".to_owned(),
                    },
                    ReactionOwner {
                        agent_id: agent_id("agent-a"),
                        message_ref: MessageFactId::new(message_ref),
                    },
                );
            }
            *probe_slot.lock().expect("probe") = Some(ReactionRunnerProbe {
                state: Arc::clone(&ext.state),
                output_failed: Arc::clone(&ext.output_failed),
                shutdown: Arc::clone(&ext.shutdown),
            });
            ext
        })
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Ownership capacity rejects a new unowned add before Slack I/O.
#[test]
fn reaction_ownership_capacity_is_enforced_before_io() {
    let (ext, _rx, client) = extension();
    register_agent(&ext, "agent-a");
    install_source_reaction_target(&ext, "slack-message:test-c123-1.0");
    {
        let mut state = ext.state.lock().expect("state");
        for index in 0..reactions::OWNERSHIP_LIMIT {
            state.reactions.owners.insert(
                ReactionKey {
                    channel_id: "C999".to_owned(),
                    message_ts: format!("{index}.0"),
                    emoji: "eyes".to_owned(),
                },
                ReactionOwner {
                    agent_id: agent_id("agent-a"),
                    message_ref: MessageFactId::new(format!("slack-message:test-c999-{index}.0")),
                },
            );
        }
    }
    assert!(matches!(
        ext.handle_react_event(tool_call(
            REACT_TOOL_NAME,
            "agent-a",
            "capacity-add",
            reaction_args("slack-message:test-c123-1.0", "eyes", "add"),
        )),
        Event::ToolError(_)
    ));
    assert!(client.reactions.lock().expect("reactions").is_empty());
}

/// Target and attempt caches enforce oldest-first eviction without displacing
/// live pinned entries or in-flight attempts.
#[test]
fn reaction_target_and_attempt_bounds_preserve_live_entries() {
    let target = ReactionTarget {
        agent_id: agent_id("agent-a"),
        conversation: slack_conversation("C123", None),
        message_ts: "1.0".to_owned(),
        installation_team_id: "T123".to_owned(),
        authority: ReactionAuthority::ConfiguredDestination {
            alias: "team".to_owned(),
        },
    };
    let mut state = State::default();
    for index in 0..reactions::TARGET_LIMIT {
        assert!(state.reactions.insert_target(
            MessageFactId::new(format!("slack-message:test-c123-{index}.0")),
            target.clone()
        ));
    }
    state.reactions.in_flight.insert(
        ReactionKey {
            channel_id: "C123".to_owned(),
            message_ts: "0.0".to_owned(),
            emoji: "eyes".to_owned(),
        },
        ReactionReservation {
            agent_id: agent_id("agent-a"),
            token: 1,
            message_ref: MessageFactId::new("slack-message:test-c123-0.0"),
            unowned_add: false,
        },
    );
    assert!(state.reactions.insert_target(
        MessageFactId::new("slack-message:test-c123-new"),
        target.clone()
    ));
    assert!(
        state
            .reactions
            .targets
            .contains_key(&MessageFactId::new("slack-message:test-c123-0.0"))
    );
    assert!(
        !state
            .reactions
            .targets
            .contains_key(&MessageFactId::new("slack-message:test-c123-1.0"))
    );
    assert_eq!(state.reactions.targets.len(), reactions::TARGET_LIMIT);

    state.reactions.clear();
    for index in 0..reactions::TARGET_LIMIT {
        let message_ref = MessageFactId::new(format!("slack-message:test-c123-{index}.0"));
        assert!(
            state
                .reactions
                .insert_target(message_ref.clone(), target.clone())
        );
        state.reactions.owners.insert(
            ReactionKey {
                channel_id: "C123".to_owned(),
                message_ts: format!("{index}.0"),
                emoji: "eyes".to_owned(),
            },
            ReactionOwner {
                agent_id: agent_id("agent-a"),
                message_ref,
            },
        );
    }
    assert!(!state.reactions.insert_target(
        MessageFactId::new("slack-message:test-c123-blocked"),
        target
    ));

    state.reactions.clear();
    for index in 0..reactions::ATTEMPT_LIMIT {
        let invoke = tool_call(
            REACT_TOOL_NAME,
            "agent-a",
            &format!("completed-{index}"),
            reaction_args("slack-message:test-c123-1.0", "eyes", "add"),
        );
        assert!(state.reactions.remember_attempt(
            &invoke,
            ReactionAttemptDisposition::Success(CborValue::Null),
        ));
    }
    let replacement = tool_call(
        REACT_TOOL_NAME,
        "agent-a",
        "completed-new",
        reaction_args("slack-message:test-c123-1.0", "eyes", "add"),
    );
    assert!(state.reactions.remember_attempt(
        &replacement,
        ReactionAttemptDisposition::Success(CborValue::Null),
    ));
    assert_eq!(state.reactions.attempts.len(), reactions::ATTEMPT_LIMIT);
    assert!(
        !state
            .reactions
            .attempts
            .contains_key(&tau_proto::ToolCallId::new("completed-0"))
    );

    state.reactions.clear();
    for index in 0..reactions::ATTEMPT_LIMIT {
        let invoke = tool_call(
            REACT_TOOL_NAME,
            "agent-a",
            &format!("in-flight-{index}"),
            reaction_args("slack-message:test-c123-1.0", "eyes", "add"),
        );
        assert!(
            state
                .reactions
                .remember_attempt(&invoke, ReactionAttemptDisposition::InFlight)
        );
    }
    let blocked = tool_call(
        REACT_TOOL_NAME,
        "agent-a",
        "in-flight-blocked",
        reaction_args("slack-message:test-c123-1.0", "eyes", "add"),
    );
    assert!(
        !state
            .reactions
            .remember_attempt(&blocked, ReactionAttemptDisposition::InFlight)
    );
}

/// A current Tau-issued reply selector resolves to its retained private route
/// and submits the sent report/result without accepting a native destination.
#[test]
fn successful_send_uses_local_reply_selector() {
    let (ext, rx, client) = extension();
    register_agent(&ext, "agent-a");
    install_source_reaction_target(&ext, "slack-message:test-c123-1.0");
    assert!(
        ext.handle_send(tool_call(
            SEND_TOOL_NAME,
            "agent-a",
            "successful-local-reply",
            tau_proto::json_to_cbor(
                &serde_json::json!({"message":"reply","reply_to":"slack-message:test-c123-1.0"}),
            ),
        ))
        .is_none()
    );
    let mut sent_report = None;
    loop {
        let message = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("sent report and result");
        if let HarnessInputMessage::Emit(emit) = message {
            match *emit.event {
                Event::MessageSentReported(report) => {
                    sent_report = Some(report);
                }
                Event::ToolResultReported(_) => break,
                _ => {}
            }
        }
    }
    let sent_report = sent_report.expect("sent report");
    assert_eq!(sent_report.publisher_extension_id.as_str(), "std-slack");
    assert_eq!(
        sent_report
            .recipient
            .as_ref()
            .map(|party| party.stable_id.as_str()),
        Some(slack_sender_ref("T123", "U123").as_str())
    );
    let canonical = Event::MessageSentReported(sent_report)
        .into_stamped_canonical_message_fact(
            tau_proto::MessagePublisherId::parse("std-slack").expect("canonical publisher"),
        )
        .expect("sent report converts to a canonical fact");
    let projection = tau_proto::project_message_fact(&canonical)
        .expect("canonical message fact")
        .expect("valid projection");
    let (ContentPart::Text { text } | ContentPart::HarnessInternalText { text }) =
        &projection.item.content[0];
    assert!(text.contains(" recipient_ref=\"slack-sender:"), "{text}");
    assert!(text.contains(" conversation=\"team\""), "{text}");
    assert!(!text.contains(" sender_ref="), "{text}");
    assert!(!text.contains("content_trust="), "{text}");
    let sent = client.sent.lock().expect("sent");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].channel_id, "C123");
    assert_eq!(sent[0].text, "reply");
}

/// Successful add/replay/remove obey local ownership without repeating Slack
/// I/O, while another agent cannot remove the owned reaction.
#[test]
fn reaction_target_ownership_and_replay_are_enforced() {
    let (ext, _rx, client) = extension();
    register_agent(&ext, "agent-a");
    install_source_reaction_target(&ext, "slack-message:test-c123-1.0");

    let add = tool_call(
        REACT_TOOL_NAME,
        "agent-a",
        "react-add",
        reaction_args("slack-message:test-c123-1.0", "eyes", "add"),
    );
    assert!(matches!(
        ext.handle_react_event(add.clone()),
        Event::ToolResult(_)
    ));
    assert!(matches!(ext.handle_react_event(add), Event::ToolResult(_)));
    assert_eq!(
        client.reactions.lock().expect("lock").as_slice(),
        &[RecordedReaction {
            action: ReactionActionKind::Add,
            channel_id: "C123".to_owned(),
            message_ts: "1.0".to_owned(),
            emoji: "eyes".to_owned(),
        }]
    );

    assert!(matches!(
        ext.handle_react_event(tool_call(
            REACT_TOOL_NAME,
            "agent-b",
            "react-other",
            reaction_args("slack-message:test-c123-1.0", "eyes", "remove"),
        )),
        Event::ToolError(_)
    ));
    assert_eq!(client.reactions.lock().expect("lock").len(), 1);
    assert!(matches!(
        ext.handle_react_event(tool_call(
            REACT_TOOL_NAME,
            "agent-a",
            "react-remove",
            reaction_args("slack-message:test-c123-1.0", "eyes", "remove"),
        )),
        Event::ToolResult(_)
    ));
    assert_eq!(client.reactions.lock().expect("lock").len(), 2);
    assert!(ext.state.lock().expect("state").reactions.owners.is_empty());
}

/// A confirmed same-call replay still emits its retained terminal through
/// ordinary dispatch, while never repeating Slack I/O.
#[test]
fn reaction_success_replay_reports_retained_terminal_once() {
    let (ext, rx, client) = extension();
    register_agent(&ext, "agent-a");
    let message_ref = "slack-message:test-c123-replay-report";
    install_source_reaction_target(&ext, message_ref);
    let invoke = tool_call(
        REACT_TOOL_NAME,
        "agent-a",
        "reaction-replay-report",
        reaction_args(message_ref, "eyes", "add"),
    );

    ext.dispatch_scoped_tool(&tau_proto::ToolName::new(REACT_TOOL_NAME), invoke.clone());
    let first = rx
        .try_iter()
        .filter(|message| {
            matches!(
                message,
                HarnessInputMessage::Emit(emit)
                    if matches!(emit.event.as_ref(), Event::ToolResultReported(_))
            )
        })
        .count();
    ext.dispatch_scoped_tool(&tau_proto::ToolName::new(REACT_TOOL_NAME), invoke);
    let replayed = rx
        .try_iter()
        .filter(|message| {
            matches!(
                message,
                HarnessInputMessage::Emit(emit)
                    if matches!(emit.event.as_ref(), Event::ToolResultReported(_))
            )
        })
        .count();

    assert_eq!(first, 1);
    assert_eq!(replayed, 1);
    assert_eq!(client.reactions.lock().expect("reactions").len(), 1);
}

/// A real tau-client write and flush must finish before reaction ownership and
/// same-call success replay become final.
#[test]
fn reaction_success_commits_only_after_actual_writer_confirmation() {
    let client = FakeClient::new();
    let output = SharedWriter::default();
    let written = output.clone();
    let overloaded = Arc::new(AtomicBool::new(false));
    let probe_slot = Arc::new(Mutex::new(None));
    run_reaction_writer_fixture(
        output,
        Arc::clone(&client),
        ReactionActionKind::Add,
        0,
        overloaded,
        Arc::clone(&probe_slot),
    )
    .expect("reaction writer run");

    assert_eq!(client.reactions.lock().expect("reactions").len(), 1);
    let probe = probe_slot.lock().expect("probe");
    let probe = probe.as_ref().expect("installed probe");
    let state = probe.state.lock().expect("state");
    assert_eq!(state.reactions.owners.len(), 1);
    assert!(
        state
            .reactions
            .attempts
            .values()
            .any(|attempt| matches!(attempt.disposition, ReactionAttemptDisposition::Success(_)))
    );
    drop(state);
    let mut reader = tau_proto::HarnessInputReader::new(path_std_io::Cursor::new(written.bytes()));
    let frames = std::iter::from_fn(|| reader.read_message().transpose())
        .collect::<Result<Vec<_>, _>>()
        .expect("decode output");
    assert_eq!(
        frames
            .iter()
            .filter(|frame| matches!(
                frame,
                HarnessInputMessage::Emit(emit)
                    if matches!(emit.event.as_ref(), Event::ToolResultReported(result)
                        if result.tool_name.as_str() == REACT_TOOL_NAME)
            ))
            .count(),
        1
    );
}

/// Actual add/remove terminal flush failure retires the whole Slack session,
/// preserves no reaction authority, and never issues compensation.
#[test]
fn reaction_writer_failure_retires_add_and_remove_without_compensation() {
    for action in [ReactionActionKind::Add, ReactionActionKind::Remove] {
        let client = FakeClient::new();
        let probe_slot = Arc::new(Mutex::new(None));
        let result = run_reaction_writer_fixture(
            ReactionFlushFailureWriter {
                reaction_completed: Arc::clone(&client.reaction_completed),
            },
            Arc::clone(&client),
            action,
            0,
            Arc::new(AtomicBool::new(false)),
            Arc::clone(&probe_slot),
        );
        assert!(result.is_err(), "{action:?} writer must fail");
        assert_eq!(
            client.reactions.lock().expect("reactions").len(),
            1,
            "{action:?} must not retry or compensate"
        );
        let probe = probe_slot.lock().expect("probe");
        let probe = probe.as_ref().expect("installed probe");
        assert!(probe.output_failed.load(Ordering::Acquire));
        assert!(probe.shutdown.is_requested());
        let state = probe.state.lock().expect("state");
        assert!(!state.session_active);
        assert!(state.reactions.targets.is_empty());
        assert!(state.reactions.owners.is_empty());
        assert!(state.reactions.in_flight.is_empty());
        assert!(state.reactions.attempts.is_empty());
    }
}

/// Saturating tau-client's actual detached FIFO must not drop a successful
/// reaction terminal: confirmed output waits for the writer and then commits.
#[test]
fn reaction_confirmation_survives_actual_detached_output_overload() {
    let client = FakeClient::new();
    let (completed_tx, completed_rx) = mpsc::channel();
    *client
        .reaction_completion_signal
        .lock()
        .expect("reaction completion signal") = Some(completed_tx);
    let overloaded = Arc::new(AtomicBool::new(false));
    let probe_slot = Arc::new(Mutex::new(None));
    let gate = Arc::new((Mutex::new(true), Condvar::new()));
    let (entered_tx, entered_rx) = mpsc::channel();
    let writer = ReactionOverloadWriter {
        gate: Arc::clone(&gate),
        entered: entered_tx,
        announced: false,
        spam_frames: 0,
    };
    let running_client = Arc::clone(&client);
    let running_overloaded = Arc::clone(&overloaded);
    let running_probe = Arc::clone(&probe_slot);
    let runner = std::thread::spawn(move || {
        run_reaction_writer_fixture(
            writer,
            running_client,
            ReactionActionKind::Add,
            96,
            running_overloaded,
            running_probe,
        )
    });
    entered_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("writer blocked on detached progress");
    completed_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("reaction reached remote completion");
    assert!(
        overloaded.load(Ordering::Acquire),
        "fixture must hit the real detached FIFO bound"
    );
    let (lock, condvar) = &*gate;
    *lock.lock().expect("writer gate") = false;
    condvar.notify_all();
    runner
        .join()
        .expect("runner thread")
        .expect("confirmed reaction after overload");

    let probe = probe_slot.lock().expect("probe");
    let state = probe.as_ref().expect("probe").state.lock().expect("state");
    assert_eq!(state.reactions.owners.len(), 1);
    assert!(
        state
            .reactions
            .attempts
            .values()
            .any(|attempt| matches!(attempt.disposition, ReactionAttemptDisposition::Success(_)))
    );
}

/// Agent unload racing the post-flush boundary waits on the shared gate: state
/// remains provisional at the boundary, then unload clears the committed owner.
#[test]
fn reaction_result_boundary_serializes_lifecycle_without_restoring_authority() {
    let (ext, rx, client) = extension();
    register_agent(&ext, "agent-a");
    let message_ref = "slack-message:test-c123-race";
    install_source_reaction_target(&ext, message_ref);
    let (reached_tx, reached_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    *ext.test_hooks
        .reaction_result_boundary
        .lock()
        .expect("hook") = Some(BlockingTestHook {
        reached: reached_tx,
        release: release_rx,
    });
    let (lifecycle_tx, lifecycle_rx) = mpsc::channel();
    *ext.test_hooks.lifecycle_gate_attempt.lock().expect("hook") = Some(lifecycle_tx);
    let ext = Arc::new(ext);
    let reacting = Arc::clone(&ext);
    let reaction = std::thread::spawn(move || {
        reacting.handle_react_event(tool_call(
            REACT_TOOL_NAME,
            "agent-a",
            "reaction-race",
            reaction_args(message_ref, "eyes", "add"),
        ))
    });
    reached_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("confirmed output boundary");
    {
        let state = ext.state.lock().expect("state");
        assert!(state.reactions.owners.is_empty());
        assert!(
            state
                .reactions
                .attempts
                .values()
                .any(|attempt| matches!(attempt.disposition, ReactionAttemptDisposition::InFlight))
        );
    }
    let unloading = Arc::clone(&ext);
    let unload = std::thread::spawn(move || unloading.unload_agent(&agent_id("agent-a")));
    lifecycle_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("lifecycle waiting for gate");
    release_tx.send(()).expect("release result boundary");
    assert!(matches!(
        reaction.join().expect("reaction"),
        Event::ToolResult(_)
    ));
    unload.join().expect("unload");
    assert_eq!(client.reactions.lock().expect("reactions").len(), 1);
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(1))
            .expect("reported reaction result"),
        HarnessInputMessage::Emit(emit)
            if matches!(emit.event.as_ref(), Event::ToolResultReported(_))
    ));
    let state = ext.state.lock().expect("state");
    assert!(state.reactions.targets.is_empty());
    assert!(state.reactions.owners.is_empty());
    assert!(state.reactions.attempts.is_empty());
}

/// A successful remove keeps its existing owner and provisional replay through
/// the confirmed-output boundary, then clears ownership and commits Success.
#[test]
fn reaction_remove_remains_provisional_until_result_confirmation() {
    let (ext, rx, client) = extension();
    register_agent(&ext, "agent-a");
    let message_ref = "slack-message:test-c123-remove-boundary";
    install_source_reaction_target(&ext, message_ref);
    let key = ReactionKey {
        channel_id: "C123".to_owned(),
        message_ts: "1.0".to_owned(),
        emoji: "eyes".to_owned(),
    };
    ext.state.lock().expect("state").reactions.owners.insert(
        key.clone(),
        ReactionOwner {
            agent_id: agent_id("agent-a"),
            message_ref: MessageFactId::new(message_ref),
        },
    );
    let (reached_tx, reached_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    *ext.test_hooks
        .reaction_result_boundary
        .lock()
        .expect("hook") = Some(BlockingTestHook {
        reached: reached_tx,
        release: release_rx,
    });
    let ext = Arc::new(ext);
    let removing = Arc::clone(&ext);
    let removal = std::thread::spawn(move || {
        removing.handle_react_event(tool_call(
            REACT_TOOL_NAME,
            "agent-a",
            "reaction-remove-boundary",
            reaction_args(message_ref, "eyes", "remove"),
        ))
    });
    reached_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("confirmed output boundary");
    {
        let state = ext.state.lock().expect("state");
        assert!(state.reactions.owners.contains_key(&key));
        assert!(
            state
                .reactions
                .attempts
                .values()
                .any(|attempt| matches!(attempt.disposition, ReactionAttemptDisposition::InFlight))
        );
    }
    release_tx.send(()).expect("release result boundary");
    assert!(matches!(
        removal.join().expect("removal"),
        Event::ToolResult(_)
    ));
    assert_eq!(client.reactions.lock().expect("reactions").len(), 1);
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(1))
            .expect("reported removal result"),
        HarnessInputMessage::Emit(emit)
            if matches!(emit.event.as_ref(), Event::ToolResultReported(_))
    ));
    let state = ext.state.lock().expect("state");
    assert!(!state.reactions.owners.contains_key(&key));
    assert!(
        state
            .reactions
            .attempts
            .values()
            .any(|attempt| matches!(attempt.disposition, ReactionAttemptDisposition::Success(_)))
    );
}

/// Slack idempotency outcomes never adopt an unowned shared-bot reaction, while
/// an owned missing reaction is a successful local removal.
#[test]
fn reaction_idempotency_errors_respect_local_ownership() {
    let (ext, _rx, client) = extension();
    register_agent(&ext, "agent-a");
    install_source_reaction_target(&ext, "slack-message:test-c123-1.0");

    client.push_reaction_result(Err(ReactionApiError::AlreadyReacted));
    assert!(matches!(
        ext.handle_react_event(tool_call(
            REACT_TOOL_NAME,
            "agent-a",
            "already",
            reaction_args("slack-message:test-c123-1.0", "eyes", "add"),
        )),
        Event::ToolError(_)
    ));
    assert!(ext.state.lock().expect("state").reactions.owners.is_empty());

    client.push_reaction_result(Ok(()));
    assert!(matches!(
        ext.handle_react_event(tool_call(
            REACT_TOOL_NAME,
            "agent-a",
            "add-ok",
            reaction_args("slack-message:test-c123-1.0", "eyes", "add"),
        )),
        Event::ToolResult(_)
    ));
    client.push_reaction_result(Err(ReactionApiError::NoReaction));
    assert!(matches!(
        ext.handle_react_event(tool_call(
            REACT_TOOL_NAME,
            "agent-a",
            "remove-missing",
            reaction_args("slack-message:test-c123-1.0", "eyes", "remove"),
        )),
        Event::ToolResult(_)
    ));
    assert!(ext.state.lock().expect("state").reactions.owners.is_empty());

    client.push_reaction_result(Err(ReactionApiError::OutcomeUnknown));
    assert!(matches!(
        ext.handle_react_event(tool_call(
            REACT_TOOL_NAME,
            "agent-a",
            "ambiguous-add",
            reaction_args("slack-message:test-c123-1.0", "wave", "add"),
        )),
        Event::ToolError(_)
    ));
    assert!(ext.state.lock().expect("state").reactions.owners.is_empty());
}

/// Malformed fields, native selectors, unknown refs, and ambiguous failures
/// fail closed without ownership adoption or automatic retry.
#[test]
fn reaction_validation_and_ambiguous_failure_are_fail_closed() {
    let (ext, _rx, client) = extension();
    register_agent(&ext, "agent-a");
    for (index, args) in [
        reaction_args("C123", "eyes", "add"),
        reaction_args("", "eyes", "add"),
        reaction_args("msg", ":eyes:", "add"),
        reaction_args("msg", "eyes", "toggle"),
        CborValue::Map(vec![
            example_field("message_ref", example_text("msg")),
            example_field("emoji", example_text("eyes")),
            example_field("action", example_text("add")),
            example_field("timestamp", example_text("1.0")),
        ]),
    ]
    .into_iter()
    .enumerate()
    {
        assert!(matches!(
            ext.handle_react_event(tool_call(
                REACT_TOOL_NAME,
                "agent-a",
                &format!("invalid-{index}"),
                args,
            )),
            Event::ToolError(_)
        ));
    }
    assert!(client.reactions.lock().expect("lock").is_empty());
}

/// Discovery returns sorted static route metadata while excluding every native
/// route, dynamic link, identity, and runtime-state field.
#[test]
fn conversation_discovery_exposes_only_configured_model_facing_policy() {
    let (tx, _rx) = mpsc::channel();
    let ext = Extension::new(FakeClient::new(), tx);
    let mut config = proactive_cfg();
    config.conversations.insert(
        "receive-all".to_owned(),
        ConversationPolicy {
            alias: "receive-all".to_owned(),
            conversation_id: "C888".to_owned(),
            kind: ConversationPolicyKind::Channel,
            receive: Some(ReceiveMode::AllMessages),
            description: Some("Receive-only hint".to_owned()),
            thread_ts: None,
        },
    );
    apply_test_config(&ext, config);
    {
        let mut state = ext.state.lock().expect("lock");
        state.linked_dms.insert(
            "DSECRET".to_owned(),
            LinkedConversation {
                user_id: "USECRET".to_owned(),
            },
        );
        state.registered_agents.insert(agent_id("agent-secret"));
    }
    let event = ext.handle_conversations(tool(
        CONVERSATIONS_TOOL_NAME,
        "agent-a",
        CborValue::Map(Vec::new()),
    ));
    let Event::ToolResult(result) = event else {
        panic!("discovery should succeed");
    };
    let root = result.result;
    assert_eq!(cbor_map_keys(&root), ["conversations"]);
    let conversations =
        tau_proto::cbor_array_field(&root, "conversations").expect("conversation array");
    let aliases = discovery_aliases(&root);
    assert_eq!(
        aliases,
        [
            "alice-dm",
            "incident-thread",
            "receive-all",
            "team",
            "team-ops"
        ]
    );
    for record in conversations {
        let alias = tau_proto::cbor_text_field(record, "alias").expect("alias");
        let (expected_kind, expected_scope) = match alias.as_str() {
            "alice-dm" => ("dm", "conversation"),
            "incident-thread" => ("mpim", "fixed_thread"),
            "receive-all" | "team" | "team-ops" => ("channel", "conversation"),
            other => panic!("unexpected alias {other}"),
        };
        let expected_keys = if matches!(alias.as_str(), "receive-all" | "team-ops") {
            vec!["alias", "description", "kind", "policy", "scope"]
        } else {
            vec!["alias", "kind", "policy", "scope"]
        };
        assert_eq!(cbor_map_keys(record), expected_keys);
        assert_eq!(
            tau_proto::cbor_text_field(record, "kind").as_deref(),
            Some(expected_kind)
        );
        assert_eq!(
            tau_proto::cbor_text_field(record, "scope").as_deref(),
            Some(expected_scope)
        );
        let policy = tau_proto::cbor_field(record, "policy").expect("policy");
        assert_eq!(cbor_map_keys(policy), ["proactive_send", "receive"]);
        assert!(
            matches!(
                tau_proto::cbor_field(policy, "receive"),
                Some(CborValue::Null)
                    if matches!(alias.as_str(), "alice-dm" | "incident-thread" | "team-ops")
            ) || matches!(
                tau_proto::cbor_field(policy, "receive"),
                Some(CborValue::Text(receive)) if alias == "team" && receive == "mentions_only"
            ) || matches!(
                tau_proto::cbor_field(policy, "receive"),
                Some(CborValue::Text(receive))
                    if alias == "receive-all" && receive == "all_messages"
            )
        );
        assert_eq!(
            tau_proto::cbor_bool_field(policy, "proactive_send"),
            Some(!matches!(alias.as_str(), "receive-all" | "team"))
        );
    }
    let team_ops = conversations
        .iter()
        .find(|record| tau_proto::cbor_text_field(record, "alias").as_deref() == Some("team-ops"))
        .expect("team-ops");
    assert_eq!(
        tau_proto::cbor_text_field(team_ops, "description").as_deref(),
        Some("Trusted ops hint")
    );
    let receive_all = conversations
        .iter()
        .find(|record| {
            tau_proto::cbor_text_field(record, "alias").as_deref() == Some("receive-all")
        })
        .expect("receive-all");
    assert_eq!(
        tau_proto::cbor_text_field(receive_all, "description").as_deref(),
        Some("Receive-only hint")
    );
    let encoded = serde_json::to_vec(&root).expect("serialize result");
    assert!(encoded.len() <= MAX_DISCOVERY_RESULT_BYTES);
    let text = format!("{root:?}");
    for private in [
        "C123",
        "C456",
        "C888",
        "G789",
        "D123",
        "1720000000.123456",
        "DSECRET",
        "USECRET",
        "agent-secret",
        "conversation_id",
        "thread_ts",
    ] {
        assert!(!text.contains(private), "discovery leaked {private}");
    }
    let state = ext.state.lock().expect("lock");
    assert!(!state.config_frozen);
    assert!(!state.worker_started);
}

/// A configuration containing only private dynamic-DM policy has no static
/// inventory and discovery remains a side-effect-free empty local read.
#[test]
fn conversation_discovery_excludes_dynamic_only_policy() {
    let (tx, _rx) = mpsc::channel();
    let ext = Extension::new(FakeClient::new(), tx);
    let mut config = cfg();
    config.conversations.clear();
    config.parent_receives.clear();
    config.proactive_aliases.clear();
    config.dynamic_direct_messages = Some(DynamicDirectMessages {
        receive: ReceiveMode::AllMessages,
    });
    apply_test_config(&ext, config);
    let event = ext.handle_conversations(tool(
        CONVERSATIONS_TOOL_NAME,
        "agent-a",
        CborValue::Map(Vec::new()),
    ));
    let Event::ToolResult(result) = event else {
        panic!("discovery should succeed");
    };
    assert_eq!(cbor_map_keys(&result.result), ["conversations"]);
    assert!(
        tau_proto::cbor_array_field(&result.result, "conversations")
            .expect("conversations")
            .is_empty()
    );
    assert!(!format!("{:?}", result.result).contains(DYNAMIC_DM_LABEL));
    let state = ext.state.lock().expect("lock");
    assert!(!state.worker_started);
    assert!(!state.config_frozen);
}

/// Pagination enforces strict bounds, opaque current-config cursors, and exact
/// continuation without duplicates.
#[test]
fn conversation_discovery_paginates_and_rejects_invalid_requests() {
    let longest_alias = format!("a{}", "z".repeat(63));
    let longest_cursor = encode_discovery_cursor(&longest_alias);
    assert!(longest_cursor.len() <= MAX_DISCOVERY_CURSOR_BYTES);
    assert_eq!(
        decode_discovery_cursor(&longest_cursor).expect("maximum cursor"),
        longest_alias
    );

    let (tx, _rx) = mpsc::channel();
    let ext = Extension::new(FakeClient::new(), tx);
    apply_test_config(&ext, proactive_cfg());
    let first = ext.handle_conversations(tool(
        CONVERSATIONS_TOOL_NAME,
        "agent-a",
        tau_proto::json_to_cbor(&serde_json::json!({"limit": 2})),
    ));
    let Event::ToolResult(first) = first else {
        panic!("first page");
    };
    let first = first.result;
    let cursor = tau_proto::cbor_text_field(&first, "next_cursor").expect("continuation cursor");
    assert!(!cursor.contains("incident-thread"));
    let second = ext.handle_conversations(tool(
        CONVERSATIONS_TOOL_NAME,
        "agent-a",
        tau_proto::json_to_cbor(&serde_json::json!({"limit": 2, "cursor": cursor})),
    ));
    let Event::ToolResult(second) = second else {
        panic!("second page");
    };
    let second = second.result;
    assert!(tau_proto::cbor_field(&second, "next_cursor").is_none());
    let first_aliases = discovery_aliases(&first);
    let second_aliases = discovery_aliases(&second);
    assert_eq!(first_aliases, ["alice-dm", "incident-thread"]);
    assert_eq!(second_aliases, ["team", "team-ops"]);
    let all_aliases = first_aliases
        .iter()
        .chain(&second_aliases)
        .collect::<BTreeSet<_>>();
    assert_eq!(all_aliases.len(), 4, "pages must not duplicate aliases");

    for arguments in [
        serde_json::json!({"limit": 0}),
        serde_json::json!({"limit": 33}),
        serde_json::json!({"limit": "2"}),
        serde_json::json!({"cursor": "not-a-cursor"}),
        serde_json::json!({"cursor": "x".repeat(MAX_DISCOVERY_CURSOR_BYTES + 1)}),
        serde_json::json!({"cursor": encode_discovery_cursor("missing")}),
        serde_json::json!({"unknown": true}),
    ] {
        assert!(matches!(
            ext.handle_conversations(tool(
                CONVERSATIONS_TOOL_NAME,
                "agent-a",
                tau_proto::json_to_cbor(&arguments)
            )),
            Event::ToolError(_)
        ));
    }
}

/// Default and maximum page sizes remain bounded, and the largest valid static
/// inventory with worst-case descriptions stays within the serialized cap.
#[test]
fn conversation_discovery_enforces_page_and_serialized_output_bounds() {
    let (tx, _rx) = mpsc::channel();
    let ext = Extension::new(FakeClient::new(), tx);
    let mut config = cfg();
    config.conversations.clear();
    config.proactive_aliases.clear();
    config.parent_receives.clear();
    for index in 0..CONVERSATION_LIMIT {
        let alias = format!("route-{index:02}");
        config.conversations.insert(
            alias.clone(),
            ConversationPolicy {
                alias: alias.clone(),
                conversation_id: format!("C{index:08}"),
                kind: ConversationPolicyKind::Channel,
                receive: Some(ReceiveMode::AllMessages),
                description: Some("🦀".repeat(120)),
                thread_ts: None,
            },
        );
        config.proactive_aliases.insert(alias);
    }
    apply_test_config(&ext, config);

    for (arguments, expected_len) in [
        (serde_json::json!({}), DEFAULT_DISCOVERY_PAGE_LIMIT),
        (
            serde_json::json!({"limit": MAX_DISCOVERY_PAGE_LIMIT}),
            MAX_DISCOVERY_PAGE_LIMIT,
        ),
    ] {
        let Event::ToolResult(result) = ext.handle_conversations(tool(
            CONVERSATIONS_TOOL_NAME,
            "agent-a",
            tau_proto::json_to_cbor(&arguments),
        )) else {
            panic!("discovery page");
        };
        assert_eq!(discovery_aliases(&result.result).len(), expected_len);
        assert!(tau_proto::cbor_field(&result.result, "next_cursor").is_some());
        assert!(
            serde_json::to_vec(&result.result)
                .expect("serialize result")
                .len()
                <= MAX_DISCOVERY_RESULT_BYTES
        );
    }
}
/// Unified proactive conversation policy rejects ambiguous or unsafe shapes
/// while accepting channel, MPIM, DM, and fixed-thread forms.
#[test]
fn proactive_conversation_config_validation_matrix() {
    let mut secrets = BTreeMap::new();
    secrets.insert("app".to_owned(), tau_proto::SecretValue::new("xapp-test"));
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("xoxb-test"));
    let validate = |destinations: serde_json::Value| {
        tau_proto::json_to_cbor(&serde_json::json!({
            "app_token_secret": "app",
            "bot_token_secret": "bot",
            "allowed_user_ids": ["U123"],
            "conversations": destinations,
        }))
        .deserialized::<ExtConfig>()
        .map_err(|error| format!("{error:?}"))
        .and_then(|config| config.validate(&secrets).map(|_| ()))
    };
    assert!(
        validate(serde_json::json!([
            {"alias":"chan","conversation_id":"C123","kind":"channel","proactive_send":true},
            {"alias":"private","conversation_id":"G123","kind":"channel","thread_ts":"1.000001","proactive_send":true},
            {"alias":"group","conversation_id":"G456","kind":"mpim","proactive_send":true},
            {"alias":"direct","conversation_id":"D123","kind":"dm","description":"Alice","proactive_send":true}
        ]))
        .is_ok()
    );
    for (invalid, expected) in [
        (
            serde_json::json!([{"alias":"Bad","conversation_id":"C123","kind":"channel","proactive_send":true}]),
            "alias",
        ),
        (
            serde_json::json!([{"alias":"same","conversation_id":"C123","kind":"channel","proactive_send":true},{"alias":"same","conversation_id":"C456","kind":"channel","proactive_send":true}]),
            "duplicate alias",
        ),
        (
            serde_json::json!([{"alias":"one","conversation_id":"C123","kind":"channel","proactive_send":true},{"alias":"two","conversation_id":"C123","kind":"channel","proactive_send":true}]),
            "duplicate exact native route",
        ),
        (
            serde_json::json!([{"alias":"dm","conversation_id":"U123","kind":"dm","proactive_send":true}]),
            "never U/W",
        ),
        (
            serde_json::json!([{"alias":"dm","conversation_id":"C123","kind":"dm","proactive_send":true}]),
            "kind does not match",
        ),
        (
            serde_json::json!([{"alias":"mpim","conversation_id":"D123","kind":"mpim","proactive_send":true}]),
            "kind does not match",
        ),
        (
            serde_json::json!([{"alias":"thread","conversation_id":"C123","kind":"channel","thread_ts":"bad","proactive_send":true}]),
            "thread_ts",
        ),
        (
            serde_json::json!([{"alias":"thread","conversation_id":"C123","kind":"channel","thread_ts":" 1.0","proactive_send":true}]),
            "thread_ts",
        ),
        (
            serde_json::json!([{"alias":"blank","conversation_id":"C123","kind":"channel","description":"  ","proactive_send":true}]),
            "description",
        ),
        (
            serde_json::json!([{"alias":"control","conversation_id":"C123","kind":"channel","description":"bad\nline","proactive_send":true}]),
            "description",
        ),
        (
            serde_json::json!([{"alias":"long","conversation_id":"C123","kind":"channel","description":"x".repeat(121),"proactive_send":true}]),
            "description",
        ),
        (
            serde_json::json!([{"alias":"unknown","conversation_id":"C123","kind":"channel","proactive_send":true,"extra":true}]),
            "unknown field",
        ),
    ] {
        let error = validate(invalid).expect_err("invalid policy");
        assert!(
            error.contains(expected),
            "{error:?} did not contain {expected}"
        );
    }
    let too_many = (0..=CONVERSATION_LIMIT)
        .map(|index| {
            serde_json::json!({
                "alias": format!("route-{index}"),
                "conversation_id": format!("C{index:03}"),
                "kind": "channel"
            })
        })
        .collect();
    assert!(validate(serde_json::Value::Array(too_many)).is_err());
    assert!(validate(serde_json::json!([])).is_err());
}

/// Sender aliases are bounded one-to-one operator presentation, not admission.
#[test]
fn sender_alias_config_is_bounded_unique_and_strict() {
    let mut secrets = BTreeMap::new();
    secrets.insert("app".to_owned(), tau_proto::SecretValue::new("xapp-test"));
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("xoxb-test"));
    let validate = |aliases: serde_json::Value| {
        tau_proto::json_to_cbor(&serde_json::json!({
            "app_token_secret": "app",
            "bot_token_secret": "bot",
            "allowed_user_ids": ["U123"],
            "sender_aliases": aliases,
            "conversations": [{
                "alias":"team",
                "conversation_id":"C123",
                "kind":"channel",
                "receive":"all_messages"
            }]
        }))
        .deserialized::<ExtConfig>()
        .map_err(|error| format!("{error:?}"))
        .and_then(|config| config.validate(&secrets))
    };
    let config = validate(serde_json::json!([
        {"user_id":"U123","alias":"dpc"},
        {"user_id":"W456","alias":"alice-2"}
    ]))
    .expect("valid aliases");
    assert_eq!(
        config.sender_aliases.get("U123").map(String::as_str),
        Some("dpc")
    );
    for aliases in [
        serde_json::json!([{"user_id":"U123","alias":"Bad"}]),
        serde_json::json!([
            {"user_id":"U123","alias":"dpc"},
            {"user_id":"U123","alias":"alice"}
        ]),
        serde_json::json!([
            {"user_id":"U123","alias":"dpc"},
            {"user_id":"W456","alias":"dpc"}
        ]),
        serde_json::json!([{"user_id":"U123","alias":"dpc","extra":true}]),
    ] {
        assert!(validate(aliases).is_err());
    }
    let over_limit = (0..65)
        .map(|index| {
            serde_json::json!({
                "user_id": format!("U{index:03}"),
                "alias": format!("user-{index}")
            })
        })
        .collect::<Vec<_>>();
    assert!(validate(serde_json::Value::Array(over_limit)).is_err());
}

/// Replacing still-mutable configuration discards any preflight installation
/// observation so new credentials cannot inherit the old bot/workspace pair.
#[test]
fn mutable_config_replacement_clears_installation_preflight() {
    let (ext, _rx, _client) = extension();
    {
        let state = ext.state.lock().expect("state");
        assert_eq!(state.bot_user_id.as_deref(), Some("UBOT123"));
        assert_eq!(state.installation_team_id.as_deref(), Some("T123"));
        assert!(!state.config_frozen);
    }
    ext.apply_config(cfg()).expect("replace mutable config");
    let state = ext.state.lock().expect("state");
    assert_eq!(state.bot_user_id, None);
    assert_eq!(state.installation_team_id, None);
}

/// Config validation requires both token secret names, non-empty resolved
/// secret values, and a non-empty user allowlist before Slack can be contacted.
#[test]
fn config_rejects_missing_tokens_or_empty_allowlist() {
    let err = ExtConfig::default()
        .validate(&BTreeMap::new())
        .err()
        .expect("missing app token");
    assert!(err.contains("app_token_secret"));

    let mut secrets = BTreeMap::new();
    secrets.insert("app".to_owned(), tau_proto::SecretValue::new("xapp-test"));
    let err = ExtConfig {
        app_token_secret: Some("app".to_owned()),
        bot_token_secret: Some("bot".to_owned()),
        allowed_user_ids: vec!["U123".to_owned()],
        ..Default::default()
    }
    .validate(&secrets)
    .err()
    .expect("missing bot token");
    assert!(err.contains("bot token secret`bot`") || err.contains("bot token secret `bot`"));

    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new(""));
    let err = ExtConfig {
        app_token_secret: Some("app".to_owned()),
        bot_token_secret: Some("bot".to_owned()),
        allowed_user_ids: vec!["U123".to_owned()],
        ..Default::default()
    }
    .validate(&secrets)
    .err()
    .expect("empty bot token");
    assert!(err.contains("missing or empty"));

    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("xoxb-test"));
    let err = ExtConfig {
        app_token_secret: Some("app".to_owned()),
        bot_token_secret: Some("bot".to_owned()),
        ..Default::default()
    }
    .validate(&secrets)
    .err()
    .expect("empty allowlist");
    assert!(err.contains("allowed_user_ids"));
}

/// Unknown config keys are rejected instead of being silently ignored, because
/// a typo in chat-bridge policy should surface as a harness ConfigError.
#[test]
fn config_rejects_unknown_fields() {
    let value = tau_proto::json_to_cbor(&serde_json::json!({
        "app_token_secret": "app",
        "bot_token_secret": "bot",
        "allowed_user_ids": ["U123"],
        "destination": "C123"
    }));
    let err = value
        .deserialized::<ExtConfig>()
        .map_err(|error| format!("{error:?}"))
        .expect_err("unknown field");
    assert!(err.contains("unknown field"));
    assert!(err.contains("destination"));
}

/// Duplicate user or channel ids are most likely policy mistakes and must
/// become visible configuration errors rather than silently collapsing.
#[test]
fn config_rejects_duplicate_allowlist_entries() {
    let mut secrets = BTreeMap::new();
    secrets.insert("app".to_owned(), tau_proto::SecretValue::new("xapp-test"));
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("xoxb-test"));

    let error = ExtConfig {
        app_token_secret: Some("app".to_owned()),
        bot_token_secret: Some("bot".to_owned()),
        allowed_user_ids: vec!["U123".to_owned(), "U123".to_owned()],
        ..Default::default()
    }
    .validate(&secrets)
    .err()
    .expect("duplicate id must fail");
    assert!(error.contains("duplicate id"), "{error}");
}

/// Empty and malformed ids in either security allowlist fail validation rather
/// than being trimmed away or accepted as unusable policy entries.
#[test]
fn config_rejects_empty_and_malformed_allowlist_ids() {
    let mut secrets = BTreeMap::new();
    secrets.insert("app".to_owned(), tau_proto::SecretValue::new("xapp-test"));
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("xoxb-test"));
    for users in [vec!["".to_owned()], vec!["user-lower".to_owned()]] {
        let error = ExtConfig {
            app_token_secret: Some("app".to_owned()),
            bot_token_secret: Some("bot".to_owned()),
            allowed_user_ids: users,
            ..Default::default()
        }
        .validate(&secrets)
        .err()
        .expect("invalid id must fail");
        assert!(error.contains("invalid Slack id") || error.contains("empty ids"));
    }
}

/// The obsolete singular destination key is rejected so operators cannot
/// believe a channel is authorized when the extension ignored it.
#[test]
fn config_rejects_singular_channel_id() {
    let value = tau_proto::json_to_cbor(&serde_json::json!({
        "app_token_secret": "app",
        "bot_token_secret": "bot",
        "allowed_user_ids": ["U123"],
        "channel_id": "C123"
    }));
    let error = value
        .deserialized::<ExtConfig>()
        .map_err(|error| format!("{error:?}"))
        .expect_err("obsolete singular key");
    assert!(error.contains("unknown field"));
    assert!(error.contains("channel_id"));
}

/// Slack Web API endpoint overrides must not downgrade production traffic or
/// smuggle credentials/query data into diagnostics; loopback HTTP remains
/// usable for tests.
#[test]
fn config_rejects_unsafe_api_base_overrides() {
    let mut secrets = BTreeMap::new();
    secrets.insert("app".to_owned(), tau_proto::SecretValue::new("xapp-test"));
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("xoxb-test"));

    for api_base in [
        "http://example.com/api",
        "https://user@example.com/api",
        "https://example.com/api?debug=1",
        "https://example.com/api#frag",
    ] {
        let err = ExtConfig {
            app_token_secret: Some("app".to_owned()),
            bot_token_secret: Some("bot".to_owned()),
            allowed_user_ids: vec!["U123".to_owned()],
            api_base: Some(api_base.to_owned()),
            ..Default::default()
        }
        .validate(&secrets)
        .err()
        .expect("unsafe api base");
        assert!(err.contains("api_base"));
    }

    let cfg = ExtConfig {
        app_token_secret: Some("app".to_owned()),
        bot_token_secret: Some("bot".to_owned()),
        allowed_user_ids: vec!["U123".to_owned()],
        dynamic_direct_messages: Some(DynamicDirectMessages {
            receive: ReceiveMode::AllMessages,
        }),
        api_base: Some("http://127.0.0.1:8080/api".to_owned()),
        ..Default::default()
    }
    .validate(&secrets)
    .expect("loopback accepted");
    assert_eq!(cfg.api_base, "http://127.0.0.1:8080/api");
}

/// Once Socket Mode owns a config snapshot, reconfiguration must fail closed
/// and leave active credentials/routing untouched until Tau restarts.
#[test]
fn config_after_worker_start_is_rejected() {
    let (ext, _rx, _client) = extension();
    ext.state.lock().expect("lock").config_frozen = true;
    let mut new_cfg = cfg();
    new_cfg
        .conversations
        .get_mut("team")
        .expect("team")
        .conversation_id = "C999".to_owned();
    let err = ext.apply_config(new_cfg).expect_err("locked config");
    assert!(err.contains("restart Tau"));
    assert_eq!(
        ext.state.lock().expect("lock").config.as_ref().map(|cfg| {
            cfg.conversations
                .values()
                .any(|policy| policy.conversation_id == "C123")
        }),
        Some(true)
    );
}

/// Before worker startup, invalid reconfiguration clears inactive config and
/// registrations so stale credentials or destinations cannot remain live.
#[test]
fn invalid_pre_start_reconfiguration_clears_inactive_state() {
    let (ext, _rx, _client) = extension();
    register_agent(&ext, "agent-a");
    ext.remember_posted_message(
        slack_conversation("C123", Some("10.0")),
        PostedMessage {
            channel_id: "C123".to_owned(),
            ts: "1.0".to_owned(),
            thread_ts: None,
        },
        agent_id("agent-a"),
    );
    ext.clear_config_after_error();
    let state = ext.state.lock().expect("lock");
    assert!(state.config.is_none());
    assert!(state.registered_agents.is_empty());
    assert!(
        state
            .posted_messages
            .get(&PostedMessageKey::new("C123", "1.0"))
            .is_none()
    );
}

/// Protocol migration regression: a malformed pre-start config must emit a
/// parse `ConfigError`, clear inactive config, and prevent a later registration
/// from starting Slack with stale credentials.
#[test]
fn run_malformed_pre_start_config_clears_inactive_state() {
    let client = FakeClient::new();
    let frames = run_protocol_messages(
        &[
            valid_config_message(),
            malformed_config_message(),
            HarnessOutputMessage::deliver(Event::ToolStarted(tool(
                REGISTER_TOOL_NAME,
                "agent-a",
                bool_args(true),
            ))),
        ],
        client.clone(),
    );

    assert!(
        frames.iter().any(|frame| matches!(
            frame,
            HarnessInputMessage::ConfigError(error)
                if error.message.contains("unknown_field")
        )),
        "malformed config should be reported"
    );
    assert!(
        frames.iter().any(|frame| matches!(
            frame,
            HarnessInputMessage::Emit(emit)
                if matches!(
                     emit.event.as_ref(),
                     Event::ToolErrorReported(error)
                         if error.tool_name.as_str() == REGISTER_TOOL_NAME
                            && error.message.contains("not configured")
                 )
        )),
        "register should fail after malformed config clears active config"
    );
    assert_eq!(*client.auth_count.lock().expect("lock"), 0);
    assert_eq!(*client.open_count.lock().expect("lock"), 0);
}

/// Configuration changes never refresh or expand the fixed send schema.
#[test]
fn run_config_does_not_refresh_send_schema() {
    let frames = run_protocol_messages(
        &[proactive_config_message(), malformed_config_message()],
        FakeClient::new(),
    );
    let registrations = frames
        .iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::Emit(emit) => match emit.event.as_ref() {
                Event::ToolRegistrationDeclared(register)
                    if register.tool.name.as_str() == SEND_TOOL_NAME =>
                {
                    Some(&register.tool)
                }
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(registrations.len(), 1);
    let schema = registrations[0].parameters.as_ref().expect("fixed schema");
    assert!(schema["properties"]["destination"].get("enum").is_none());
    assert_eq!(schema["required"], serde_json::json!(["message"]));
}

/// The one static send declaration precedes readiness and has no alias enum.
#[test]
fn initial_send_schema_is_static_before_ready() {
    let frames = run_protocol_messages(&[proactive_config_message()], FakeClient::new());
    let registrations = frames
        .iter()
        .enumerate()
        .filter_map(|(index, frame)| match frame {
            HarnessInputMessage::Emit(emit) => match emit.event.as_ref() {
                Event::ToolRegistrationDeclared(register)
                    if register.tool.name.as_str() == SEND_TOOL_NAME =>
                {
                    Some((index, &register.tool))
                }
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(registrations.len(), 1);
    let (registration_index, final_tool) = registrations[0];
    assert!(
        final_tool
            .parameters
            .as_ref()
            .is_some_and(|schema| schema["properties"]["destination"].get("enum").is_none())
    );
    let ready_index = frames
        .iter()
        .position(|frame| matches!(frame, HarnessInputMessage::Ready(_)))
        .expect("Ready");
    assert!(registration_index < ready_index);
}

/// Protocol migration regression: once Socket Mode has started, even malformed
/// config must return the immutable/restart-required error without clearing
/// active registration or Slack routing state.
#[test]
fn run_malformed_post_start_config_preserves_active_state() {
    let client = FakeClient::new();
    let frames = run_protocol_messages(
        &[
            valid_config_message(),
            HarnessOutputMessage::deliver(Event::ToolStarted(tool(
                REGISTER_TOOL_NAME,
                "agent-a",
                bool_args(true),
            ))),
            malformed_config_message(),
            HarnessOutputMessage::deliver(Event::ToolStarted(tool(
                SEND_TOOL_NAME,
                "agent-a",
                message_args("reply"),
            ))),
        ],
        client.clone(),
    );

    assert!(
        frames.iter().any(|frame| matches!(
            frame,
            HarnessInputMessage::ConfigError(error)
                if error.message.contains("configuration is frozen")
        )),
        "post-start malformed config should report immutable config"
    );
    assert!(
        !frames.iter().any(|frame| matches!(
            frame,
            HarnessInputMessage::ConfigError(error)
                if error.message.contains("unknown_field")
        )),
        "post-start config should not be parsed after worker startup"
    );
    assert!(client.sent.lock().expect("lock").is_empty());
    assert!(frames.iter().any(|frame| matches!(
        frame,
        HarnessInputMessage::Emit(emit)
            if matches!(
                emit.event.as_ref(),
                Event::ToolErrorReported(error)
                    if error.tool_name.as_str() == SEND_TOOL_NAME
                        && error.message.contains("unknown or stale")
            )
    )));
}

/// Replayed lifecycle events are ignored wholesale by the tau-client migration,
/// so historical session shutdown cannot clear a live Slack registration.
#[test]
fn run_replayed_lifecycle_event_does_not_clear_registration() {
    let client = FakeClient::new();
    let frames = run_protocol_messages(
        &[
            valid_config_message(),
            HarnessOutputMessage::deliver(Event::ToolStarted(tool(
                REGISTER_TOOL_NAME,
                "agent-a",
                bool_args(true),
            ))),
            HarnessOutputMessage::deliver_replay(
                tau_proto::UnixMicros::new(1_700_000_000_000_000),
                Event::SessionShutdown(tau_proto::SessionShutdown {
                    session_id: "s1"
                        .parse::<tau_proto::SessionId>()
                        .expect("known-safe SessionId must be valid"),
                }),
            ),
            HarnessOutputMessage::deliver(Event::ToolStarted(tool(
                SEND_TOOL_NAME,
                "agent-a",
                message_args("after replay"),
            ))),
        ],
        client.clone(),
    );

    assert!(client.sent.lock().expect("lock").is_empty());
    assert!(frames.iter().any(|frame| matches!(
        frame,
        HarnessInputMessage::Emit(emit)
            if matches!(
                emit.event.as_ref(),
                Event::ToolErrorReported(error)
                    if error.tool_name.as_str() == SEND_TOOL_NAME
                        && error.message.contains("unknown or stale")
            )
    )));
}

/// Bad tool arguments should emit a tool error and return `Ok(())` from the
/// tau-client handler so the runner continues to handle subsequent Slack tools.
#[test]
fn run_bad_tool_args_do_not_stop_runner() {
    let client = FakeClient::new();
    let frames = run_protocol_messages(
        &[
            valid_config_message(),
            HarnessOutputMessage::deliver(Event::ToolStarted(tool(
                SEND_TOOL_NAME,
                "agent-a",
                CborValue::Map(vec![(
                    CborValue::Text("channel_id".to_owned()),
                    CborValue::Text("C999".to_owned()),
                )]),
            ))),
            HarnessOutputMessage::deliver(Event::ToolStarted(tool(
                REGISTER_TOOL_NAME,
                "agent-a",
                bool_args(true),
            ))),
        ],
        client.clone(),
    );

    assert!(
        frames.iter().any(|frame| matches!(
            frame,
            HarnessInputMessage::Emit(emit)
                if matches!(
                    emit.event.as_ref(),
                    Event::ToolErrorReported(error)
                        if error.tool_name.as_str() == SEND_TOOL_NAME
                            && error.message.contains("unknown argument")
                )
        )),
        "bad send args should emit ToolError"
    );
    assert!(
        frames.iter().any(|frame| matches!(
            frame,
            HarnessInputMessage::Emit(emit)
                if matches!(
                    emit.event.as_ref(),
                    Event::ToolResultReported(result)
                        if result.tool_name.as_str() == REGISTER_TOOL_NAME
                )
        )),
        "runner should continue to later register tool"
    );
    assert_eq!(*client.auth_count.lock().expect("lock"), 1);
}

/// `slack_send` is available only after the calling agent registers, preventing
/// accidental replies from unrelated agents.
#[test]
fn slack_send_fails_before_register() {
    let (ext, _rx, _client) = extension();
    let event = ext.handle_send(tool(SEND_TOOL_NAME, "agent-a", message_args("hi")));
    let Some(Event::ToolError(err)) = event else {
        panic!("expected error");
    };
    assert!(err.message.contains("slack_register"));
}

/// Registration records the calling agent and lazily starts Socket Mode;
/// turning it off removes selections pointing at the same agent.
#[test]
fn slack_register_toggles_agent_and_starts_worker() {
    let (ext, _rx, _client) = extension();
    let result = ext.handle_register(tool(REGISTER_TOOL_NAME, "agent-a", bool_args(true)));
    let Event::ToolResult(result) = result else {
        panic!("expected structured registration result")
    };
    assert_eq!(
        result.result,
        CborValue::Map(vec![
            example_field("status", example_text("registered")),
            example_field(
                "incoming_transport_reference",
                example_text("@slack_bridge")
            ),
        ])
    );
    let encoded = serde_json::to_string(&result.result).expect("registration result");
    assert!(!encoded.contains("UBOT123"));
    assert!(!encoded.contains("T123"));
    {
        let state = ext.state.lock().expect("lock");
        assert!(state.worker_started);
        assert!(state.config_frozen);
        assert!(state.registered_agents.contains(&agent_id("agent-a")));
    }
    assert!(ext.apply_config(cfg()).is_err());
    ext.state
        .lock()
        .expect("lock")
        .selected_agent_by_route
        .insert(
            SelectionRouteKey::StaticAlias("team".to_owned()),
            agent_id("agent-a"),
        );
    ext.remember_posted_message(
        slack_conversation("C123", None),
        PostedMessage {
            channel_id: "C123".to_owned(),
            ts: "1.0".to_owned(),
            thread_ts: None,
        },
        agent_id("agent-a"),
    );
    let result = ext.handle_register(tool(REGISTER_TOOL_NAME, "agent-a", bool_args(false)));
    let Event::ToolResult(result) = result else {
        panic!("expected structured unregister result")
    };
    assert_eq!(
        result.result,
        CborValue::Map(vec![example_field("status", example_text("unregistered"))])
    );
    let state = ext.state.lock().expect("lock");
    assert!(!state.registered_agents.contains(&agent_id("agent-a")));
    assert!(state.selected_agent_by_route.is_empty());
    assert!(
        state
            .posted_messages
            .get(&PostedMessageKey::new("C123", "1.0"))
            .is_some()
    );
}

/// Canonical Slack skin-tone reactions are accepted, while malformed colon
/// suffixes and unsafe timestamp metadata fail closed.
#[test]
fn reaction_metadata_validation_matches_slack_grammar() {
    for valid in ["thumbsup", "thumbsup::skin-tone-2", "wave::skin-tone-6"] {
        assert!(validate_reaction_name(valid).is_ok(), "{valid}");
    }
    for invalid in [
        "",
        "thumbsup:",
        "thumbsup::",
        "thumbsup::skin-tone-1",
        "thumbsup::skin-tone-7",
        "thumbsup::skin-tone-2::extra",
        "bad reaction",
    ] {
        assert!(validate_reaction_name(invalid).is_err(), "{invalid}");
    }
    for valid in ["1.0", "1712345678.123456"] {
        assert!(validate_slack_ts(valid).is_ok(), "{valid}");
    }
    for invalid in ["", "1", ".1", "1.", "1.2.3", "x.1", "1. x"] {
        assert!(validate_slack_ts(invalid).is_err(), "{invalid}");
    }
}

/// Bot users remain unable to route reactions even if an operator mistakenly
/// includes their U-shaped bot user id in the allowlist.
#[test]
fn allowlisted_non_human_reaction_actor_is_rejected() {
    let (ext, rx, _client) = extension();
    {
        let mut state = ext.state.lock().expect("lock");
        state
            .config
            .as_mut()
            .expect("config")
            .allowed_user_ids
            .insert("UBOT999".to_owned());
    }
    register_agent(&ext, "agent-a");
    ext.remember_posted_message(
        slack_conversation("C123", None),
        PostedMessage {
            channel_id: "C123".to_owned(),
            ts: "1.0".to_owned(),
            thread_ts: None,
        },
        agent_id("agent-a"),
    );
    let mut reaction = slack_reaction("ER-BOT", "reaction_added", "C123", "1.0");
    reaction.user_id = "UBOT999".to_owned();
    ext.process_slack_reaction(reaction);
    assert!(rx.try_recv().is_err());
}

/// Slack post response parsing requires a canonical message timestamp and
/// retains only canonical optional thread metadata.
#[test]
fn posted_message_response_validates_identity_metadata() {
    assert!(posted_message_from_response(&serde_json::json!({})).is_err());
    assert!(
        posted_message_from_response(&serde_json::json!({ "channel": "C123", "ts": "bad" }))
            .is_err()
    );
    assert!(
        posted_message_from_response(&serde_json::json!({
            "channel": "C123",
            "ts": "12.34",
            "message": { "thread_ts": "not-a-ts" }
        }))
        .is_err()
    );
    let post = posted_message_from_response(&serde_json::json!({
        "channel": "C123",
        "ts": "12.34"
    }))
    .expect("valid message ts");
    assert_eq!(post.channel_id, "C123");
    assert_eq!(post.ts, "12.34");
    assert!(post.thread_ts.is_none());
}

/// Web API request serialization omits thread metadata for root posts and
/// includes a supplied validated root for threaded posts.
#[test]
fn post_message_body_serializes_optional_thread_context() {
    assert_eq!(
        post_message_body("C123", "root", None),
        serde_json::json!({
            "channel": "C123",
            "text": "root",
            "mrkdwn": true,
            "link_names": false
        })
    );
    assert_eq!(
        post_message_body("C123", "reply", Some("42.0")),
        serde_json::json!({
            "channel": "C123",
            "text": "reply",
            "thread_ts": "42.0",
            "mrkdwn": true,
            "link_names": false
        })
    );
}

/// Eviction, agent removal, and clear keep semantic post ownership synchronized
/// so stale message identities cannot reappear.
#[test]
fn posted_message_cache_eviction_and_cleanup_are_synchronized() {
    let agent_a = agent_id("agent-a");
    let agent_b = agent_id("agent-b");
    let mut cache = PostedMessageCache::new(2);
    for (ts, agent_id) in [
        ("1.0", agent_a.clone()),
        ("2.0", agent_b.clone()),
        ("3.0", agent_a.clone()),
    ] {
        cache.insert(
            PostedMessageKey::new("C123", ts),
            PostedMessageOwner {
                agent_id,
                message_id: MessageFactId::new(format!("slack-message:test-c123-{ts}")),
                thread_ts: None,
                conversation: slack_conversation("C123", None),
                installation_team_id: "T123".to_owned(),
            },
        );
    }
    assert!(cache.get(&PostedMessageKey::new("C123", "1.0")).is_none());
    assert!(cache.get(&PostedMessageKey::new("C123", "2.0")).is_some());
    assert!(cache.get(&PostedMessageKey::new("C123", "3.0")).is_some());
    cache.remove_agent(&agent_a);
    assert!(cache.get(&PostedMessageKey::new("C123", "2.0")).is_some());
    assert!(cache.get(&PostedMessageKey::new("C123", "3.0")).is_none());
    cache.clear();
    assert!(cache.get(&PostedMessageKey::new("C123", "2.0")).is_none());
}

/// Human verification fails closed when Slack omits account-type facts and
/// rejects deleted, bot, and app-user accounts.
#[test]
fn users_info_response_requires_explicit_live_human_facts() {
    assert!(human_user_from_response(&serde_json::json!({}), "U123").is_err());
    assert!(!human_user_from_response(&serde_json::json!({ "user": {} }), "U123").expect("shape"));
    assert!(
        human_user_from_response(
            &serde_json::json!({
                "user": { "id": "U123", "deleted": false, "is_bot": false, "is_app_user": false }
            }),
            "U123"
        )
        .expect("human")
    );
    for user in [
        serde_json::json!({ "id": "U123", "deleted": true, "is_bot": false }),
        serde_json::json!({ "id": "U123", "deleted": false, "is_bot": true }),
        serde_json::json!({ "id": "U123", "deleted": false, "is_bot": false, "is_app_user": true }),
        serde_json::json!({ "id": "U999", "deleted": false, "is_bot": false }),
        serde_json::json!({ "id": "U123", "deleted": "false", "is_bot": false }),
    ] {
        assert!(
            !human_user_from_response(&serde_json::json!({ "user": user }), "U123")
                .expect("account")
        );
    }
    assert!(
        !human_user_from_response(
            &serde_json::json!({
                "user": { "id": "USLACKBOT", "deleted": false, "is_bot": false }
            }),
            "USLACKBOT"
        )
        .expect("slackbot")
    );
}

/// A successful human lookup retains only a bounded, structurally safe,
/// presentation-only `profile.display_name` snapshot.
#[test]
fn users_info_identity_retains_only_safe_bounded_display_name() {
    let response = serde_json::json!({
        "user": {
            "id": "U123",
            "deleted": false,
            "is_bot": false,
            "is_app_user": false,
            "profile": {"display_name": " Alice "}
        }
    });
    let identity = verified_human_from_response(&response, "U123")
        .expect("valid response")
        .expect("human");
    assert_eq!(identity.user_id, "U123");
    assert_eq!(identity.display_name.as_deref(), Some("Alice"));
    for display_name in [
        "Alice\nadmin".to_owned(),
        "x".repeat(81),
        "🦀".repeat(65),
        "Alice\u{180e}admin".to_owned(),
        "Alice\u{fe0f}".to_owned(),
        "Alice\u{fff0}".to_owned(),
        "Alice\u{e0100}".to_owned(),
        "Alice\u{3164}".to_owned(),
    ] {
        let mut value = response.clone();
        value["user"]["profile"]["display_name"] = serde_json::Value::String(display_name);
        assert_eq!(
            verified_human_from_response(&value, "U123")
                .expect("shape")
                .expect("human")
                .display_name,
            None
        );
    }
}

/// Event wrappers bind to the authenticated installation via exact context
/// team or one unambiguous fallback authorization.
#[test]
fn event_installation_binding_is_exact_and_connect_safe() {
    let with_context = serde_json::json!({
        "payload": {"context_team_id":"T123", "authorizations":[]}
    });
    assert!(event_matches_installation(&with_context, "T123", "UBOT123"));
    assert!(!event_matches_installation(
        &with_context,
        "T999",
        "UBOT123"
    ));

    let authorization = serde_json::json!({
        "payload": {"authorizations":[{
            "team_id":"T123",
            "user_id":"UBOT123",
            "is_bot":true
        }]}
    });
    assert!(event_matches_installation(
        &authorization,
        "T123",
        "UBOT123"
    ));
    for invalid in [
        serde_json::json!({"payload": {}}),
        serde_json::json!({"payload": {"authorizations":[]}}),
        serde_json::json!({"payload": {"authorizations":[
            {"team_id":"T123","user_id":"UBOT123"},
            {"team_id":"T123","user_id":"UBOT123"}
        ]}}),
        serde_json::json!({"payload": {"authorizations":[
            {"team_id":"T999","user_id":"UBOT123"}
        ]}}),
        serde_json::json!({"payload": {"authorizations":[
            {"team_id":"T123","user_id":"UOTHER"}
        ]}}),
    ] {
        assert!(!event_matches_installation(&invalid, "T123", "UBOT123"));
    }
}

/// An `auth.test` response is unusable unless it contains both halves of the
/// bot-user/installing-team authority pair.
#[test]
fn auth_test_requires_both_bot_and_team_identity() {
    assert_eq!(
        installation_from_response(&serde_json::json!({
            "user_id":"UBOT123",
            "team_id":"T123"
        }))
        .expect("complete identity"),
        SlackInstallationIdentity {
            bot_user_id: "UBOT123".to_owned(),
            team_id: "T123".to_owned()
        }
    );
    for malformed in [
        serde_json::json!({"team_id":"T123"}),
        serde_json::json!({"user_id":"UBOT123"}),
        serde_json::json!({"user_id":null,"team_id":"T123"}),
        serde_json::json!({"user_id":"UBOT123","team_id":7}),
    ] {
        assert_eq!(
            installation_from_response(&malformed),
            Err(SlackApiError::MalformedResponse)
        );
    }
}

/// `users.info` must put its required user argument in a form body rather than
/// a JSON body, which Slack treats as a missing user and reports as
/// `user_not_found`.
#[test]
fn users_info_uses_form_encoding() {
    let listener = path_std_net::TcpListener::bind("127.0.0.1:0").expect("bind test API");
    let address = listener.local_addr().expect("test API address");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept users.info request");
        let mut request = Vec::new();
        loop {
            let mut chunk = [0_u8; 4096];
            let length = stream.read(&mut chunk).expect("read users.info request");
            request.extend_from_slice(&chunk[..length]);
            let text = String::from_utf8_lossy(&request);
            let Some(header_end) = text.find("\r\n\r\n") else {
                continue;
            };
            let content_length = text
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .and_then(|value| value.parse::<usize>().ok())
                .expect("request content length");
            if request.len() >= header_end + 4 + content_length {
                break;
            }
        }
        let request = String::from_utf8_lossy(&request);
        assert!(
            request.starts_with("POST /api/users.info HTTP/1.1\r\n"),
            "unexpected request line: {}",
            request.lines().next().unwrap_or_default()
        );
        assert!(request.contains("authorization: Bearer xoxb-test\r\n"));
        assert!(request.contains("content-type: application/x-www-form-urlencoded\r\n"));
        assert!(request.contains("\r\n\r\nuser=U123"));
        let body = r#"{"ok":true,"user":{"id":"U123","deleted":false,"is_bot":false,"is_app_user":false}}"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .expect("write users.info response");
    });
    let cfg = RuntimeConfig {
        api_base: format!("http://{address}/api"),
        ..cfg()
    };
    assert!(
        HttpSlackClient::default()
            .verified_human_identity(&cfg, "U123")
            .expect("form-encoded users.info")
            .is_some()
    );
    server.join().expect("users.info test server");
}

/// Production chat.postMessage parsing classifies rate limits, hostile
/// transient/unknown bodies, and route-integrity mismatches without leaking raw
/// provider text or granting unsafe retry.
#[test]
fn post_http_outcomes_are_typed_bounded_and_body_safe() {
    let listener = path_std_net::TcpListener::bind("127.0.0.1:0").expect("bind test API");
    let address = listener.local_addr().expect("test API address");
    let responses = [
        (429, Some("999999999999999999999999999999"), r#"{}"#),
        (
            500,
            None,
            r#"xoxb-secret <@U123> <!channel> <#C123> payload"#,
        ),
        (200, None, r#"{"ok":false,"error":"xoxb-secret-unknown"}"#),
        (200, None, r#"{"ok":true,"channel":"C999","ts":"1.0"}"#),
        (200, None, r#"{"ok":true,"channel":"C123","ts":"2.0"}"#),
    ];
    let server = std::thread::spawn(move || {
        for (status, retry_after, body) in responses {
            let (mut stream, _) = listener.accept().expect("accept post request");
            let mut request = Vec::new();
            loop {
                let mut chunk = [0_u8; 4096];
                let length = stream.read(&mut chunk).expect("read post request");
                request.extend_from_slice(&chunk[..length]);
                let text = String::from_utf8_lossy(&request);
                let Some(header_end) = text.find("\r\n\r\n") else {
                    continue;
                };
                let content_length = text
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .and_then(|value| value.parse::<usize>().ok())
                    .expect("post content length");
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&request);
            assert!(request.starts_with("POST /api/chat.postMessage HTTP/1.1\r\n"));
            assert!(request.contains("\"link_names\":false"));
            let reason = match status {
                200 => "OK",
                429 => "Too Many Requests",
                500 => "Internal Server Error",
                _ => "Error",
            };
            let retry = retry_after
                .map(|value| format!("retry-after: {value}\r\n"))
                .unwrap_or_default();
            write!(
                stream,
                "HTTP/1.1 {status} {reason}\r\n{retry}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .expect("write post response");
        }
    });
    let cfg = RuntimeConfig {
        api_base: format!("http://{address}/api"),
        ..cfg()
    };
    let client = HttpSlackClient::default();
    let root = FrozenPostBody::new(
        "C123",
        None,
        &SlackPostMode::agent("safe".to_owned(), None).expect("safe mode"),
    );
    let thread = FrozenPostBody::new(
        "C123",
        Some("9.0"),
        &SlackPostMode::agent("safe".to_owned(), None).expect("safe mode"),
    );
    assert_eq!(
        client.post_message(&cfg, &root),
        PostAttemptOutcome::RateLimited(send_delivery::MAX_RETRY_AFTER)
    );
    assert_eq!(
        client.post_message(&cfg, &root),
        PostAttemptOutcome::OutcomeUnknown(SendFailureCategory::ServiceUnavailable)
    );
    assert_eq!(
        client.post_message(&cfg, &root),
        PostAttemptOutcome::OutcomeUnknown(SendFailureCategory::MalformedResponse)
    );
    assert_eq!(
        client.post_message(&cfg, &root),
        PostAttemptOutcome::DefinitiveFailure(SendFailureCategory::ConflictingRoute)
    );
    assert_eq!(
        client.post_message(&cfg, &thread),
        PostAttemptOutcome::DefinitiveFailure(SendFailureCategory::ConflictingRoute)
    );
    server.join().expect("post error server");
    for category in [
        SendFailureCategory::ServiceUnavailable,
        SendFailureCategory::MalformedResponse,
        SendFailureCategory::ConflictingRoute,
    ] {
        let display = category.to_string();
        assert!(!display.contains("xoxb-secret"));
        assert!(!display.contains("<@U123>"));
        assert!(!display.contains("C999"));
    }
}

/// Reaction methods use only the exact JSON endpoint, bearer header, and three
/// approved body fields; add/remove do not send text, thread, or file
/// selectors.
#[test]
fn reactions_use_exact_json_wire_contract() {
    let listener = path_std_net::TcpListener::bind("127.0.0.1:0").expect("bind test API");
    let address = listener.local_addr().expect("test API address");
    let server = std::thread::spawn(move || {
        for (method, emoji) in [("reactions.add", "eyes"), ("reactions.remove", "+1")] {
            let (mut stream, _) = listener.accept().expect("accept reaction request");
            let mut request = Vec::new();
            loop {
                let mut chunk = [0_u8; 4096];
                let length = stream.read(&mut chunk).expect("read reaction request");
                request.extend_from_slice(&chunk[..length]);
                let text = String::from_utf8_lossy(&request);
                let Some(header_end) = text.find("\r\n\r\n") else {
                    continue;
                };
                let content_length = text
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .and_then(|value| value.parse::<usize>().ok())
                    .expect("content length");
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&request);
            assert!(request.starts_with(&format!("POST /api/{method} HTTP/1.1\r\n")));
            assert!(request.contains("authorization: Bearer xoxb-test\r\n"));
            assert!(request.contains("content-type: application/json\r\n"));
            let body = request.split("\r\n\r\n").nth(1).expect("JSON body");
            let value: serde_json::Value = serde_json::from_str(body).expect("valid JSON");
            assert_eq!(
                value,
                serde_json::json!({"channel":"C123","timestamp":"1.0","name":emoji})
            );
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-length: 11\r\nconnection: close\r\n\r\n{{\"ok\":true}}"
            )
            .expect("write reaction response");
        }
    });
    let cfg = RuntimeConfig {
        api_base: format!("http://{address}/api"),
        ..cfg()
    };
    let client = HttpSlackClient::default();
    client
        .react(&cfg, ReactionActionKind::Add, "C123", "1.0", "eyes")
        .expect("add");
    client
        .react(&cfg, ReactionActionKind::Remove, "C123", "1.0", "+1")
        .expect("remove");
    server.join().expect("reaction test server");
}

/// Reaction failures expose only typed safe categories: rate limits are
/// clamped, missing scope is actionable, and ambiguous bodies never leak.
#[test]
fn reaction_http_errors_are_typed_and_body_safe() {
    let listener = path_std_net::TcpListener::bind("127.0.0.1:0").expect("bind test API");
    let address = listener.local_addr().expect("test API address");
    let server = std::thread::spawn(move || {
        for response in [
            "HTTP/1.1 429 Too Many Requests\r\nretry-after: 999999\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
            "HTTP/1.1 200 OK\r\ncontent-length: 36\r\nconnection: close\r\n\r\n{\"ok\":false,\"error\":\"missing_scope\"}",
            "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 40\r\nconnection: close\r\n\r\nxoxb-secret CSECRET 123.456 raw body",
        ] {
            let (mut stream, _) = listener.accept().expect("accept reaction request");
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).expect("read request");
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        }
    });
    let cfg = RuntimeConfig {
        api_base: format!("http://{address}/api"),
        ..cfg()
    };
    let client = HttpSlackClient::default();
    assert_eq!(
        client.react(&cfg, ReactionActionKind::Add, "C123", "1.0", "eyes"),
        Err(ReactionApiError::RateLimited(3_600))
    );
    assert_eq!(
        client.react(&cfg, ReactionActionKind::Add, "C123", "1.0", "eyes"),
        Err(ReactionApiError::MissingScope)
    );
    assert_eq!(
        client.react(&cfg, ReactionActionKind::Add, "C123", "1.0", "eyes"),
        Err(ReactionApiError::OutcomeUnknown)
    );
    server.join().expect("reaction error server");
    for error in [
        reaction_error_message(
            Some(ReactionApiError::RateLimited(3_600)),
            ReactionActionKind::Add,
            true,
            false,
        ),
        reaction_error_message(
            Some(ReactionApiError::MissingScope),
            ReactionActionKind::Add,
            true,
            false,
        ),
        reaction_error_message(
            Some(ReactionApiError::OutcomeUnknown),
            ReactionActionKind::Add,
            true,
            false,
        ),
    ] {
        assert!(!error.contains("xoxb-secret"));
        assert!(!error.contains("CSECRET"));
        assert!(!error.contains("123.456"));
        assert!(!error.contains("raw body"));
    }
}

/// Reactions from unauthorized users, unconfigured conversations, or messages
/// not posted by this bridge never become prompts.
#[test]
fn reactions_outside_authorized_owned_posts_are_ignored() {
    let (ext, rx, _client) = extension();
    register_agent(&ext, "agent-a");
    ext.remember_posted_message(
        slack_conversation("C123", None),
        PostedMessage {
            channel_id: "C123".to_owned(),
            ts: "1.0".to_owned(),
            thread_ts: None,
        },
        agent_id("agent-a"),
    );
    let mut unauthorized = slack_reaction("ER1", "reaction_added", "C123", "1.0");
    unauthorized.user_id = "U999".to_owned();
    ext.process_slack_reaction(unauthorized);
    ext.process_slack_reaction(slack_reaction("ER2", "reaction_added", "C999", "1.0"));
    ext.process_slack_reaction(slack_reaction("ER3", "reaction_removed", "C123", "404.0"));
    assert!(rx.try_recv().is_err());
}

/// Blank and oversized outbound messages are rejected before Slack API calls so
/// tool diagnostics remain deterministic and bounded.
#[test]
fn slack_send_rejects_blank_and_oversized_messages() {
    let (ext, _rx, client) = extension();
    register_agent(&ext, "agent-a");
    assert!(matches!(
        ext.handle_send(tool(SEND_TOOL_NAME, "agent-a", message_args("   "))),
        Some(Event::ToolError(_))
    ));
    let long = "x".repeat(DEFAULT_MAX_MESSAGE_BYTES + 1);
    assert!(matches!(
        ext.handle_send(tool(SEND_TOOL_NAME, "agent-a", message_args(&long))),
        Some(Event::ToolError(_))
    ));
    assert!(client.sent.lock().expect("lock").is_empty());
}

/// Messages from users outside the allowlist are ignored with no prompt and no
/// Slack reply side effects.
#[test]
fn unallowed_user_produces_no_prompt_or_reply() {
    let (ext, rx, client) = extension();
    register_agent(&ext, "agent-a");
    let mut msg = slack_message("C123", None, "<@UBOT123> hello");
    msg.user_id = "U999".to_owned();
    ext.process_slack_message(msg);
    assert!(rx.try_recv().is_err());
    assert!(client.sent.lock().expect("lock").is_empty());
}

/// Security mode omission is strict, both supported spellings deserialize, and
/// ambiguous values fail parsing rather than silently expanding ingress.
#[test]
fn security_mode_config_is_strict_by_default_and_rejects_invalid_values() {
    for (value, expected) in [
        (serde_json::json!({}), SecurityMode::Strict),
        (
            serde_json::json!({"security_mode": "strict"}),
            SecurityMode::Strict,
        ),
        (
            serde_json::json!({"security_mode": "lax"}),
            SecurityMode::Lax,
        ),
    ] {
        let config = tau_proto::json_to_cbor(&value)
            .deserialized::<ExtConfig>()
            .expect("valid mode");
        assert_eq!(config.security_mode, expected);
    }
    let error = tau_proto::json_to_cbor(&serde_json::json!({"security_mode": "permissive"}))
        .deserialized::<ExtConfig>()
        .expect_err("invalid mode");
    assert!(format!("{error:?}").contains("unknown variant"));
}

/// The agent-id prefix is an opt-in compatibility presentation setting:
/// omission and explicit false are equivalent, true is accepted, and
/// non-booleans fail.
#[test]
fn agent_id_prefix_config_defaults_false_and_requires_a_boolean() {
    for (value, expected) in [
        (serde_json::json!({}), false),
        (serde_json::json!({"prefix_agent_id": false}), false),
        (serde_json::json!({"prefix_agent_id": true}), true),
    ] {
        let config = tau_proto::json_to_cbor(&value)
            .deserialized::<ExtConfig>()
            .expect("valid prefix config");
        assert_eq!(config.prefix_agent_id, expected);
    }
    let error = tau_proto::json_to_cbor(&serde_json::json!({"prefix_agent_id": "true"}))
        .deserialized::<ExtConfig>()
        .map_err(|error| format!("{error:?}"))
        .expect_err("string boolean must fail");
    assert!(error.contains("bool"), "{error}");
}

/// Configured channel policy silently rejects other channels and DMs without
/// even granting them a Slack reply side effect.
#[test]
fn configured_channel_rejects_other_conversations() {
    let (ext, rx, client) = extension();
    register_agent(&ext, "agent-a");
    ext.process_slack_message(slack_message("C999", None, "<@UBOT123> hello"));
    ext.process_slack_message(slack_message("D123", Some("im"), "hello"));
    assert!(rx.try_recv().is_err());
    assert!(client.sent.lock().expect("lock").is_empty());
}

/// A dynamic D id remains bound to its exact allowlisted U/W user for prompts,
/// validation replies, and all bridge-control commands.
#[test]
fn dynamic_dm_wrong_user_has_no_ingress_control_or_local_effects() {
    let (ext, rx, client) = extension();
    let mut config = dm_cfg();
    config.allowed_user_ids.insert("U999".to_owned());
    apply_test_config(&ext, config);
    register_agent(&ext, "agent-a");
    ext.process_slack_message(slack_message("D123", Some("im"), "start"));
    let baseline_posts = client.sent_pairs().len();

    for text in [
        "",
        "plain",
        "start",
        "agents",
        "select agent-a",
        "to agent-a hello",
        "/unknown",
        "message that is deliberately too long for a tiny configured limit",
    ] {
        let mut wrong_user = slack_message("D123", Some("im"), text);
        wrong_user.user_id = "U999".to_owned();
        ext.process_slack_message(wrong_user);
    }
    assert!(rx.try_recv().is_err());
    assert_eq!(client.sent_pairs().len(), baseline_posts);
    assert!(
        ext.state
            .lock()
            .expect("state")
            .selected_agent_by_route
            .is_empty()
    );
}

/// Dynamic DM capacity never evicts/replaces existing exact links, while a
/// proactive-only static DM remains compatible with dynamic receive linkage.
#[test]
fn dynamic_dm_capacity_and_proactive_only_collision_rules() {
    let (ext, _rx, client) = extension();
    let mut config = dm_cfg();
    let proactive = ConversationPolicy {
        alias: "proactive-dm".to_owned(),
        conversation_id: "D999".to_owned(),
        kind: ConversationPolicyKind::Dm,
        receive: None,
        description: None,
        thread_ts: None,
    };
    config
        .conversations
        .insert(proactive.alias.clone(), proactive.clone());
    config.proactive_aliases.insert(proactive.alias.clone());
    apply_test_config(&ext, config);
    register_agent(&ext, "agent-a");
    ext.process_slack_message(slack_message("D999", Some("im"), "start"));
    assert!(
        ext.state
            .lock()
            .expect("state")
            .linked_dms
            .contains_key("D999")
    );

    {
        let mut state = ext.state.lock().expect("state");
        state.linked_dms.clear();
        for index in 0..DYNAMIC_DM_LIMIT {
            state.linked_dms.insert(
                format!("D{index:03}"),
                LinkedConversation {
                    user_id: "U123".to_owned(),
                },
            );
        }
    }
    ext.process_slack_message(slack_message("DNEW", Some("im"), "start"));
    let state = ext.state.lock().expect("state");
    assert_eq!(state.linked_dms.len(), DYNAMIC_DM_LIMIT);
    assert!(!state.linked_dms.contains_key("DNEW"));
    assert!(state.linked_dms.contains_key("D000"));
    drop(state);
    assert!(
        client
            .sent_pairs()
            .last()
            .is_some_and(|(_, text)| text.contains("capacity"))
    );
}

/// Any static receive-enabled DM scope blocks dynamic broadening, including a
/// fixed-thread-only receive route.
#[test]
fn static_fixed_dm_receive_blocks_dynamic_parent_link() {
    let (ext, _rx, client) = extension();
    let mut config = fixed_thread_cfg(ConversationPolicyKind::Dm, "D777");
    config.dynamic_direct_messages = Some(DynamicDirectMessages {
        receive: ReceiveMode::AllMessages,
    });
    apply_test_config(&ext, config);
    register_agent(&ext, "agent-a");
    ext.process_slack_message(slack_message("D777", Some("im"), "start"));
    assert!(
        !ext.state
            .lock()
            .expect("state")
            .linked_dms
            .contains_key("D777")
    );
    assert!(
        client
            .sent_pairs()
            .last()
            .is_some_and(|(_, text)| text.contains("cannot broaden"))
    );
}

/// With exactly one registered agent, plain Slack text becomes an unprefixed
/// typed payload with structured external provenance.
#[test]
fn one_registered_agent_receives_plain_text() {
    let (ext, rx, _client) = extension();
    register_agent(&ext, "agent-a");
    ext.process_slack_message(slack_message("C123", None, "<@UBOT123> hello"));
    assert_eq!(recv_prompt(&rx), "hello");
}

/// Multiple registered agents without a selection produce Slack guidance rather
/// than guessing which Tau agent should receive prompt text.
#[test]
fn multiple_agents_without_selection_get_guidance() {
    let (ext, rx, client) = extension();
    register_agent(&ext, "agent-a");
    register_agent(&ext, "agent-b");
    ext.process_slack_message(slack_message("C123", None, "<@UBOT123> hello"));
    assert!(rx.try_recv().is_err());
    assert!(
        client.sent.lock().expect("lock")[0]
            .text
            .contains("Multiple Tau agents")
    );
}

/// Malformed command-shaped text is handled as command feedback and must not
/// fall through as a routed prompt.
#[test]
fn malformed_commands_do_not_become_prompts() {
    let (ext, rx, client) = extension();
    register_agent(&ext, "agent-a");
    let mut select = slack_message("C123", None, "<@UBOT123> select");
    select.thread_ts = Some("44.0".to_owned());
    let mut unknown = slack_message("C123", None, "<@UBOT123> /unknown");
    unknown.thread_ts = Some("44.0".to_owned());
    ext.process_slack_message(select);
    ext.process_slack_message(unknown);
    assert!(rx.try_recv().is_err());
    assert_eq!(client.sent.lock().expect("lock").len(), 2);
    assert_eq!(
        client.sent_thread_ids(),
        vec![Some("44.0".to_owned()), Some("44.0".to_owned())]
    );
}

/// Socket Mode envelopes with ids are acked before routing, so Slack retries
/// are avoided while prompt submission still happens through the extension
/// event.
#[test]
fn valid_envelopes_are_acked_and_routed() {
    let (ext, rx, _client) = extension();
    register_agent(&ext, "agent-a");
    let text = serde_json::json!({
        "type": "events_api",
        "envelope_id": "env-1",
        "payload": {
            "type": "event_callback",
                    "context_team_id": "T123",
            "event_id": "Ev1",
            "event": {
                "type": "app_mention",
                "channel": "C123",
                "channel_type": "channel",
                "user": "U123",
                "text": "<@UBOT123> hello",
                "ts": "1.0",
                "thread_ts": "0.5"
            }
        }
    })
    .to_string();
    let action = handle_socket_text(&ext, &text);
    assert_eq!(action.ack_envelope_id.as_deref(), Some("env-1"));
    let Some(DecodedSlackEvent::Message(message)) = action.event else {
        panic!("decoded message");
    };
    assert_eq!(message.thread_ts.as_deref(), Some("0.5"));
    ext.process_slack_message(message);
    assert_eq!(recv_prompt(&rx), "hello");
}

/// Malformed thread metadata is rejected rather than downgraded to a top-level
/// destination that could misroute a reply.
#[test]
fn malformed_message_thread_metadata_is_rejected() {
    let value = serde_json::json!({
        "payload": {
            "type": "event_callback",
                    "context_team_id": "T123",
            "event_id": "Ev-thread-bad",
            "event": {
                "type": "app_mention",
                "channel": "C123",
                "user": "U123",
                "text": "<@UBOT123> hello",
                "ts": "1.0",
                "thread_ts": "not-a-ts"
            }
        }
    });
    assert!(decode_socket_event(&value).is_none());
}

/// Missing or malformed create identity, sender, conversation, timestamp, and
/// message-family metadata fail before ingress or local Slack effects.
#[test]
fn malformed_create_metadata_is_silent_and_fail_closed() {
    let (ext, rx, client) = extension();
    register_agent(&ext, "agent-a");
    let mut cases = Vec::new();
    let mut missing_ts = slack_message("C123", Some("channel"), "<@UBOT123> x");
    missing_ts.ts = None;
    cases.push(missing_ts);
    let mut bad_ts = slack_message("C123", Some("channel"), "<@UBOT123> x");
    bad_ts.ts = Some("bad".to_owned());
    cases.push(bad_ts);
    cases.push(slack_message(
        "not-a-conversation",
        Some("channel"),
        "<@UBOT123> x",
    ));
    let mut bad_user = slack_message("C123", Some("channel"), "<@UBOT123> x");
    bad_user.user_id = "guest".to_owned();
    cases.push(bad_user);
    let mut unsupported = slack_message("C123", Some("mpim"), "<@UBOT123> x");
    unsupported.event_type = "message".to_owned();
    cases.push(unsupported);
    for message in cases {
        ext.process_slack_message(message);
    }
    assert!(rx.try_recv().is_err());
    assert!(client.sent_pairs().is_empty());
}

/// Socket Mode reaction envelopes retain stable message identity metadata and
/// are acked independently of later authorization checks.
#[test]
fn reaction_envelopes_are_acked_and_decoded() {
    let (ext, _rx, _client) = extension();
    let text = serde_json::json!({
        "type": "events_api",
        "envelope_id": "env-reaction",
        "payload": {
            "type": "event_callback",
                    "context_team_id": "T123",
            "event_id": "Er1",
            "event": {
                "type": "reaction_removed",
                "user": "U123",
                "reaction": "eyes",
                "item": {
                    "type": "message",
                    "channel": "C123",
                    "ts": "12.34",
                    "thread_ts": "10.00"
                }
            }
        }
    })
    .to_string();
    let action = handle_socket_text(&ext, &text);
    assert_eq!(action.ack_envelope_id.as_deref(), Some("env-reaction"));
    let Some(DecodedSlackEvent::Reaction(reaction)) = action.event else {
        panic!("decoded reaction");
    };
    assert_eq!(reaction.event_type.as_str(), "reaction_removed");
    assert_eq!(reaction.channel_id, "C123");
    assert_eq!(reaction.message_ts, "12.34");
    assert_eq!(reaction.thread_ts.as_deref(), Some("10.00"));
}

/// Slack event ids are bounded and free of controls before they can be retained
/// in the process-local occurrence cache.
#[test]
fn edit_and_reaction_event_ids_are_bounded_and_visible() {
    let reaction = || {
        serde_json::json!({
            "payload": {
                "type": "event_callback",
                "event_id": "ER1",
                "event": {
                    "type": "reaction_added",
                    "user": "U123",
                    "reaction": "eyes",
                    "item": {"type": "message", "channel": "C123", "ts": "1.0"}
                }
            }
        })
    };
    let edit = || {
        serde_json::json!({
            "payload": {
                "type": "event_callback",
                "event_id": "EE1",
                "event": {
                    "type": "message",
                    "subtype": "message_changed",
                    "channel": "C123",
                    "message": {
                        "user": "U123",
                        "text": "edited",
                        "ts": "1.0",
                        "edited": {"user": "U123", "ts": "2.0"}
                    },
                    "previous_message": {
                        "user": "U123",
                        "text": "original",
                        "ts": "1.0"
                    }
                }
            }
        })
    };
    for invalid in [
        String::new(),
        "x".repeat(MAX_EVENT_ID_BYTES + 1),
        "line\nbreak".to_owned(),
    ] {
        for mut envelope in [reaction(), edit()] {
            envelope["payload"]["event_id"] = serde_json::Value::String(invalid.clone());
            assert!(decode_socket_event(&envelope).is_none());
        }
    }
}

/// Slack `message_changed` decoding requires consistent original/editor/thread
/// metadata and retains explicit native revision identity.
#[test]
fn message_changed_envelopes_decode_as_edits_and_reject_conflicts() {
    let envelope = |message_ts: &str, previous_ts: &str, thread: Option<&str>| {
        serde_json::json!({
            "payload": {
                "type": "event_callback",
                    "context_team_id": "T123",
                "event_id": "EE-DECODE",
                "event": {
                    "type": "message",
                    "subtype": "message_changed",
                    "channel": "C123",
                    "message": {
                        "user": "U123",
                        "text": "edited",
                        "ts": message_ts,
                        "thread_ts": thread,
                        "edited": {"user": "U123", "ts": "2.0"}
                    },
                    "previous_message": {
                        "user": "U123",
                        "text": "original",
                        "ts": previous_ts,
                        "thread_ts": thread
                    }
                }
            }
        })
    };
    let Some(DecodedSlackEvent::Edit(edit)) =
        decode_socket_event(&envelope("1.0", "1.0", Some("9.0")))
    else {
        panic!("expected typed edit");
    };
    assert_eq!(edit.message_ts, "1.0");
    assert_eq!(edit.thread_ts.as_deref(), Some("9.0"));
    assert_eq!(edit.revision_ts, "2.0");
    assert!(decode_socket_event(&envelope("1.0", "3.0", Some("9.0"))).is_none());

    let mut conflicting = envelope("1.0", "1.0", None);
    conflicting["payload"]["event"]["previous_message"]["user"] =
        serde_json::Value::String("U999".to_owned());
    assert!(decode_socket_event(&conflicting).is_none());
    let mut conflicting = envelope("1.0", "1.0", None);
    conflicting["payload"]["event"]["message"]["user"] =
        serde_json::Value::String("U999".to_owned());
    assert!(decode_socket_event(&conflicting).is_none());
    let mut conflicting = envelope("1.0", "1.0", Some("9.0"));
    conflicting["payload"]["event"]["previous_message"]["thread_ts"] =
        serde_json::Value::String("8.0".to_owned());
    assert!(decode_socket_event(&conflicting).is_none());
    for malformed in [
        envelope("bad", "bad", None),
        envelope("1.0", "1.0", Some("bad")),
    ] {
        assert!(decode_socket_event(&malformed).is_none());
    }
    let mut malformed_revision = envelope("1.0", "1.0", None);
    malformed_revision["payload"]["event"]["message"]["edited"]["ts"] =
        serde_json::Value::String("bad".to_owned());
    assert!(decode_socket_event(&malformed_revision).is_none());
}

/// Slack event types are conversation-specific: configured channels accept
/// mentions, while DM mode accepts only direct-message `message` events.
#[test]
fn mention_and_message_event_types_do_not_cross_conversation_modes() {
    let (ext, rx, client) = extension();
    register_agent(&ext, "agent-a");
    let mut channel_message = slack_message("C123", None, "<@UBOT123> channel");
    channel_message.event_type = "message".to_owned();
    ext.process_slack_message(channel_message);
    let mut dm_mention = slack_message("D123", Some("im"), "<@UBOT123> dm");
    dm_mention.event_type = "app_mention".to_owned();
    ext.process_slack_message(dm_mention);
    assert!(rx.try_recv().is_err());
    assert!(client.sent.lock().expect("lock").is_empty());
}

/// Unmentioned all-messages chatter must remain prompt content even when its
/// first word resembles a bridge command, and must cause no Slack reply.
#[test]
fn all_messages_unmentioned_chatter_cannot_invoke_bridge_commands() {
    let (ext, rx, client) = extension();
    ext.state
        .lock()
        .expect("lock")
        .config
        .as_mut()
        .expect("config")
        .conversations
        .get_mut("team")
        .expect("team")
        .receive = Some(ReceiveMode::AllMessages);
    register_agent(&ext, "agent-a");
    for text in ["start", "to clarify this", "/select agent-b"] {
        let mut message = slack_message("C123", None, text);
        message.event_type = "message".to_owned();
        ext.process_slack_message(message);
        assert_eq!(recv_prompt(&rx), text);
    }
    assert!(client.sent.lock().expect("lock").is_empty());
}

/// Every removed global/asymmetric key produces actionable migration guidance.
#[test]
fn removed_conversation_keys_return_migration_error() {
    let mut secrets = BTreeMap::new();
    secrets.insert("app".to_owned(), tau_proto::SecretValue::new("xapp-test"));
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("xoxb-test"));
    for legacy in [
        serde_json::json!({"channel_ids":["C123"]}),
        serde_json::json!({"listening_scope":"mentions_only"}),
        serde_json::json!({"send_destinations":[]}),
        serde_json::json!({"channel_ids":null}),
        serde_json::json!({"listening_scope":null}),
        serde_json::json!({"send_destinations":null}),
    ] {
        let mut value = serde_json::json!({
            "app_token_secret":"app",
            "bot_token_secret":"bot",
            "allowed_user_ids":["U123"],
        });
        value
            .as_object_mut()
            .expect("object")
            .extend(legacy.as_object().expect("legacy object").clone());
        let error = tau_proto::json_to_cbor(&value)
            .deserialized::<ExtConfig>()
            .expect("removed key recognized")
            .validate(&secrets)
            .err()
            .expect("removed key rejected");
        assert!(error.contains("were removed"));
        assert!(error.contains("conversations[]"));
        assert!(error.contains("proactive_send"));
    }
}

/// Unified conversation validation rejects ambiguous receive authority while
/// preserving deliberate send-only and fixed-thread combinations.
#[test]
fn conversation_policy_receive_collision_matrix() {
    let mut secrets = BTreeMap::new();
    secrets.insert("app".to_owned(), tau_proto::SecretValue::new("xapp-test"));
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("xoxb-test"));
    let validate = |conversations: serde_json::Value| {
        tau_proto::json_to_cbor(&serde_json::json!({
            "app_token_secret": "app",
            "bot_token_secret": "bot",
            "allowed_user_ids": ["U123", "W456"],
            "conversations": conversations,
        }))
        .deserialized::<ExtConfig>()
        .map_err(|error| format!("{error:?}"))
        .and_then(|config| config.validate(&secrets).map(|_| ()))
    };

    assert!(validate(serde_json::json!([
        {"alias":"parent","conversation_id":"C123","kind":"channel","receive":"mentions_only"},
        {"alias":"thread","conversation_id":"C123","kind":"channel","thread_ts":"1.0","proactive_send":true},
        {"alias":"dm","conversation_id":"D123","kind":"dm","receive":"all_messages"},
        {"alias":"mpim","conversation_id":"G123","kind":"mpim","receive":"all_messages"}
    ])).is_ok());
    assert!(validate(serde_json::json!([
        {"alias":"one","conversation_id":"C123","kind":"channel","thread_ts":"1.0","receive":"all_messages"},
        {"alias":"two","conversation_id":"C123","kind":"channel","thread_ts":"2.0","receive":"mentions_only"}
    ])).is_ok());
    assert!(validate(serde_json::json!([
        {"alias":"receive","conversation_id":"C123","kind":"channel","receive":"all_messages","description":"Receive-only route"}
    ])).is_ok());

    for invalid in [
        serde_json::json!([
            {"alias":"parent","conversation_id":"C123","kind":"channel","receive":"all_messages"},
            {"alias":"child","conversation_id":"C123","kind":"channel","thread_ts":"1.0","receive":"mentions_only"}
        ]),
        serde_json::json!([{"alias":"dm","conversation_id":"D123","kind":"dm","receive":"mentions_only"}]),
        serde_json::json!([{"alias":"direct-message","conversation_id":"D123","kind":"dm","proactive_send":true}]),
        serde_json::json!([{"alias":"inert","conversation_id":"C123","kind":"channel"}]),
        serde_json::json!([{"alias":"padded","conversation_id":" C123","kind":"channel","proactive_send":true}]),
        serde_json::json!([
            {"alias":"channel","conversation_id":"G123","kind":"channel","proactive_send":true},
            {"alias":"mpim","conversation_id":"G123","kind":"mpim","receive":"all_messages"}
        ]),
    ] {
        assert!(validate(invalid).is_err());
    }
}

/// Operator-facing deserialization covers combined permissions, fixed MPIM/DM
/// threads, dynamic-DM modes, Enterprise users, and exact unpadded ID bounds.
#[test]
fn operator_conversation_config_parsing_matrix() {
    let mut secrets = BTreeMap::new();
    secrets.insert("app".to_owned(), tau_proto::SecretValue::new("xapp-test"));
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("xoxb-test"));
    let validate = |value: serde_json::Value| {
        tau_proto::json_to_cbor(&value)
            .deserialized::<ExtConfig>()
            .map_err(|error| format!("{error:?}"))
            .and_then(|config| config.validate(&secrets).map(|_| ()))
    };
    let base = |users: serde_json::Value,
                conversations: serde_json::Value,
                dynamic: Option<serde_json::Value>| {
        let mut value = serde_json::json!({
            "app_token_secret":"app",
            "bot_token_secret":"bot",
            "allowed_user_ids":users,
            "conversations":conversations,
        });
        if let Some(dynamic) = dynamic {
            value["dynamic_direct_messages"] = dynamic;
        }
        value
    };
    assert!(validate(base(
        serde_json::json!(["W123"]),
        serde_json::json!([
            {"alias":"combined","conversation_id":"C123","kind":"channel","receive":"all_messages","proactive_send":true},
            {"alias":"mpim-thread","conversation_id":"G123","kind":"mpim","thread_ts":"1.0","receive":"mentions_only"},
            {"alias":"dm-thread","conversation_id":"D123","kind":"dm","thread_ts":"2.0","receive":"all_messages"}
        ]),
        Some(serde_json::json!({"receive":"all_messages"})),
    ))
    .is_ok());
    assert!(
        validate(base(
            serde_json::json!(["U123"]),
            serde_json::json!([]),
            Some(serde_json::json!({"receive":"all_messages"})),
        ))
        .is_ok()
    );
    for invalid in [
        base(
            serde_json::json!([" U123"]),
            serde_json::json!([{"alias":"x","conversation_id":"C123","kind":"channel","proactive_send":true}]),
            None,
        ),
        base(
            serde_json::json!([format!("U{}", "1".repeat(64))]),
            serde_json::json!([{"alias":"x","conversation_id":"C123","kind":"channel","proactive_send":true}]),
            None,
        ),
        base(
            serde_json::json!(["U123"]),
            serde_json::json!([{"alias":"x","conversation_id":" C123","kind":"channel","proactive_send":true}]),
            None,
        ),
        base(
            serde_json::json!(["U123"]),
            serde_json::json!([]),
            Some(serde_json::json!({"receive":"mentions_only"})),
        ),
        base(
            serde_json::json!(["U123"]),
            serde_json::json!([]),
            Some(serde_json::json!({"receive":"all_messages","extra":true})),
        ),
    ] {
        assert!(validate(invalid).is_err());
    }
}

/// Slack app mentions are message-like and need not carry `channel_type`;
/// explicit static policy still supplies channel versus MPIM authority.
#[test]
fn app_mention_without_channel_type_uses_explicit_static_kind() {
    let (ext, rx, _client) = extension();
    register_agent(&ext, "agent-a");
    let mut mention = slack_message("C123", Some("channel"), "<@UBOT123> hello");
    mention.channel_type = None;
    ext.process_slack_message(mention);
    assert_eq!(recv_prompt(&rx), "hello");
}

/// Every bridge-local response to a fixed-thread root uses the configured root
/// even though Slack omits `thread_ts` on the root create.
#[test]
fn fixed_thread_root_local_replies_never_escape_to_parent() {
    for text in [
        "<@UBOT123>",
        "<@UBOT123> agents",
        "<@UBOT123> /unknown",
        "<@UBOT123> message that is deliberately too long",
    ] {
        let (ext, rx, client) = extension();
        let mut config = fixed_thread_cfg(ConversationPolicyKind::Channel, "C777");
        if text.contains("deliberately too long") {
            config.max_message_bytes = 20;
        }
        apply_test_config(&ext, config);
        register_agent(&ext, "agent-a");
        let mut root = slack_message("C777", Some("channel"), text);
        root.ts = Some("7.0".to_owned());
        root.thread_ts = None;
        ext.process_slack_message(root);
        assert!(rx.try_recv().is_err());
        assert_eq!(client.sent_thread_ids(), vec![Some("7.0".to_owned())]);
    }

    let (ext, rx, client) = extension();
    apply_test_config(
        &ext,
        fixed_thread_cfg(ConversationPolicyKind::Channel, "C777"),
    );
    let mut root = slack_message("C777", Some("channel"), "<@UBOT123> hello");
    root.ts = Some("7.0".to_owned());
    ext.process_slack_message(root);
    assert!(rx.try_recv().is_err());
    assert_eq!(client.sent_thread_ids(), vec![Some("7.0".to_owned())]);
}

/// Slack's two wrappers for one leading-mention command have identical command
/// semantics and exactly one bridge-local side effect in either arrival order.
#[test]
fn dual_wrapper_commands_have_one_local_effect() {
    for text in [
        "<@UBOT123> start",
        "<@UBOT123> agents",
        "<@UBOT123> select agent-a",
        "<@UBOT123> to",
        "<@UBOT123> /unknown",
        "<@UBOT123>",
        "<@UBOT123> message that is deliberately too long",
    ] {
        for message_first in [true, false] {
            let (ext, rx, client) = extension();
            {
                let mut state = ext.state.lock().expect("state");
                let config = state.config.as_mut().expect("config");
                config.conversations.get_mut("team").expect("team").receive =
                    Some(ReceiveMode::AllMessages);
                if text.contains("deliberately too long") {
                    config.max_message_bytes = 20;
                }
            }
            register_agent(&ext, "agent-a");
            let (message, mention) = dual_slack_messages(text);
            if message_first {
                ext.process_slack_message(message);
                ext.process_slack_message(mention);
            } else {
                ext.process_slack_message(mention);
                ext.process_slack_message(message);
            }
            assert!(rx.try_recv().is_err(), "{text}");
            assert_eq!(client.sent_pairs().len(), 1, "{text}");
        }
    }
}

/// Dual wrappers produce only one local routing-error response when no unique
/// registered target exists.
#[test]
fn dual_wrapper_plain_routing_errors_have_one_local_effect() {
    for registered in [0, 2] {
        for message_first in [true, false] {
            let (ext, rx, client) = extension();
            ext.state
                .lock()
                .expect("state")
                .config
                .as_mut()
                .expect("config")
                .conversations
                .get_mut("team")
                .expect("team")
                .receive = Some(ReceiveMode::AllMessages);
            for index in 0..registered {
                register_agent(&ext, &format!("agent-{index}"));
            }
            let (message, mention) = dual_slack_messages("<@UBOT123> hello");
            if message_first {
                ext.process_slack_message(message);
                ext.process_slack_message(mention);
            } else {
                ext.process_slack_message(mention);
                ext.process_slack_message(message);
            }
            assert!(rx.try_recv().is_err());
            assert_eq!(client.sent_pairs().len(), 1);
        }
    }
}

/// Leading whitespace is removed before testing the exact authenticated mention
/// token, so both wrappers retain one bridge-command side effect.
#[test]
fn padded_leading_mention_retains_command_authority() {
    for message_first in [true, false] {
        let (ext, rx, client) = extension();
        ext.state
            .lock()
            .expect("state")
            .config
            .as_mut()
            .expect("config")
            .conversations
            .get_mut("team")
            .expect("team")
            .receive = Some(ReceiveMode::AllMessages);
        register_agent(&ext, "agent-a");
        let (message, mention) = dual_slack_messages(" <@UBOT123> agents");
        if message_first {
            ext.process_slack_message(message);
            ext.process_slack_message(mention);
        } else {
            ext.process_slack_message(mention);
            ext.process_slack_message(message);
        }
        assert!(rx.try_recv().is_err());
        assert_eq!(client.sent_pairs().len(), 1);
    }
}

/// Slack redelivery replays a pending report, then canonical confirmation
/// retires it into ordinary duplicate suppression.
#[test]
fn duplicate_slack_event_ids_are_dropped_locally() {
    let (ext, rx, _client) = extension();
    register_agent(&ext, "agent-a");
    let msg = slack_message("C123", None, "<@UBOT123> hello");
    ext.process_slack_message(msg.clone());
    ext.process_slack_message(msg);
    let first = recv_message_report(&rx, "first delivered");
    let second = recv_message_report(&rx, "replayed delivered");
    assert_eq!(first, second);
    acknowledge_message_report(&ext, &first);
    let duplicate = slack_message("C123", None, "<@UBOT123> hello");
    ext.process_slack_message(duplicate);
    assert!(rx.try_recv().is_err());
}

/// Pending duplicate replay preserves the original target even if route
/// selection changes before Slack redelivery.
#[test]
fn pending_duplicate_replays_original_target_after_selection_change() {
    let (ext, rx, client) = extension();
    register_agent(&ext, "agent-a");
    register_agent(&ext, "agent-b");
    ext.state
        .lock()
        .expect("state")
        .selected_agent_by_route
        .insert(
            SelectionRouteKey::StaticAlias("team".to_owned()),
            agent_id("agent-a"),
        );
    let message = slack_message("C123", Some("channel"), "<@UBOT123> hello");
    ext.process_slack_message(message.clone());
    let original = recv_message_report(&rx, "original delivered");
    let identity_count = *client.identity_count.lock().expect("identity count");
    ext.state
        .lock()
        .expect("state")
        .selected_agent_by_route
        .insert(
            SelectionRouteKey::StaticAlias("team".to_owned()),
            agent_id("agent-b"),
        );
    ext.process_slack_message(message);
    let replay = recv_message_report(&rx, "replayed delivered");
    assert_eq!(replay, original);
    let Event::MessageDeliveredReported(report) = replay else {
        panic!("expected delivered replay");
    };
    assert_eq!(report.agent_id.as_str(), "agent-a");
    assert_eq!(
        *client.identity_count.lock().expect("identity count"),
        identity_count,
        "pending replay must not repeat identity lookup"
    );
}

/// Pending message replay ignores changed sender/wrapper/route metadata,
/// performs no second identity lookup, and keeps one original outstanding
/// permit.
#[test]
fn pending_message_duplicate_replays_before_mutable_metadata_policy() {
    let (ext, rx, client) = extension();
    register_agent(&ext, "agent-a");
    ext.state.lock().expect("state").session_active = true;
    let queue = AdmissionQueue::<AdmissionWork>::new();
    let original_context = admission_context(&ext);
    original_context
        .permit
        .borrow_mut()
        .replace(queue.retain_test_permit());
    let message = slack_message("C123", Some("channel"), "<@UBOT123> hello");
    ext.process_slack_message_admitted(message.clone(), Some(&original_context));
    let original = recv_message_report(&rx, "original delivered");
    let identity_count = *client.identity_count.lock().expect("identity count");

    let duplicate_context = admission_context(&ext);
    duplicate_context
        .permit
        .borrow_mut()
        .replace(queue.retain_test_permit());
    let mut changed = message;
    changed.user_id = "invalid changed sender".to_owned();
    changed.event_type = "unsupported_wrapper".to_owned();
    changed.channel_type = Some("im".to_owned());
    changed.thread_ts = Some("invalid changed route".to_owned());
    ext.process_slack_message_admitted(changed, Some(&duplicate_context));
    drop(duplicate_context.permit.borrow_mut().take());
    let replay = recv_message_report(&rx, "replayed delivered");
    assert_eq!(replay, original);
    assert_eq!(
        *client.identity_count.lock().expect("identity count"),
        identity_count
    );
    assert_eq!(ext.state.lock().expect("state").pending_ingress.len(), 1);
    let held = (1..admission::CAPACITY)
        .map(|_| queue.reserve().expect("remaining admission slot"))
        .collect::<Vec<_>>();
    assert!(matches!(queue.reserve(), Err(ReserveError::Full)));
    drop(held);
}

/// Rejected unseen wrappers do not consume the stable key before the former
/// post-policy occurrence-admission point.
#[test]
fn rejected_unseen_wrapper_does_not_suppress_later_valid_occurrence() {
    for mutation in ["bot", "subtype", "unsupported"] {
        let (ext, rx, client) = extension();
        register_agent(&ext, "agent-a");
        let valid = slack_message("C123", Some("channel"), "<@UBOT123> hello");
        let mut rejected = valid.clone();
        match mutation {
            "bot" => rejected.bot_id = Some("B123".to_owned()),
            "subtype" => rejected.subtype = Some("channel_join".to_owned()),
            "unsupported" => rejected.event_type = "unsupported_wrapper".to_owned(),
            _ => unreachable!("closed mutation cases"),
        }
        ext.process_slack_message(rejected);
        assert!(rx.try_recv().is_err());
        ext.process_slack_message(valid);
        let Event::MessageDeliveredReported(_) = recv_message_report(&rx, "valid delivered") else {
            panic!("valid wrapper must remain admissible after {mutation}");
        };
        assert_eq!(*client.identity_count.lock().expect("identity count"), 1);
    }
}

/// If canonical confirmation wins after duplicate classification but before
/// replay acquires the gate, retirement suppresses the now-obsolete replay.
#[test]
fn canonical_echo_between_duplicate_classification_and_replay_suppresses_output() {
    let (ext, rx, _client) = extension();
    register_agent(&ext, "agent-a");
    let message = slack_message("C123", Some("channel"), "<@UBOT123> hello");
    ext.process_slack_message(message.clone());
    let report = recv_message_report(&rx, "original delivered");
    let canonical = report
        .clone()
        .into_stamped_canonical_message_fact(
            tau_proto::MessagePublisherId::parse("std-slack").expect("publisher"),
        )
        .expect("canonical delivered fact");
    let (reached_tx, reached_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    *ext.test_hooks
        .ingress_replay_classified_boundary
        .lock()
        .expect("hook") = Some(BlockingTestHook {
        reached: reached_tx,
        release: release_rx,
    });
    let ext = Arc::new(ext);
    let replaying = Arc::clone(&ext);
    let replay = std::thread::spawn(move || replaying.process_slack_message(message));
    reached_rx.recv().expect("pending replay classified");
    ext.apply_live_event(&canonical);
    assert!(ext.state.lock().expect("state").pending_ingress.is_empty());
    release_tx.send(()).expect("release replay");
    replay.join().expect("replay worker");
    assert!(rx.try_recv().is_err(), "retired report must not replay");
}

/// A repeated deletion replays its retained report even though immediate
/// fail-closed revocation removed the native owner before canonical
/// confirmation.
#[test]
fn pending_delete_replays_after_immediate_authority_revocation() {
    let (ext, rx, _client) = extension();
    register_agent(&ext, "agent-a");
    let message = slack_message("C123", Some("channel"), "<@UBOT123> hello");
    let message_ts = message.ts.clone().expect("message timestamp");
    ext.process_slack_message(message);
    let delivered = recv_message_report(&rx, "delivered");
    acknowledge_message_report(&ext, &delivered);
    let delete = || SlackDelete {
        event_id: Some("delete-replay".to_owned()),
        channel_id: "C123".to_owned(),
        message_ts: message_ts.clone(),
        thread_ts: None,
    };
    ext.process_slack_delete(delete());
    let first = recv_message_report(&rx, "first delete");
    assert!(
        !ext.state
            .lock()
            .expect("state")
            .incoming_messages
            .contains_key(&PostedMessageKey::new("C123", &message_ts))
    );
    ext.process_slack_delete(delete());
    let replay = recv_message_report(&rx, "replayed delete");
    assert_eq!(first, replay);
    acknowledge_message_report(&ext, &first);
    ext.process_slack_delete(delete());
    assert!(rx.try_recv().is_err());
}

/// The extension-local duplicate set remains bounded and evicts oldest ids.
#[test]
fn received_occurrence_cache_is_bounded() {
    let mut cache = ReceivedOccurrenceCache::default();
    for index in 0..=RECEIVED_OCCURRENCE_LIMIT {
        assert!(cache.insert_new(format!("event-{index}")));
    }
    assert_eq!(cache.seen.len(), RECEIVED_OCCURRENCE_LIMIT);
    assert!(!cache.seen.contains("event-0"));
    assert!(!cache.insert_new(format!("event-{RECEIVED_OCCURRENCE_LIMIT}")));
}

/// Bot/self messages and message subtypes are ignored to avoid routing Slack
/// bot echoes, edits, joins, deletes, or other non-user text events.
#[test]
fn bot_self_and_subtype_messages_are_ignored() {
    let (ext, rx, _client) = extension();
    register_agent(&ext, "agent-a");
    let mut self_msg = slack_message("C123", None, "<@UBOT123> echo");
    self_msg.user_id = "UBOT123".to_owned();
    ext.process_slack_message(self_msg);
    let mut bot_msg = slack_message("C123", None, "<@UBOT123> bot");
    bot_msg.bot_id = Some("B123".to_owned());
    ext.process_slack_message(bot_msg);
    let mut edit = slack_message("C123", None, "<@UBOT123> edit");
    edit.subtype = Some("message_changed".to_owned());
    ext.process_slack_message(edit);
    assert!(rx.try_recv().is_err());
}

/// Socket Mode URL validation rejects remote plaintext while preserving
/// loopback-only test transport.
#[test]
fn socket_url_transport_validation_is_fail_closed() {
    assert!(validate_socket_url("ws://example.com/socket?ticket=secret").is_err());
    assert!(validate_socket_url("ws://127.0.0.1:9000/socket?ticket=secret").is_ok());
}

/// A users.info outage rejects every occurrence, reports only once per
/// consecutive failure episode, bounds/redacts the notice, and reports again
/// after a successful verification establishes recovery.
#[test]
fn identity_failure_notice_is_bounded_redacted_and_resets_after_recovery() {
    let client = Arc::new(IdentitySequenceClient {
        results: Mutex::new(
            [
                Err(SlackApiError::Transport),
                Err(SlackApiError::Transport),
                Ok(true),
                Err(SlackApiError::Transport),
            ]
            .into_iter()
            .collect(),
        ),
    });
    let (tx, rx) = mpsc::channel();
    let ext = Extension::new(client, tx);
    assert!(!ext.verified_human(&cfg(), "U123"));
    assert!(!ext.verified_human(&cfg(), "U123"));
    assert!(ext.verified_human(&cfg(), "U123"));
    assert!(!ext.verified_human(&cfg(), "U123"));

    let notices = rx
        .try_iter()
        .filter_map(|message| match message {
            HarnessInputMessage::ExtensionNoticeRequest(request) => Some(request),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(notices.len(), 2);
    for notice in notices {
        assert_eq!(notice.level, NoticeLevel::Warning);
        assert!(!notice.message.contains("xoxb-test"));
        assert!(notice.message.len() <= MAX_DIAGNOSTIC_BYTES + 3);
    }
}

/// Envelope ids remain available for the exact wire ACK while diagnostics
/// expose only ACK status and supported-event state. A failed required ACK must
/// also return before the decoded event can route, preventing unacknowledged
/// deliveries from creating duplicate ingress.
#[test]
fn socket_ack_diagnostics_are_safe_and_failure_prevents_routing() {
    let (ext, rx, _client) = extension();
    let secret_id = "env-secret-payload-token".repeat(50);
    let text = serde_json::json!({
        "type": "events_api",
        "envelope_id": secret_id,
        "payload": {"type": "unsupported"}
    })
    .to_string();
    let action = handle_socket_text(&ext, &text);
    assert_eq!(action.ack_envelope_id.as_deref(), Some(secret_id.as_str()));
    assert!(action.event.is_none());

    let trace = SharedWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .without_time()
        .with_ansi(false)
        .with_writer({
            let trace = trace.clone();
            move || trace.clone()
        })
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        log_socket_ack_sent(false);
        assert_eq!(
            finish_socket_ack(Err("wire closed".to_owned()), true),
            Err("wire closed".to_owned())
        );
    });
    let output = String::from_utf8(trace.bytes()).expect("UTF-8 trace output");
    assert!(output.contains("ack=\"sent\""));
    assert!(output.contains("has_supported_event=false"));
    assert!(output.contains("ack=\"failed\""));
    assert!(output.contains("lifecycle=\"degraded\""));
    assert!(output.contains("has_supported_event=true"));
    assert!(!output.contains(&secret_id));
    assert!(!output.contains("payload"));
    assert!(!output.contains("token"));

    let routed_text = serde_json::json!({
        "type": "events_api",
        "envelope_id": secret_id,
        "payload": {
            "type": "event_callback",
                    "context_team_id": "T123",
            "event": {
                "type": "app_mention", "channel": "C123", "user": "U123",
                "text": "<@UBOT123> must-not-route", "ts": "2.0"
            }
        }
    })
    .to_string();
    let action = handle_socket_text(&ext, &routed_text);
    let queue = AdmissionQueue::<DecodedSlackEvent>::new();
    let reservation = queue.reserve().expect("reserve before ACK");
    assert!(finish_socket_ack(Err("forced ACK write failure".to_owned()), true).is_err());
    drop(reservation);
    assert!(queue.reserve().is_ok(), "ACK failure must release capacity");
    assert!(action.event.is_some(), "fixture must decode supported work");
    assert_no_ingress(&rx);
}

/// Latency markers expose only local ordinals, durations, and bounded classes;
/// sentinel native identities, content, tokens, and destinations never appear.
#[test]
fn latency_markers_are_payload_free() {
    let (ext, _rx, _client) = extension();
    let trace = SharedWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .without_time()
        .with_ansi(false)
        .with_writer({
            let trace = trace.clone();
            move || trace.clone()
        })
        .finish();
    let sentinel_user = "U_SENTINEL_SECRET";
    let sentinel_channel = "C_SENTINEL_SECRET";
    let sentinel_text = "payload-sentinel-secret";
    tracing::subscriber::with_default(subscriber, || {
        let timing = LatencyTrace {
            connection_generation: 7,
            trace_seq: 11,
            event_class: EventClass::Create,
        };
        let (ingress_epoch, config_generation, agent_generation) = {
            let mut state = ext.state.lock().expect("state");
            state.installation_team_id = Some("T123".to_owned());
            (
                state.ingress_epoch,
                state.config_generation,
                state.agent_generation,
            )
        };
        let context = AdmissionContext {
            trace: timing,
            received_at: Instant::now(),
            ingress_epoch,
            config_generation,
            agent_generation,
            installation_team_id: "T123".to_owned(),
            queue_wait_us: 0,
            identity_us: Cell::new(0),
            outcome: Cell::new(AdmissionOutcome::RejectedPolicy),
            permit: RefCell::new(None),
        };
        assert!(
            ext.verified_human_traced(&cfg(), sentinel_user, Some(&context))
                .is_some()
        );
        ext.post_message_traced(&cfg(), sentinel_channel, sentinel_text, None, Some(timing));
    });
    let output = String::from_utf8(trace.bytes()).expect("UTF-8 trace output");
    assert!(output.contains("slack.identity.verification_finished"));
    assert!(output.contains("slack.api.post_message_finished"));
    for sentinel in [sentinel_user, sentinel_channel, sentinel_text, "xoxb-test"] {
        assert!(!output.contains(sentinel));
    }
}

/// Private latency trace class mappings retain every approved exact spelling.
#[test]
fn latency_trace_classes_have_stable_spellings() {
    let event_classes = [
        (EventClass::Malformed, "malformed"),
        (EventClass::Unsupported, "unsupported"),
        (EventClass::LocalCommand, "local_command"),
        (EventClass::Create, "create"),
        (EventClass::Reaction, "reaction"),
        (EventClass::Edit, "edit"),
        (EventClass::Delete, "delete"),
    ];
    for (class, expected) in event_classes {
        assert_eq!(class.as_str(), expected);
    }

    let admission_outcomes = [
        (AdmissionOutcome::StaleEpoch, "stale_epoch"),
        (AdmissionOutcome::RejectedIdentity, "rejected_identity"),
        (AdmissionOutcome::DuplicateIngress, "duplicate_ingress"),
        (AdmissionOutcome::RejectedRoute, "rejected_route"),
        (AdmissionOutcome::RejectedPolicy, "rejected_policy"),
        (AdmissionOutcome::LocalEffect, "local_effect"),
        (AdmissionOutcome::Submitted, "submitted"),
    ];
    for (outcome, expected) in admission_outcomes {
        assert_eq!(outcome.as_str(), expected);
    }
}

/// Builds a validated extension name used by this test module.
fn test_extension_name(value: impl AsRef<str>) -> tau_proto::ExtensionName {
    tau_proto::ExtensionName::parse(value.as_ref())
        .expect("test extension name must satisfy the identifier grammar")
}
