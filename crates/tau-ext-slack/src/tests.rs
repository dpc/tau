//! Local-fake and loopback lifecycle coverage follows
//! `DESIGN-tau-ext-slack-lifecycle-testing`; route/security matrices follow
//! `DESIGN-tau-ext-slack-conversation-policy`.

use std::io::{Read, Write};
use std::sync::Mutex;

use tau_proto::{HarnessInputMessage, HarnessOutputMessage, ToolStarted};

use super::*;

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

struct FakeClient {
    sent: Mutex<Vec<SentMessage>>,
    open_count: Mutex<usize>,
    auth_count: Mutex<usize>,
}

impl FakeClient {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            sent: Mutex::new(Vec::new()),
            open_count: Mutex::new(0),
            auth_count: Mutex::new(0),
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
}

impl SlackClient for FakeClient {
    fn open_socket(&self, _cfg: &RuntimeConfig) -> Result<String, String> {
        *self.open_count.lock().expect("lock") += 1;
        Ok("ws://127.0.0.1:9/socket-ticket".to_owned())
    }

    fn auth_test(&self, _cfg: &RuntimeConfig) -> Result<String, String> {
        *self.auth_count.lock().expect("lock") += 1;
        Ok("UBOT123".to_owned())
    }

    fn is_human_user(&self, _cfg: &RuntimeConfig, user_id: &str) -> Result<bool, String> {
        Ok(user_id != "UBOT999")
    }

    fn post_message(
        &self,
        _cfg: &RuntimeConfig,
        channel_id: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<PostedMessage, String> {
        let mut sent = self.sent.lock().expect("lock");
        sent.push(SentMessage {
            channel_id: channel_id.to_owned(),
            text: text.to_owned(),
            thread_ts: thread_ts.map(str::to_owned),
        });
        Ok(PostedMessage {
            channel_id: channel_id.to_owned(),
            ts: format!("{}.0", sent.len()),
            thread_ts: None,
        })
    }
}

struct FailingAuthClient;

impl SlackClient for FailingAuthClient {
    fn open_socket(&self, _cfg: &RuntimeConfig) -> Result<String, String> {
        Ok("ws://127.0.0.1:9/socket-ticket".to_owned())
    }

    fn auth_test(&self, _cfg: &RuntimeConfig) -> Result<String, String> {
        Err("Slack API auth.test failed: invalid_auth".to_owned())
    }

    fn is_human_user(&self, _cfg: &RuntimeConfig, _user_id: &str) -> Result<bool, String> {
        Ok(true)
    }

    fn post_message(
        &self,
        _cfg: &RuntimeConfig,
        _channel_id: &str,
        _text: &str,
        _thread_ts: Option<&str>,
    ) -> Result<PostedMessage, String> {
        Ok(PostedMessage {
            channel_id: _channel_id.to_owned(),
            ts: "1.0".to_owned(),
            thread_ts: None,
        })
    }
}

struct FailingPostClient;

impl SlackClient for FailingPostClient {
    fn open_socket(&self, _cfg: &RuntimeConfig) -> Result<String, String> {
        unreachable!("post-only client")
    }

    fn auth_test(&self, _cfg: &RuntimeConfig) -> Result<String, String> {
        unreachable!("post-only client")
    }

    fn is_human_user(&self, _cfg: &RuntimeConfig, _user_id: &str) -> Result<bool, String> {
        unreachable!("post-only client")
    }

    fn post_message(
        &self,
        _cfg: &RuntimeConfig,
        _channel_id: &str,
        _text: &str,
        _thread_ts: Option<&str>,
    ) -> Result<PostedMessage, String> {
        Err("ambiguous Slack post failure".to_owned())
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
    fn open_socket(&self, _cfg: &RuntimeConfig) -> Result<String, String> {
        unreachable!("post-only test client")
    }

    fn auth_test(&self, _cfg: &RuntimeConfig) -> Result<String, String> {
        unreachable!("post-only test client")
    }

    fn is_human_user(&self, _cfg: &RuntimeConfig, _user_id: &str) -> Result<bool, String> {
        unreachable!("post-only test client")
    }

    fn post_message(
        &self,
        _cfg: &RuntimeConfig,
        _channel_id: &str,
        _text: &str,
        _thread_ts: Option<&str>,
    ) -> Result<PostedMessage, String> {
        *self.posts.lock().expect("posts") += 1;
        Ok(PostedMessage {
            channel_id: self.returned_channel.clone(),
            ts: "1.0".to_owned(),
            thread_ts: self.returned_thread.clone(),
        })
    }
}

/// Scripted identity client used to verify fail-closed outage and recovery
/// behavior.
struct IdentitySequenceClient {
    /// Ordered users.info results consumed by calls.
    results: Mutex<VecDeque<Result<bool, String>>>,
}

impl SlackClient for IdentitySequenceClient {
    fn open_socket(&self, _cfg: &RuntimeConfig) -> Result<String, String> {
        unreachable!("identity-only test client")
    }

    fn auth_test(&self, _cfg: &RuntimeConfig) -> Result<String, String> {
        unreachable!("identity-only test client")
    }

    fn is_human_user(&self, _cfg: &RuntimeConfig, _user_id: &str) -> Result<bool, String> {
        self.results
            .lock()
            .expect("lock identity sequence")
            .pop_front()
            .expect("scripted identity result")
    }

    fn post_message(
        &self,
        _cfg: &RuntimeConfig,
        _channel_id: &str,
        _text: &str,
        _thread_ts: Option<&str>,
    ) -> Result<PostedMessage, String> {
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
        security_mode: SecurityMode::Strict,
        conversations: [("team".to_owned(), policy.clone())].into_iter().collect(),
        parent_receives: [("C123".to_owned(), policy.alias.clone())]
            .into_iter()
            .collect(),
        thread_receives: HashMap::new(),
        proactive_aliases: BTreeSet::new(),
        dynamic_direct_messages: None,
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

fn multi_channel_cfg() -> RuntimeConfig {
    let mut config = cfg();
    config.conversations.insert(
        "team-two".to_owned(),
        ConversationPolicy {
            alias: "team-two".to_owned(),
            conversation_id: "C456".to_owned(),
            kind: ConversationPolicyKind::Channel,
            receive: Some(ReceiveMode::MentionsOnly),
            description: None,
            thread_ts: None,
        },
    );
    reindex_receive_routes(&mut config);
    config
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

fn valid_config_message() -> HarnessOutputMessage {
    let mut secrets = BTreeMap::new();
    secrets.insert("app".to_owned(), tau_proto::SecretValue::new("xapp-test"));
    secrets.insert("bot".to_owned(), tau_proto::SecretValue::new("xoxb-test"));
    HarnessOutputMessage::Configure(tau_proto::Configure {
        tool_prefix: None,
        instance_name: None,
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
        instance_name: None,
        config: tau_proto::json_to_cbor(&serde_json::json!({
            "unknown_field": true,
        })),
        state_dir: None,
        secrets: BTreeMap::new(),
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
                    Event::ToolRegister(register) => Some(register),
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
            registration.tool.name.as_str() == format!("{prefix}_{SEND_TOOL_NAME}")
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
        if matches!(
            message,
            HarnessOutputMessage::Configure(configure) if !configure.secrets.is_empty()
        ) {
            writer
                .write_message(&HarnessOutputMessage::RegisterTransportCapabilityResult(
                    tau_proto::RegisterTransportCapabilityResult {
                        request_id: format!("{CAPABILITY_REQUEST_PREFIX}1"),
                        accepted: true,
                        error: None,
                    },
                ))
                .expect("write capability result");
        }
    }
    writer.flush().expect("flush input");

    let output = SharedWriter::default();
    let written = output.clone();
    run_with_client(std::io::Cursor::new(input), output, client).expect("run");

    let mut frames = Vec::new();
    let mut reader = tau_proto::HarnessInputReader::new(std::io::Cursor::new(written.bytes()));
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
    let ext = Extension::new(client.clone(), tx);
    ext.apply_config(cfg()).expect("config");
    {
        let mut state = ext.state.lock().expect("lock");
        state.bot_user_id = Some("UBOT123".to_owned());
        state.capability_active = true;
    }
    (ext, rx, client)
}

fn slack_message(channel_id: &str, channel_type: Option<&str>, text: &str) -> SlackMessage {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
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
    ext.state.lock().expect("state").capability_active = true;
}

fn recv_prompt(rx: &mpsc::Receiver<HarnessInputMessage>) -> String {
    let request = recv_prompt_request(rx);
    match request.draft.operation {
        MessageOperation::Create {
            payload: MessagePayload::Text { text, .. },
        } => text,
        operation => panic!("expected text create, got {operation:?}"),
    }
}

fn recv_prompt_request(
    rx: &mpsc::Receiver<HarnessInputMessage>,
) -> tau_proto::TransportMessageIngressRequest {
    loop {
        if let HarnessInputMessage::TransportMessageIngress(request) = rx.recv().expect("message") {
            return *request;
        }
    }
}

fn activate_prompt_origin(ext: &Extension, prompt: &tau_proto::TransportMessageIngressRequest) {
    activate_ingress_route(ext, prompt, "msg-test");
}

fn activate_ingress_route(
    ext: &Extension,
    prompt: &tau_proto::TransportMessageIngressRequest,
    message_id: &str,
) {
    apply_output_message(
        &HarnessOutputMessage::TransportMessageIngressResult(
            tau_proto::TransportMessageIngressResult {
                request_id: prompt.request_id.clone(),
                message_id: Some(MessageId::new(message_id)),
                outcome: Some(tau_proto::TransportMessageIngressOutcome::Accepted),
                error: None,
            },
        ),
        ext,
    );
}

fn ingress_text(request: &tau_proto::TransportMessageIngressRequest) -> &str {
    match &request.draft.operation {
        MessageOperation::Create {
            payload: MessagePayload::Text { text, .. },
        } => text,
        operation => panic!("expected text create, got {operation:?}"),
    }
}

fn assert_no_ingress(rx: &mpsc::Receiver<HarnessInputMessage>) {
    while let Ok(message) = rx.try_recv() {
        assert!(
            !matches!(message, HarnessInputMessage::TransportMessageIngress(_)),
            "unexpected typed ingress"
        );
    }
}

fn complete_pending_send(
    ext: &Extension,
    rx: &mpsc::Receiver<HarnessInputMessage>,
    accepted: bool,
) {
    loop {
        if let HarnessInputMessage::CompleteTransportSend(request) =
            rx.recv().expect("send completion request")
        {
            apply_output_message(
                &HarnessOutputMessage::CompleteTransportSendResult(
                    tau_proto::CompleteTransportSendResult {
                        request_id: request.request_id.clone(),
                        message_id: accepted.then(|| MessageId::new("msg-outgoing")),
                        accepted,
                        error: (!accepted).then(|| "rejected_for_test".to_owned()),
                    },
                ),
                ext,
            );
            return;
        }
    }
}

/// Commit-gated harness results, rather than Slack text or prompt lifecycle
/// strings, activate the exact opaque route used by successful send completion
/// and preserve the originating sender policy on that completion.
#[test]
fn typed_ingress_result_activates_exact_send_completion_route() {
    let (ext, rx, _client) = extension();
    ext.state
        .lock()
        .expect("lock")
        .config
        .as_mut()
        .expect("config")
        .security_mode = SecurityMode::Lax;
    register_agent(&ext, "agent-a");
    let mut message = slack_message("C123", None, "<@UBOT123> typed");
    message.user_id = "U999".to_owned();
    ext.process_slack_message(message);
    let ingress = recv_prompt_request(&rx);
    apply_output_message(
        &HarnessOutputMessage::TransportMessageIngressResult(
            tau_proto::TransportMessageIngressResult {
                request_id: ingress.request_id,
                message_id: Some(MessageId::new("msg-canonical")),
                outcome: Some(tau_proto::TransportMessageIngressOutcome::Accepted),
                error: None,
            },
        ),
        &ext,
    );

    let args = CborValue::Map(vec![
        (
            CborValue::Text("message".to_owned()),
            CborValue::Text("reply".to_owned()),
        ),
        (
            CborValue::Text("reply_to".to_owned()),
            CborValue::Text("msg-canonical".to_owned()),
        ),
    ]);
    assert!(
        ext.handle_send(tool(SEND_TOOL_NAME, "agent-a", args))
            .is_none()
    );
    let HarnessInputMessage::CompleteTransportSend(request) = rx.recv().expect("completion") else {
        panic!("expected successful-send completion");
    };
    assert_eq!(request.in_reply_to, Some(MessageId::new("msg-canonical")));
    assert_eq!(
        request.draft.policy_status,
        SenderPolicyStatus::LaxPermitted
    );
    assert_eq!(
        request.draft.conversation,
        Some(message_conversation(&slack_conversation("C123", None)))
    );
    assert!(
        ext.state
            .lock()
            .expect("lock")
            .posted_messages
            .get(&PostedMessageKey::new("C123", "1.0"))
            .is_none(),
        "post ownership must wait for durable completion"
    );
    apply_output_message(
        &HarnessOutputMessage::CompleteTransportSendResult(
            tau_proto::CompleteTransportSendResult {
                request_id: request.request_id.clone(),
                message_id: None,
                accepted: false,
                error: Some("rejected_for_test".to_owned()),
            },
        ),
        &ext,
    );
    assert!(
        ext.state
            .lock()
            .expect("lock")
            .posted_messages
            .get(&PostedMessageKey::new("C123", "1.0"))
            .is_none(),
        "rejected completion must never activate ownership"
    );
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

/// Canonical reply routes evict oldest selectors at their hard capacity instead
/// of growing connection state.
#[test]
fn typed_reply_route_state_is_bounded() {
    let mut state = State::default();
    for index in 0..=REPLY_ROUTE_LIMIT {
        state.insert_reply_route(
            MessageId::new(format!("msg-{index}")),
            ReplyRoute {
                agent_id: agent_id("agent-a"),
                conversation: slack_conversation("C123", None),
                user_id: "U123".to_owned(),
                policy_status: SenderPolicyStatus::Allowlisted,
            },
        );
    }
    assert_eq!(state.reply_routes.len(), REPLY_ROUTE_LIMIT);
    assert!(!state.reply_routes.contains_key(&MessageId::new("msg-0")));
}

/// Session capability renewal uses unique correlation ids and ignores late
/// results from an earlier harness generation.
#[test]
fn capability_renewal_ignores_stale_results() {
    let (ext, rx, _client) = extension();
    ext.request_transport_capability();
    let HarnessInputMessage::RegisterTransportCapability(first) = rx.recv().expect("first") else {
        panic!("expected capability request");
    };
    ext.request_transport_capability();
    let HarnessInputMessage::RegisterTransportCapability(second) = rx.recv().expect("second")
    else {
        panic!("expected capability request");
    };
    assert_ne!(first.request_id, second.request_id);
    apply_output_message(
        &HarnessOutputMessage::RegisterTransportCapabilityResult(
            tau_proto::RegisterTransportCapabilityResult {
                request_id: first.request_id,
                accepted: true,
                error: None,
            },
        ),
        &ext,
    );
    assert!(!ext.state.lock().expect("lock").capability_active);
    apply_output_message(
        &HarnessOutputMessage::RegisterTransportCapabilityResult(
            tau_proto::RegisterTransportCapabilityResult {
                request_id: second.request_id,
                accepted: true,
                error: None,
            },
        ),
        &ext,
    );
    assert!(ext.state.lock().expect("lock").capability_active);
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

/// Ensures a Socket Mode worker blocked on websocket receive exits promptly
/// when shutdown is requested, preserving shutdown latency without a receive
/// timeout.
#[tokio::test]
async fn socket_worker_once_shutdown_interrupts_idle_websocket_receive() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback websocket listener");
    let socket_url = format!(
        "ws://{}/socket-ticket",
        listener.local_addr().expect("listener local address")
    );
    let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
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
                socket_url,
            }),
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

/// Long successful Slack Web API JSON responses must be parsed from the raw
/// body rather than from bounded diagnostic text, otherwise successful sends
/// can be reported as false JSON errors.
#[test]
fn long_successful_slack_api_response_still_parses() {
    let cfg = cfg();
    let long_text = "x".repeat(MAX_DIAGNOSTIC_BYTES + 200);
    let body = serde_json::json!({
        "ok": true,
        "url": "wss://wss-primary.slack.com/link",
        "message": { "text": long_text }
    })
    .to_string();
    let value = parse_slack_api_response(&cfg, "chat.postMessage", 200, None, &body)
        .expect("long ok response parses");
    assert_eq!(
        value.get("ok").and_then(|value| value.as_bool()),
        Some(true)
    );
}

/// Slack API diagnostic responses are token-redacted and bounded without
/// affecting parsing of successful response bodies.
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
    let err =
        parse_slack_api_response(&cfg, "auth.test", 200, None, &body).expect_err("slack error");
    assert!(!err.contains(&cfg.app_token));
    assert!(!err.contains(&cfg.bot_token));
    assert!(err.len() <= MAX_DIAGNOSTIC_BYTES + 64, "{err}");
}

/// Slack bridge tools are disabled by default because roles must explicitly opt
/// into an external chat bridge before the model can use it.
#[test]
fn slack_tools_are_role_opt_in() {
    assert!(!register_tool_spec().enabled_by_default);
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
        send_tool_spec()
            .tags
            .iter()
            .any(|tag| tag.as_str() == SEND_TOOL_TAG)
    );
}

/// Provider-owned repair examples must remain schema-valid as Slack tool
/// argument shapes evolve.
#[test]
fn slack_tool_examples_are_schema_valid() {
    for spec in [register_tool_spec(), send_tool_spec()] {
        tau_core::validate_tool_examples(&spec)
            .unwrap_or_else(|error| panic!("invalid examples for {}: {error}", spec.name));
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
    assert_eq!(
        client.sent_pairs(),
        vec![("G789".to_owned(), "[agent-a] update".to_owned())]
    );
    assert_eq!(
        client.sent_thread_ids(),
        vec![Some("1720000000.123456".to_owned())]
    );
    let request = match rx.try_recv().expect("completion request") {
        HarnessInputMessage::CompleteTransportSend(request) => request,
        other => panic!("unexpected frame: {other:?}"),
    };
    assert!(request.in_reply_to.is_none());
    assert!(matches!(
        request.authorization,
        tau_proto::TransportSendAuthorization::ConfiguredDestination { ref alias }
            if alias == "incident-thread"
    ));
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
    let (tx, _rx) = mpsc::channel();
    let ext = Extension::new(Arc::new(FailingPostClient), tx);
    ext.apply_config(proactive_cfg()).expect("config");
    ext.state.lock().expect("state").capability_active = true;
    let result = ext.handle_send(tool(
        SEND_TOOL_NAME,
        "agent-a",
        tau_proto::json_to_cbor(&serde_json::json!({"message":"update","destination":"team-ops"})),
    ));
    assert!(matches!(result, Some(Event::ToolError(_))));
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
    assert!(ext.apply_config(cfg()).is_err());
    {
        let state = ext.state.lock().expect("state");
        assert!(state.capability_active);
        assert_eq!(state.pending_posts.len(), 1);
        assert_eq!(state.accepted_send_attempts.len(), 1);
        assert!(
            state
                .config
                .as_ref()
                .expect("config")
                .proactive_aliases
                .contains("team-ops")
        );
    }
    complete_pending_send(&ext, &rx, true);
    assert!(ext.state.lock().expect("state").pending_posts.is_empty());
    assert!(ext.handle_send(invoke).is_none());
    assert!(matches!(
        rx.try_recv(),
        Ok(HarnessInputMessage::CompleteTransportSend(_))
    ));
    assert_eq!(client.sent_pairs().len(), 1);
}

/// Identical delivery after Slack acceptance resubmits the typed completion
/// without posting again or fabricating a terminal tool result.
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
    assert!(ext.handle_send(invoke.clone()).is_none());
    let first = rx.try_recv().expect("first completion");
    assert!(ext.handle_send(invoke).is_none());
    let replay = rx.try_recv().expect("replayed completion");
    assert_eq!(format!("{first:?}"), format!("{replay:?}"));
    assert_eq!(client.sent_pairs().len(), 1);
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

/// Configured aliases are sorted and are the only destination identities
/// advertised by the refreshed model schema.
#[test]
fn proactive_schema_advertises_only_sorted_aliases() {
    let config = proactive_cfg();
    let spec = send_tool_spec_for_destinations(&config.conversations, &config.proactive_aliases);
    let parameters = spec.parameters.expect("parameters");
    let schema = parameters.as_object().expect("object schema");
    let destination = &schema["properties"]["destination"];
    assert_eq!(
        destination["enum"],
        serde_json::json!(["alice-dm", "incident-thread", "team-ops"])
    );
    let serialized = serde_json::to_string(&parameters).expect("schema json");
    assert!(serialized.contains("team-ops: Trusted ops hint"));
    assert!(!serialized.contains("\"team\""));
    assert!(!serialized.contains(DYNAMIC_DM_LABEL));
    for native in ["C456", "G789", "D123", "1720000000.123456"] {
        assert!(!serialized.contains(native), "schema leaked {native}");
    }
}

/// Slack-accepted channel and fixed-thread mismatches become stable failures;
/// replaying the same call id must neither repost nor leak native identifiers.
#[test]
fn accepted_response_route_mismatches_replay_without_reposting() {
    for (channel, thread, expected) in [
        ("C999", None, "conflicting destination conversation"),
        ("G789", Some("9.9"), "conflicting thread metadata"),
    ] {
        let client = Arc::new(MismatchedPostClient {
            posts: Mutex::new(0),
            returned_channel: channel.to_owned(),
            returned_thread: thread.map(str::to_owned),
        });
        let (tx, _rx) = mpsc::channel();
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
        for attempt in [invoke.clone(), invoke] {
            let Some(Event::ToolError(error)) = ext.handle_send(attempt) else {
                panic!("expected stable mismatch error");
            };
            assert!(error.message.contains(expected));
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
    let (ext, _rx, client) = extension();
    apply_test_config(&ext, proactive_cfg());
    let first = tool(
        SEND_TOOL_NAME,
        "agent-a",
        tau_proto::json_to_cbor(
            &serde_json::json!({"message": "first", "destination": "team-ops"}),
        ),
    );
    assert!(ext.handle_send(first.clone()).is_none());
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
    state.clear_accepted_send_attempts();
    for index in 0..=ACCEPTED_SEND_ATTEMPT_LIMIT {
        let invoke = ToolStarted {
            call_id: format!("bounded-{index}").into(),
            ..tool(SEND_TOOL_NAME, "agent-a", CborValue::Null)
        };
        state.remember_accepted_send(
            &invoke,
            AcceptedSendDisposition::Rejected("accepted".to_owned()),
        );
    }
    assert_eq!(
        state.accepted_send_attempts.len(),
        ACCEPTED_SEND_ATTEMPT_LIMIT
    );
    assert!(
        !state
            .accepted_send_attempts
            .contains_key(&tau_proto::ToolCallId::from("bounded-0"))
    );
    drop(state);
    let mut reconfigured = proactive_cfg();
    reconfigured.max_message_bytes -= 1;
    assert!(ext.apply_config(reconfigured).is_err());
    let state = ext.state.lock().expect("state");
    assert!(!state.accepted_send_attempts.is_empty());
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

/// Changing the configured channel set before startup clears post ownership
/// along with registrations so old destinations cannot survive reconfiguration.
#[test]
fn channel_reconfiguration_clears_post_ownership() {
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
    {
        let mut state = ext.state.lock().expect("state");
        state.capability_active = true;
        state.pending_capability_request = Some("old-capability".to_owned());
        state
            .duplicate_events
            .insert_new("old-local-effect".to_owned());
    }
    ext.apply_config(multi_channel_cfg()).expect("reconfigure");
    let state = ext.state.lock().expect("lock");
    assert!(state.registered_agents.is_empty());
    assert!(!state.capability_active);
    assert!(state.pending_capability_request.is_none());
    assert!(state.duplicate_events.seen.is_empty());
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
                    Event::ToolError(error)
                        if error.tool_name.as_str() == REGISTER_TOOL_NAME
                            && error.message.contains("capability is not active")
                )
        )),
        "register should fail after malformed config clears active config"
    );
    assert_eq!(*client.auth_count.lock().expect("lock"), 0);
    assert_eq!(*client.open_count.lock().expect("lock"), 0);
}

/// Real protocol handling must refresh the send schema for configured aliases
/// and replace it with the reply-only schema after malformed reconfiguration,
/// so a stale prompt can never retain a revoked proactive destination.
#[test]
fn run_config_refreshes_and_bad_config_removes_proactive_schema() {
    let frames = run_protocol_messages(
        &[proactive_config_message(), malformed_config_message()],
        FakeClient::new(),
    );
    let registrations = frames
        .iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::Emit(emit) => match emit.event.as_ref() {
                Event::ToolRegister(register) if register.tool.name.as_str() == SEND_TOOL_NAME => {
                    Some(&register.tool)
                }
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(registrations.iter().any(|tool| {
        tool.parameters
            .as_ref()
            .and_then(|schema| schema["properties"]["destination"]["enum"].as_array())
            .is_some_and(|aliases| aliases.as_slice() == [serde_json::json!("team-ops")])
    }));
    let final_schema = registrations
        .last()
        .and_then(|tool| tool.parameters.as_ref())
        .expect("reply-only schema refresh");
    assert!(final_schema["properties"].get("destination").is_none());
    assert_eq!(
        final_schema["required"],
        serde_json::json!(["message", "reply_to"])
    );
}

/// Initial configuration-derived tool refreshes are emitted after static
/// declarations but before Ready, so the effective startup schema retains
/// proactive destination aliases.
#[test]
fn initial_proactive_schema_override_is_final_before_ready() {
    let frames = run_protocol_messages(&[proactive_config_message()], FakeClient::new());
    let registrations = frames
        .iter()
        .enumerate()
        .filter_map(|(index, frame)| match frame {
            HarnessInputMessage::Emit(emit) => match emit.event.as_ref() {
                Event::ToolRegister(register) if register.tool.name.as_str() == SEND_TOOL_NAME => {
                    Some((index, &register.tool))
                }
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    let (override_index, final_tool) = registrations.last().expect("send registration");
    assert!(
        final_tool
            .parameters
            .as_ref()
            .and_then(|schema| schema["properties"]["destination"]["enum"].as_array())
            .is_some_and(|aliases| aliases.as_slice() == [serde_json::json!("team-ops")])
    );
    let ready_index = frames
        .iter()
        .position(|frame| matches!(frame, HarnessInputMessage::Ready(_)))
        .expect("Ready");
    assert!(*override_index < ready_index);
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
                Event::ToolError(error)
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
                    session_id: "s1".into(),
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
                Event::ToolError(error)
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
                    Event::ToolError(error)
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
                    Event::ToolResult(result)
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
    assert!(matches!(result, Event::ToolResult(_)));
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
    assert!(matches!(result, Event::ToolResult(_)));
    let state = ext.state.lock().expect("lock");
    assert!(!state.registered_agents.contains(&agent_id("agent-a")));
    assert!(state.selected_agent_by_route.is_empty());
    assert!(
        state
            .posted_messages
            .get(&PostedMessageKey::new("C123", "1.0"))
            .is_none()
    );
}

/// Registration performs a bounded Slack auth/open preflight before reporting
/// success, so bad tokens are visible as tool errors instead of silent
/// background-only failures.
#[test]
fn slack_register_reports_initial_auth_failure() {
    let (tx, _rx) = mpsc::channel();
    let ext = Extension::new(Arc::new(FailingAuthClient), tx);
    ext.apply_config(cfg()).expect("config");
    ext.state.lock().expect("lock").capability_active = true;
    let event = ext.handle_register(tool(REGISTER_TOOL_NAME, "agent-a", bool_args(true)));
    let Event::ToolError(err) = event else {
        panic!("expected tool error");
    };
    assert!(err.message.contains("invalid_auth"));
    assert!(!ext.state.lock().expect("lock").config_frozen);
    let mut replacement = cfg();
    replacement.max_message_bytes -= 1;
    ext.apply_config(replacement)
        .expect("failed preflight remains reconfigurable");
}

/// Registered agents reply only to the configured conversation from which
/// the exact source-bound canonical selector returned by durable ingress.
#[test]
fn slack_send_uses_originating_conversation() {
    let (ext, rx, client) = extension();
    apply_test_config(&ext, multi_channel_cfg());
    register_agent(&ext, "agent-a");
    ext.process_slack_message(slack_message("C456", None, "<@UBOT123> hello"));
    let prompt = recv_prompt_request(&rx);
    assert_eq!(ingress_text(&prompt), "hello");
    activate_prompt_origin(&ext, &prompt);
    let result = ext.handle_send(tool(SEND_TOOL_NAME, "agent-a", message_args("hello")));
    assert!(result.is_none());
    assert_eq!(
        client.sent_pairs(),
        vec![("C456".to_owned(), "[agent-a] hello".to_owned())]
    );
}

/// Root messages keep replies top-level while thread messages automatically
/// carry their originating root without any model-supplied destination.
#[test]
fn slack_send_preserves_root_and_thread_context() {
    let (ext, rx, client) = extension();
    register_agent(&ext, "agent-a");

    ext.process_slack_message(slack_message("C123", None, "<@UBOT123> root"));
    let root = recv_prompt_request(&rx);
    activate_prompt_origin(&ext, &root);
    assert!(
        ext.handle_send(tool(SEND_TOOL_NAME, "agent-a", message_args("root reply")))
            .is_none()
    );

    let mut threaded = slack_message("C123", None, "<@UBOT123> threaded");
    threaded.thread_ts = Some("42.0".to_owned());
    ext.process_slack_message(threaded);
    let threaded = recv_prompt_request(&rx);
    activate_prompt_origin(&ext, &threaded);
    let mut thread_send = tool(SEND_TOOL_NAME, "agent-a", message_args("thread reply"));
    thread_send.call_id = tau_proto::ToolCallId::new("call-thread");
    assert!(ext.handle_send(thread_send).is_none());

    assert_eq!(
        client.sent_thread_ids(),
        vec![None, Some("42.0".to_owned())]
    );
}

/// Authorized reactions to an agent-owned bridge post preserve structured
/// source metadata, and retries are resubmitted with the same durable identity.
/// This guards `DESIGN-tau-ext-slack-reaction-ownership`.
#[test]
fn authorized_reactions_to_agent_posts_preserve_durable_dedup_identity() {
    let (ext, rx, _client) = extension();
    register_agent(&ext, "agent-a");
    ext.process_slack_message(slack_message("C123", None, "<@UBOT123> question"));
    let prompt = recv_prompt_request(&rx);
    activate_prompt_origin(&ext, &prompt);
    assert!(
        ext.handle_send(tool(SEND_TOOL_NAME, "agent-a", message_args("answer")))
            .is_none()
    );
    complete_pending_send(&ext, &rx, true);
    let reaction = slack_reaction("ER1", "reaction_added", "C123", "1.0");
    let mut reaction = reaction;
    reaction.reaction = "thumbsup::skin-tone-6".to_owned();
    ext.process_slack_reaction(reaction);
    let prompt = recv_prompt_request(&rx);
    assert_eq!(prompt.target_agent_id, agent_id("agent-a"));
    assert_eq!(
        prompt.draft.external_endpoint,
        MessageEndpoint::External {
            stable_id: Some("U123".to_owned()),
            display_name: None,
            actor_kind: ExternalActorKind::Human,
        }
    );
    assert_eq!(
        prompt.draft.conversation,
        Some(message_conversation(&slack_conversation("C123", None)))
    );
    assert_eq!(
        prompt
            .draft
            .external_identity
            .as_ref()
            .and_then(|identity| identity.event_id.as_deref()),
        Some("ER1")
    );
    assert!(matches!(
        prompt.draft.operation,
        MessageOperation::Reaction {
            action: ReactionAction::Add,
            ..
        }
    ));
    ext.process_slack_reaction(slack_reaction("ER1", "reaction_added", "C123", "1.0"));
    assert!(matches!(
        rx.try_recv(),
        Ok(HarnessInputMessage::TransportMessageIngress(_))
    ));
}

/// A commit-confirmed Slack create makes later root-message edits immutable
/// typed mutations that reference both canonical and native original identity.
#[test]
fn committed_root_message_edit_references_canonical_original() {
    let (ext, rx, _client) = extension();
    register_agent(&ext, "agent-a");
    let mut original_message = slack_message("C123", None, "<@UBOT123> original");
    original_message.ts = Some("1.0".to_owned());
    ext.process_slack_message(original_message);
    let original = recv_prompt_request(&rx);
    activate_prompt_origin(&ext, &original);

    ext.process_slack_edit(slack_edit("EE1", "C123", "1.0", None, "edited"));
    let edit = recv_prompt_request(&rx);
    assert_eq!(edit.target_agent_id, agent_id("agent-a"));
    assert_eq!(
        edit.draft.operation,
        MessageOperation::Edit {
            target: MessageRef {
                message_id: Some(MessageId::new("msg-test")),
                external_message_id: Some("1.0".to_owned()),
            },
            payload: MessagePayload::Text {
                text: "edited".to_owned(),
                format: TextFormat::Plain,
            },
        }
    );
    let identity = edit.draft.external_identity.expect("edit identity");
    assert_eq!(identity.event_id.as_deref(), Some("EE1"));
    assert_eq!(identity.message_id.as_deref(), Some("1.0"));
    assert_eq!(identity.revision_id.as_deref(), Some("2.0"));
}

/// Incoming native identity ownership is bounded, immutable on collision, and
/// remains synchronized through agent removal and full clear.
#[test]
fn incoming_edit_identity_cache_is_bounded_and_immutable() {
    let mut state = State::default();
    for index in 0..=REPLY_ROUTE_LIMIT {
        let key = PostedMessageKey::new("C123", &format!("{index}.0"));
        assert!(state.insert_incoming_message(
            key,
            IncomingMessageOwner {
                agent_id: agent_id("agent-a"),
                message_id: MessageId::new(format!("msg-{index}")),
                conversation: slack_conversation("C123", None),
                user_id: "U123".to_owned(),
            },
        ));
    }
    assert_eq!(state.incoming_messages.len(), REPLY_ROUTE_LIMIT);
    let key = PostedMessageKey::new("C123", "1.0");
    let original = state.incoming_messages.get(&key).cloned().expect("owner");
    assert!(!state.insert_incoming_message(
        key.clone(),
        IncomingMessageOwner {
            agent_id: agent_id("agent-b"),
            message_id: MessageId::new("msg-conflict"),
            conversation: slack_conversation("C123", Some("9.0")),
            user_id: "U999".to_owned(),
        },
    ));
    assert!(state.incoming_messages.get(&key) == Some(&original));
    state.remove_agent_incoming_messages(&agent_id("agent-a"));
    assert!(state.incoming_messages.is_empty());
    assert!(state.incoming_message_order.is_empty());
    state.clear_incoming_messages();
}

/// Thread edits preserve the original thread route, retries retain one durable
/// dedup identity, and unknown/conflicting originals fail closed.
#[test]
fn thread_edit_retry_and_original_confusion_fail_closed() {
    let (ext, rx, _client) = extension();
    register_agent(&ext, "agent-a");
    let mut original = slack_message("C123", None, "<@UBOT123> original");
    original.ts = Some("1.0".to_owned());
    original.thread_ts = Some("9.0".to_owned());
    ext.process_slack_message(original);
    let ingress = recv_prompt_request(&rx);
    activate_prompt_origin(&ext, &ingress);

    let edit = slack_edit("EE2", "C123", "1.0", Some("9.0"), "edited");
    ext.process_slack_edit(slack_edit("UNKNOWN", "C123", "8.0", Some("9.0"), "bad"));
    ext.process_slack_edit(slack_edit("THREAD", "C123", "1.0", Some("8.0"), "bad"));
    assert_no_ingress(&rx);
    ext.process_slack_edit(edit);
    ext.process_slack_edit(slack_edit("EE2", "C123", "1.0", Some("9.0"), "edited"));
    let first = recv_prompt_request(&rx);
    let second = recv_prompt_request(&rx);
    assert_eq!(
        first.draft.external_identity,
        second.draft.external_identity
    );
    assert_eq!(
        first.draft.conversation,
        Some(message_conversation(&slack_conversation(
            "C123",
            Some("9.0")
        )))
    );
}

/// Commit-confirmed edit and reaction occurrences each install their own exact
/// opaque reply route rather than borrowing only the original create authority.
#[test]
fn committed_mutations_install_independent_reply_authority() {
    let (ext, rx, client) = extension();
    register_agent(&ext, "agent-a");
    let mut original = slack_message("C123", Some("channel"), "<@UBOT123> original");
    original.ts = Some("1.0".to_owned());
    ext.process_slack_message(original);
    let create = recv_prompt_request(&rx);
    activate_prompt_origin(&ext, &create);

    ext.process_slack_edit(slack_edit("edit", "C123", "1.0", None, "changed"));
    let edit = recv_prompt_request(&rx);
    activate_ingress_route(&ext, &edit, "msg-edit");
    assert!(
        ext.handle_send(tool(
            SEND_TOOL_NAME,
            "agent-a",
            tau_proto::json_to_cbor(
                &serde_json::json!({"message":"edit reply","reply_to":"msg-edit"}),
            ),
        ))
        .is_none()
    );
    complete_pending_send(&ext, &rx, true);

    ext.remember_posted_message(
        slack_conversation("C123", None),
        PostedMessage {
            channel_id: "C123".to_owned(),
            ts: "9.0".to_owned(),
            thread_ts: None,
        },
        agent_id("agent-a"),
    );
    ext.process_slack_reaction(slack_reaction("reaction", "reaction_added", "C123", "9.0"));
    let reaction = recv_prompt_request(&rx);
    activate_ingress_route(&ext, &reaction, "msg-reaction");
    let mut reaction_reply = tool(
        SEND_TOOL_NAME,
        "agent-a",
        tau_proto::json_to_cbor(
            &serde_json::json!({"message":"reaction reply","reply_to":"msg-reaction"}),
        ),
    );
    reaction_reply.call_id = "call-reaction-reply".into();
    assert!(ext.handle_send(reaction_reply).is_none());
    assert!(
        client
            .sent_pairs()
            .iter()
            .any(|(_, text)| text.ends_with("reaction reply"))
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
    let post = posted_message_from_response(&serde_json::json!({
        "channel": "C123",
        "ts": "12.34",
        "message": { "thread_ts": "not-a-ts" }
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
        serde_json::json!({ "channel": "C123", "text": "root" })
    );
    assert_eq!(
        post_message_body("C123", "reply", Some("42.0")),
        serde_json::json!({
            "channel": "C123",
            "text": "reply",
            "thread_ts": "42.0"
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
                thread_ts: None,
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

/// `users.info` must put its required user argument in a form body rather than
/// a JSON body, which Slack treats as a missing user and reports as
/// `user_not_found`.
#[test]
fn users_info_uses_form_encoding() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test API");
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
            .is_human_user(&cfg, "U123")
            .expect("form-encoded users.info")
    );
    server.join().expect("users.info test server");
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

/// Proactive-only posts cannot receive reactions without covering receive
/// policy, while a parent receive route covers a proactive fixed-thread post.
#[test]
fn reaction_authority_requires_covering_receive_policy() {
    let (ext, rx, _client) = extension();
    apply_test_config(&ext, proactive_cfg());
    assert!(
        ext.handle_send(tool(
            SEND_TOOL_NAME,
            "agent-a",
            tau_proto::json_to_cbor(
                &serde_json::json!({"message":"post","destination":"team-ops"}),
            ),
        ))
        .is_none()
    );
    complete_pending_send(&ext, &rx, true);
    ext.process_slack_reaction(slack_reaction(
        "proactive-only",
        "reaction_added",
        "C456",
        "1.0",
    ));
    assert!(rx.try_recv().is_err());

    let (ext, rx, _client) = extension();
    let mut config = cfg();
    let child = ConversationPolicy {
        alias: "child".to_owned(),
        conversation_id: "C123".to_owned(),
        kind: ConversationPolicyKind::Channel,
        receive: None,
        description: None,
        thread_ts: Some("7.0".to_owned()),
    };
    config
        .conversations
        .insert(child.alias.clone(), child.clone());
    config.proactive_aliases.insert(child.alias.clone());
    apply_test_config(&ext, config);
    register_agent(&ext, "agent-a");
    assert!(
        ext.handle_send(tool(
            SEND_TOOL_NAME,
            "agent-a",
            tau_proto::json_to_cbor(
                &serde_json::json!({"message":"thread post","destination":"child"}),
            ),
        ))
        .is_none()
    );
    complete_pending_send(&ext, &rx, true);
    ext.process_slack_reaction(slack_reaction("covered", "reaction_added", "C123", "1.0"));
    let reaction = recv_prompt_request(&rx);
    let conversation = reaction.draft.conversation.expect("conversation");
    assert_eq!(conversation.display_name.as_deref(), Some("team"));
    assert_eq!(
        conversation.thread.expect("thread").stable_id,
        "7.0".to_owned()
    );
}

/// A receive-enabled fixed-thread sibling does not authorize reactions to a
/// proactive post under a different root in the same native conversation.
#[test]
fn fixed_thread_sibling_does_not_cover_proactive_reaction() {
    let (ext, rx, _client) = extension();
    let mut config = cfg();
    config.conversations.clear();
    let receive = ConversationPolicy {
        alias: "receive-eight".to_owned(),
        conversation_id: "C777".to_owned(),
        kind: ConversationPolicyKind::Channel,
        receive: Some(ReceiveMode::AllMessages),
        description: None,
        thread_ts: Some("8.0".to_owned()),
    };
    let proactive = ConversationPolicy {
        alias: "send-seven".to_owned(),
        conversation_id: "C777".to_owned(),
        kind: ConversationPolicyKind::Channel,
        receive: None,
        description: None,
        thread_ts: Some("7.0".to_owned()),
    };
    config.conversations.insert(receive.alias.clone(), receive);
    config
        .conversations
        .insert(proactive.alias.clone(), proactive.clone());
    config.proactive_aliases.insert(proactive.alias);
    apply_test_config(&ext, config);
    register_agent(&ext, "agent-a");
    assert!(
        ext.handle_send(tool(
            SEND_TOOL_NAME,
            "agent-a",
            tau_proto::json_to_cbor(
                &serde_json::json!({"message":"thread post","destination":"send-seven"}),
            ),
        ))
        .is_none()
    );
    complete_pending_send(&ext, &rx, true);
    ext.process_slack_reaction(slack_reaction(
        "wrong-sibling",
        "reaction_added",
        "C777",
        "1.0",
    ));
    assert!(rx.try_recv().is_err());
}

/// Reaction prompts use the authenticated thread from the original outbound
/// request even when Slack omits it in the response, and conflicting event
/// metadata can never redirect a root or threaded post.
#[test]
fn reaction_routing_uses_cached_authenticated_thread_only() {
    let (ext, rx, client) = extension();
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
    let mut reaction = slack_reaction("ER-THREAD", "reaction_added", "C123", "1.0");
    reaction.thread_ts = None;
    ext.process_slack_reaction(reaction);
    let prompt = recv_prompt_request(&rx);
    activate_prompt_origin(&ext, &prompt);
    assert!(
        ext.handle_send(tool(
            SEND_TOOL_NAME,
            "agent-a",
            message_args("thread reply")
        ))
        .is_none()
    );
    assert_eq!(
        client.sent_thread_ids().last(),
        Some(&Some("10.0".to_owned()))
    );

    ext.remember_posted_message(
        slack_conversation("C123", None),
        PostedMessage {
            channel_id: "C123".to_owned(),
            ts: "2.0".to_owned(),
            thread_ts: None,
        },
        agent_id("agent-a"),
    );
    let mut root_conflict = slack_reaction("ER-ROOT-CONFLICT", "reaction_added", "C123", "2.0");
    root_conflict.thread_ts = Some("99.0".to_owned());
    ext.process_slack_reaction(root_conflict);

    ext.remember_posted_message(
        slack_conversation("C123", Some("10.0")),
        PostedMessage {
            channel_id: "C123".to_owned(),
            ts: "3.0".to_owned(),
            thread_ts: Some("10.0".to_owned()),
        },
        agent_id("agent-a"),
    );
    let mut thread_conflict = slack_reaction("ER-THREAD-CONFLICT", "reaction_added", "C123", "3.0");
    thread_conflict.thread_ts = Some("99.0".to_owned());
    ext.process_slack_reaction(thread_conflict);

    ext.remember_posted_message(
        slack_conversation("C123", None),
        PostedMessage {
            channel_id: "C123".to_owned(),
            ts: "4.0".to_owned(),
            thread_ts: Some("99.0".to_owned()),
        },
        agent_id("agent-a"),
    );
    ext.process_slack_reaction(slack_reaction(
        "ER-RESPONSE-CONFLICT",
        "reaction_added",
        "C123",
        "4.0",
    ));
    assert_no_ingress(&rx);
}

/// A registered agent cannot send proactively merely because channels are
/// configured; an authorized inbound route must establish the destination.
#[test]
fn slack_send_rejects_missing_or_forged_origin_context() {
    let (ext, _rx, client) = extension();
    register_agent(&ext, "agent-a");
    assert!(matches!(
        ext.handle_send(tool(SEND_TOOL_NAME, "agent-a", message_args("hello"))),
        Some(Event::ToolError(_))
    ));
    ext.state.lock().expect("lock").reply_routes.insert(
        MessageId::new("msg-test"),
        ReplyRoute {
            agent_id: agent_id("agent-a"),
            conversation: SlackConversation {
                channel_id: "C999".to_owned(),
                thread_ts: Some("9.0".to_owned()),
                kind: ConversationPolicyKind::Channel,
                alias: "team".to_owned(),
            },
            user_id: "U123".to_owned(),
            policy_status: SenderPolicyStatus::Allowlisted,
        },
    );
    assert!(matches!(
        ext.handle_send(tool(SEND_TOOL_NAME, "agent-a", message_args("hello"))),
        Some(Event::ToolError(_))
    ));
    assert!(client.sent.lock().expect("lock").is_empty());
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

/// Lax mode admits a verified non-allowlisted human only in a configured
/// conversation and labels that independent policy fact in the typed draft.
#[test]
fn lax_mode_routes_verified_human_with_lax_policy() {
    let (ext, rx, _client) = extension();
    ext.state
        .lock()
        .expect("lock")
        .config
        .as_mut()
        .expect("config")
        .security_mode = SecurityMode::Lax;
    register_agent(&ext, "agent-a");
    let mut msg = slack_message("C123", None, "<@UBOT123> hello");
    msg.user_id = "U999".to_owned();
    ext.process_slack_message(msg);
    let HarnessInputMessage::TransportMessageIngress(request) = rx.recv().expect("lax ingress")
    else {
        panic!("expected typed ingress");
    };
    assert_eq!(
        request.draft.identity_assurance,
        SenderIdentityAssurance::VerifiedAccount
    );
    assert_eq!(
        request.draft.policy_status,
        SenderPolicyStatus::LaxPermitted
    );
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

/// Each configured channel keeps its own selected Tau agent, so commands in
/// one shared Slack conversation cannot redirect another conversation.
#[test]
fn configured_channels_keep_independent_agent_selections() {
    let (ext, rx, _client) = extension();
    apply_test_config(&ext, multi_channel_cfg());
    register_agent(&ext, "agent-alpha");
    register_agent(&ext, "agent-beta");

    ext.process_slack_message(slack_message("C123", None, "<@UBOT123> select agent-alpha"));
    ext.process_slack_message(slack_message("C456", None, "<@UBOT123> select agent-beta"));
    let mut first_message = slack_message("C123", None, "<@UBOT123> first");
    first_message.thread_ts = Some("10.0".to_owned());
    let mut second_message = slack_message("C456", None, "<@UBOT123> second");
    second_message.thread_ts = Some("20.0".to_owned());
    ext.process_slack_message(first_message);
    ext.process_slack_message(second_message);

    let first = recv_prompt_request(&rx);
    let second = recv_prompt_request(&rx);
    assert_eq!(first.target_agent_id, agent_id("agent-alpha"));
    assert_eq!(ingress_text(&first), "first");
    assert_eq!(second.target_agent_id, agent_id("agent-beta"));
    assert_eq!(ingress_text(&second), "second");
}

/// Receive-enabled fixed-thread siblings own independent selected-agent state.
#[test]
fn fixed_thread_siblings_keep_independent_agent_selections() {
    let (ext, rx, _client) = extension();
    let mut config = cfg();
    config.conversations.clear();
    for (alias, root) in [("one", "1.0"), ("two", "2.0")] {
        config.conversations.insert(
            alias.to_owned(),
            ConversationPolicy {
                alias: alias.to_owned(),
                conversation_id: "C777".to_owned(),
                kind: ConversationPolicyKind::Channel,
                receive: Some(ReceiveMode::MentionsOnly),
                description: None,
                thread_ts: Some(root.to_owned()),
            },
        );
    }
    apply_test_config(&ext, config);
    register_agent(&ext, "agent-one");
    register_agent(&ext, "agent-two");
    for (root, agent) in [("1.0", "agent-one"), ("2.0", "agent-two")] {
        let mut select = slack_message(
            "C777",
            Some("channel"),
            &format!("<@UBOT123> select {agent}"),
        );
        select.thread_ts = Some(root.to_owned());
        ext.process_slack_message(select);
    }
    for (root, text) in [("1.0", "first"), ("2.0", "second")] {
        let mut message = slack_message("C777", Some("channel"), &format!("<@UBOT123> {text}"));
        message.thread_ts = Some(root.to_owned());
        ext.process_slack_message(message);
    }
    assert_eq!(
        recv_prompt_request(&rx).target_agent_id,
        agent_id("agent-one")
    );
    assert_eq!(
        recv_prompt_request(&rx).target_agent_id,
        agent_id("agent-two")
    );
}

/// Dynamic DM discovery requires `start` and retains multiple exact links.
#[test]
fn dm_linking_is_explicit_and_multi_link() {
    let (ext, rx, client) = extension();
    apply_test_config(&ext, dm_cfg());
    register_agent(&ext, "agent-a");
    ext.process_slack_message(slack_message("D123", Some("im"), "hello before start"));
    assert!(rx.try_recv().is_err());
    assert!(
        client.sent.lock().expect("lock")[0]
            .text
            .contains("Send start")
    );

    ext.process_slack_message(slack_message("D123", Some("im"), "start"));
    assert!(
        ext.state
            .lock()
            .expect("lock")
            .linked_dms
            .contains_key("D123")
    );
    ext.process_slack_message(slack_message("D999", Some("im"), "start"));
    assert!(
        ext.state
            .lock()
            .expect("lock")
            .linked_dms
            .contains_key("D999")
    );
    ext.process_slack_message(slack_message("D123", Some("im"), "first DM"));
    ext.process_slack_message(slack_message("D999", Some("im"), "second DM"));
    assert_eq!(recv_prompt(&rx), "first DM");
    assert_eq!(recv_prompt(&rx), "second DM");
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

/// DM mode keeps the existing explicit `start` link and derives outbound
/// replies from later prompts routed through that allowlisted DM.
#[test]
fn dm_send_uses_linked_prompt_origin() {
    let (ext, rx, client) = extension();
    apply_test_config(&ext, dm_cfg());
    register_agent(&ext, "agent-a");
    ext.process_slack_message(slack_message("D123", Some("im"), "start"));
    let mut question = slack_message("D123", Some("im"), "question");
    question.thread_ts = Some("7.0".to_owned());
    ext.process_slack_message(question);
    let prompt = recv_prompt_request(&rx);
    assert_eq!(ingress_text(&prompt), "question");
    activate_prompt_origin(&ext, &prompt);
    assert!(
        ext.handle_send(tool(SEND_TOOL_NAME, "agent-a", message_args("answer")))
            .is_none()
    );
    assert_eq!(
        client.sent_pairs().last(),
        Some(&("D123".to_owned(), "[agent-a] answer".to_owned()))
    );
    assert_eq!(
        client.sent_thread_ids().last(),
        Some(&Some("7.0".to_owned()))
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

/// `agents`, `select`, and `to` use stable agent ids in Slack-visible routing
/// commands while display names remain parenthetical context only.
#[test]
fn agents_select_and_to_route_by_agent_id() {
    let (ext, rx, client) = extension();
    register_agent(&ext, "agent-alpha");
    register_agent(&ext, "agent-beta");
    ext.state
        .lock()
        .expect("lock")
        .agent_labels
        .insert(agent_id("agent-alpha"), "Alpha Display".to_owned());

    ext.process_slack_message(slack_message("C123", None, "<@UBOT123> agents"));
    assert!(
        client.sent.lock().expect("lock")[0]
            .text
            .contains("agent-alpha (Alpha Display)")
    );

    ext.process_slack_message(slack_message("C123", None, "<@UBOT123> select agent-al"));
    ext.process_slack_message(slack_message("C123", None, "<@UBOT123> later"));
    assert_eq!(recv_prompt(&rx), "later");

    ext.process_slack_message(slack_message(
        "C123",
        None,
        "<@UBOT123> to agent-beta direct",
    ));
    assert_eq!(recv_prompt(&rx), "direct");
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

/// Slack `message_changed` decoding requires consistent original/editor/thread
/// metadata and retains explicit native revision identity.
#[test]
fn message_changed_envelopes_decode_as_edits_and_reject_conflicts() {
    let envelope = |message_ts: &str, previous_ts: &str, thread: Option<&str>| {
        serde_json::json!({
            "payload": {
                "type": "event_callback",
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

/// All-messages scope accepts an eligible unmentioned configured-channel post,
/// while duplicate `message` and `app_mention` deliveries share native dedup.
#[test]
fn all_messages_accepts_unmentioned_posts_and_shares_durable_identity() {
    let (ext, rx, _client) = extension();
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
    let mut message = slack_message("C123", None, "ordinary channel text");
    message.event_type = "message".to_owned();
    let mut mention = message.clone();
    mention.event_type = "app_mention".to_owned();
    mention.event_id = Some("E-mention-copy".to_owned());
    ext.process_slack_message(message);
    ext.process_slack_message(mention);
    let first = recv_prompt_request(&rx);
    let second = recv_prompt_request(&rx);
    assert_eq!(ingress_text(&first), "ordinary channel text");
    assert_eq!(first.draft, second.draft);
    assert_eq!(
        first
            .draft
            .external_identity
            .as_ref()
            .and_then(|identity| identity.dedup_key.as_ref()),
        second
            .draft
            .external_identity
            .as_ref()
            .and_then(|identity| identity.dedup_key.as_ref())
    );
    assert!(
        first
            .draft
            .external_identity
            .as_ref()
            .and_then(|identity| identity.dedup_key.as_deref())
            .is_some_and(|key| key.starts_with("message:C123:"))
    );
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
        serde_json::json!([{"alias":"receive","conversation_id":"C123","kind":"channel","receive":"all_messages","description":"not proactive"}]),
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

/// Fixed-thread roots normalize their root create, MPIMs retain group metadata,
/// and static DMs receive without dynamic `start`.
#[test]
fn static_route_kinds_and_fixed_threads_normalize_ingress() {
    let (ext, rx, _client) = extension();
    let mut config = cfg();
    config.conversations.clear();
    for policy in [
        ConversationPolicy {
            alias: "fixed".to_owned(),
            conversation_id: "C777".to_owned(),
            kind: ConversationPolicyKind::Channel,
            receive: Some(ReceiveMode::MentionsOnly),
            description: None,
            thread_ts: Some("7.0".to_owned()),
        },
        ConversationPolicy {
            alias: "group".to_owned(),
            conversation_id: "G777".to_owned(),
            kind: ConversationPolicyKind::Mpim,
            receive: Some(ReceiveMode::AllMessages),
            description: None,
            thread_ts: None,
        },
        ConversationPolicy {
            alias: "alice".to_owned(),
            conversation_id: "D777".to_owned(),
            kind: ConversationPolicyKind::Dm,
            receive: Some(ReceiveMode::AllMessages),
            description: None,
            thread_ts: None,
        },
    ] {
        config.conversations.insert(policy.alias.clone(), policy);
    }
    apply_test_config(&ext, config);
    register_agent(&ext, "agent-a");

    let mut root = slack_message("C777", Some("channel"), "<@UBOT123> root");
    root.ts = Some("7.0".to_owned());
    ext.process_slack_message(root);
    let root = recv_prompt_request(&rx);
    let root_conversation = root.draft.conversation.expect("conversation");
    assert_eq!(root_conversation.display_name.as_deref(), Some("fixed"));
    assert_eq!(
        root_conversation
            .thread
            .as_ref()
            .map(|thread| thread.stable_id.as_str()),
        Some("7.0")
    );

    let mut mpim = slack_message("G777", Some("mpim"), "group");
    mpim.event_type = "message".to_owned();
    ext.process_slack_message(mpim);
    assert_eq!(
        recv_prompt_request(&rx)
            .draft
            .conversation
            .expect("conversation")
            .kind,
        ConversationKind::Group
    );

    ext.process_slack_message(slack_message("D777", Some("im"), "direct"));
    assert_eq!(recv_prompt(&rx), "direct");
}

/// Private-channel, parent-thread, fixed-thread, and explicit kind matching
/// cover the remaining native receive matrix and reject sibling/type ambiguity.
#[test]
fn channel_kind_and_thread_receive_matrix_is_exact() {
    let (ext, rx, _client) = extension();
    let mut config = cfg();
    for policy in [
        ConversationPolicy {
            alias: "private".to_owned(),
            conversation_id: "G222".to_owned(),
            kind: ConversationPolicyKind::Channel,
            receive: Some(ReceiveMode::AllMessages),
            description: None,
            thread_ts: None,
        },
        ConversationPolicy {
            alias: "parent".to_owned(),
            conversation_id: "C444".to_owned(),
            kind: ConversationPolicyKind::Channel,
            receive: Some(ReceiveMode::AllMessages),
            description: None,
            thread_ts: None,
        },
        ConversationPolicy {
            alias: "fixed".to_owned(),
            conversation_id: "C555".to_owned(),
            kind: ConversationPolicyKind::Channel,
            receive: Some(ReceiveMode::AllMessages),
            description: None,
            thread_ts: Some("5.0".to_owned()),
        },
    ] {
        config.conversations.insert(policy.alias.clone(), policy);
    }
    apply_test_config(&ext, config);
    register_agent(&ext, "agent-a");

    let mut private = slack_message("G222", Some("group"), "private");
    private.event_type = "message".to_owned();
    ext.process_slack_message(private);
    assert_eq!(
        recv_prompt_request(&rx)
            .draft
            .conversation
            .expect("conversation")
            .display_name
            .as_deref(),
        Some("private")
    );

    let mut parent = slack_message("C444", Some("channel"), "thread");
    parent.event_type = "message".to_owned();
    parent.thread_ts = Some("4.0".to_owned());
    ext.process_slack_message(parent);
    assert_eq!(
        recv_prompt_request(&rx)
            .draft
            .conversation
            .expect("conversation")
            .thread
            .expect("thread")
            .stable_id,
        "4.0"
    );

    for mut rejected in [
        slack_message("G222", Some("mpim"), "wrong kind"),
        slack_message("C555", Some("channel"), "wrong fixed sibling"),
        slack_message("C123", Some("channel"), "missing type"),
    ] {
        rejected.event_type = "message".to_owned();
        if rejected.channel_id == "C555" {
            rejected.thread_ts = Some("6.0".to_owned());
        }
        if rejected.channel_id == "C123" {
            rejected.channel_type = None;
        }
        ext.process_slack_message(rejected);
    }
    assert!(rx.try_recv().is_err());
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

/// A fixed-thread root edit normalizes omitted Slack thread metadata while
/// child edits with absent or conflicting roots remain rejected.
#[test]
fn fixed_thread_root_edit_normalizes_omitted_thread() {
    let (ext, rx, _client) = extension();
    apply_test_config(
        &ext,
        fixed_thread_cfg(ConversationPolicyKind::Channel, "C777"),
    );
    register_agent(&ext, "agent-a");
    let mut root = slack_message("C777", Some("channel"), "<@UBOT123> original");
    root.ts = Some("7.0".to_owned());
    ext.process_slack_message(root);
    let create = recv_prompt_request(&rx);
    activate_prompt_origin(&ext, &create);

    ext.process_slack_edit(slack_edit("edit-root", "C777", "7.0", None, "changed"));
    let edit = recv_prompt_request(&rx);
    assert_eq!(
        edit.draft
            .conversation
            .expect("conversation")
            .thread
            .expect("thread")
            .stable_id,
        "7.0"
    );
    ext.process_slack_edit(slack_edit("wrong-child", "C777", "8.0", None, "bad"));
    ext.process_slack_edit(slack_edit("wrong-root", "C777", "7.0", Some("8.0"), "bad"));
    assert!(rx.try_recv().is_err());
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

/// Valid `to` and ordinary mentioned text reach durable dedup in both wrapper
/// orders with identical drafts rather than becoming local command side
/// effects.
#[test]
fn dual_wrapper_routed_creates_are_identical() {
    for text in ["<@UBOT123> to agent-a hello", "<@UBOT123> hello"] {
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
            let (message, mention) = dual_slack_messages(text);
            if message_first {
                ext.process_slack_message(message);
                ext.process_slack_message(mention);
            } else {
                ext.process_slack_message(mention);
                ext.process_slack_message(message);
            }
            let first = recv_prompt_request(&rx);
            let second = recv_prompt_request(&rx);
            assert_eq!(first.draft, second.draft);
            assert!(client.sent_pairs().is_empty());
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

/// Slack retries and reconnect replays are resubmitted with the same durable
/// dedup identity so the harness decides idempotence.
#[test]
fn duplicate_slack_event_ids_are_resubmitted_for_durable_dedup() {
    let (ext, rx, _client) = extension();
    register_agent(&ext, "agent-a");
    let msg = slack_message("C123", None, "<@UBOT123> hello");
    ext.process_slack_message(msg.clone());
    ext.process_slack_message(msg);
    assert_eq!(recv_prompt(&rx), "hello");
    assert_eq!(recv_prompt(&rx), "hello");
}

/// A restart with an alias rename resubmits the same native dedup key but a
/// conflicting display draft; harness rejection installs no new reply route.
#[test]
fn alias_rename_retry_preserves_native_key_and_rejects_route_install() {
    let (first_ext, first_rx, _client) = extension();
    register_agent(&first_ext, "agent-a");
    let message = slack_message("C123", Some("channel"), "<@UBOT123> hello");
    first_ext.process_slack_message(message.clone());
    let first = recv_prompt_request(&first_rx);
    activate_prompt_origin(&first_ext, &first);

    let (second_ext, second_rx, _client) = extension();
    let mut renamed = cfg();
    let mut policy = renamed.conversations.remove("team").expect("team");
    policy.alias = "renamed".to_owned();
    renamed.conversations.insert(policy.alias.clone(), policy);
    reindex_receive_routes(&mut renamed);
    apply_test_config(&second_ext, renamed);
    register_agent(&second_ext, "agent-a");
    second_ext.process_slack_message(message);
    let second = recv_prompt_request(&second_rx);
    assert_eq!(
        first
            .draft
            .external_identity
            .as_ref()
            .and_then(|id| id.dedup_key.as_ref()),
        second
            .draft
            .external_identity
            .as_ref()
            .and_then(|id| id.dedup_key.as_ref())
    );
    assert_ne!(first.draft, second.draft);
    apply_output_message(
        &HarnessOutputMessage::TransportMessageIngressResult(
            tau_proto::TransportMessageIngressResult {
                request_id: second.request_id,
                message_id: None,
                outcome: None,
                error: Some("dedup_conflict".to_owned()),
            },
        ),
        &second_ext,
    );
    let state = second_ext.state.lock().expect("state");
    assert!(state.reply_routes.is_empty());
    assert!(state.incoming_messages.is_empty());
}

/// `to` is transport ingress rather than a local command side effect, so Slack
/// retries reach durable harness dedup with identical native identity.
#[test]
fn duplicate_to_command_is_resubmitted_for_durable_dedup() {
    let (ext, rx, _client) = extension();
    register_agent(&ext, "agent-a");
    let message = slack_message("C123", None, "<@UBOT123> to agent-a hello");
    ext.process_slack_message(message.clone());
    ext.process_slack_message(message);
    let first = recv_prompt_request(&rx);
    let second = recv_prompt_request(&rx);
    assert_eq!(
        first.draft.external_identity,
        second.draft.external_identity
    );
    assert_eq!(ingress_text(&first), "hello");
    assert_eq!(ingress_text(&second), "hello");
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

/// Slack app/bot tokens and Socket Mode URLs are redacted or rejected in
/// diagnostics that may become log-visible or model-visible tool errors.
#[test]
fn token_and_socket_diagnostics_are_sanitized() {
    let cfg = cfg();
    let text = sanitize_diagnostic(
        "xapp-test xoxb-test wss://wss-primary.slack.com/link?ticket=secret",
        &cfg,
    );
    assert!(!text.contains("xapp-test"));
    assert!(!text.contains("xoxb-test"));
    let socket_url = format!(
        "wss://wss-primary.slack.com/link?ticket={}",
        "secret-ticket".repeat(80)
    );
    let socket_error = sanitize_socket_diagnostic(
        &format!("connect failed for {socket_url}"),
        &cfg,
        &socket_url,
    );
    assert!(!socket_error.contains("wss://wss-primary.slack.com"));
    assert!(!socket_error.contains("secret-ticket"));
    assert!(socket_error.len() <= MAX_DIAGNOSTIC_BYTES + 3);
    assert!(validate_socket_url("ws://example.com/socket?ticket=secret").is_err());
    assert!(validate_socket_url("ws://127.0.0.1:9000/socket?ticket=secret").is_ok());
}

/// A users.info outage rejects every occurrence, reports only once per
/// consecutive failure episode, bounds/redacts the notice, and reports again
/// after a successful verification establishes recovery.
#[test]
fn identity_failure_notice_is_bounded_redacted_and_resets_after_recovery() {
    let secret_error = format!("users.info xoxb-test {}", "detail".repeat(200));
    let client = Arc::new(IdentitySequenceClient {
        results: Mutex::new(
            [
                Err(secret_error.clone()),
                Err(secret_error.clone()),
                Ok(true),
                Err(secret_error),
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
            HarnessInputMessage::Emit(emit) => match *emit.event {
                Event::HarnessNotice(notice) => Some(notice),
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(notices.len(), 2);
    for notice in notices {
        assert!(!notice.always_show);
        assert_eq!(notice.level, NoticeLevel::Warning);
        assert_eq!(notice.kind, tau_proto::notice_kind::EXTENSION_NOTICE);
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
            "event": {
                "type": "app_mention", "channel": "C123", "user": "U123",
                "text": "<@UBOT123> must-not-route", "ts": "2.0"
            }
        }
    })
    .to_string();
    let action = handle_socket_text(&ext, &routed_text);
    assert!(
        complete_socket_action(&ext, action, Err("forced ACK write failure".to_owned())).is_err()
    );
    assert_no_ingress(&rx);
}

/// A users.info failure at the real create ingress boundary emits no transport
/// request, while the next verified occurrence recovers and routes normally.
#[test]
fn message_identity_failure_is_fail_closed_then_recovers() {
    let client = Arc::new(IdentitySequenceClient {
        results: Mutex::new(
            [Err("users.info unavailable".to_owned()), Ok(true)]
                .into_iter()
                .collect(),
        ),
    });
    let (tx, rx) = mpsc::channel();
    let ext = Extension::new(client, tx);
    {
        let mut state = ext.state.lock().expect("lock state");
        state.config = Some(cfg());
        state.registered_agents.insert(agent_id("agent-a"));
        state.bot_user_id = Some("UBOT123".to_owned());
        state.capability_active = true;
    }
    ext.process_slack_message(slack_message("C123", None, "<@UBOT123> first"));
    assert_no_ingress(&rx);
    ext.process_slack_message(slack_message("C123", None, "<@UBOT123> recovered"));
    let ingress = (0..4)
        .find_map(
            |_| match rx.recv_timeout(Duration::from_millis(100)).ok()? {
                HarnessInputMessage::TransportMessageIngress(request) => Some(request),
                _ => None,
            },
        )
        .expect("recovered occurrence should emit typed ingress");
    let MessageOperation::Create {
        payload: MessagePayload::Text { text, .. },
    } = &ingress.draft.operation
    else {
        panic!("expected create ingress");
    };
    assert_eq!(text, "recovered");
}
