//! Test suite for the harness. Split by concern around major harness behaviors
//! such as interception, replay, dispatch, lifecycle, and tool handling.
//!
//! The shared helpers and imports live here so each submodule can
//! pull them in with `use super::*;`.

use std::io::{BufReader, BufWriter, Read, Write};
use std::os::unix as path_std_os_unix;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command as path_std_process_Command;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use std::{collections as path_std_collections, fs as path_std_fs, thread};

use tau_config::settings as path_tau_config_settings;
use tau_core::{
    AgentEntry, AgentStore, AgentTree, Connection, ConnectionOrigin, ConnectionSendError,
    ConnectionSink, PendingConnectionMetadata, RoutedFrame,
};
use tau_proto::{
    AgentPromptCreated, AgentPromptId, AgentPromptQueued, AgentPromptRecalled, AgentPromptSteered,
    CborValue, ContentPart, ContextItem, ContextRole, Disconnect, Event, EventDelivery,
    EventSelector, HarnessInputMessage, HarnessInputWriter, HarnessOutputMessage,
    HarnessOutputReader, Intercept, InterceptAction, InterceptReply, InterceptionPriority,
    MessageItem, NodeId, ProviderResponseFinished, ProviderResponseUpdated,
    ResponsesToolCallEnvelope, StartAgentRequest, Subscribe, ToolCallId, ToolCallItem, ToolName,
    ToolResult, ToolResultItem, ToolResultStatus, ToolSpec, UiPromptDraft, UiPromptSubmitted,
};
use tau_session_inspect::open_session_store;
use tempfile::TempDir;

use super::{AgentToolCall, HARNESS_CONNECTION_ID, Harness, NormalizedFinishedToolCall};
use crate::agent::{AgentTurnState, PendingPrompt, PendingPromptSource};
use crate::daemon::{
    ServeOptions, bind_listener, get_daemon_rendered_system_prompt,
    get_daemon_rendered_tool_definitions, run_daemon_with_echo, run_embedded_message_with_echo,
    send_daemon_message, send_daemon_message_with_trace,
};
use crate::discovery::{DiscoveredAgentsFile, DiscoveredSkill, DiscoveredSkillSource};
use crate::error::HarnessError;
use crate::event::HarnessEvent;
use crate::harness::SessionGeneration;
use crate::model::{
    InterSessionReceiverRole, clamp_effort, efforts_for_model, load_roles, role_infos,
    select_model_for_role, selected_params_for_role, thinking_summaries_for_model,
    verbosities_for_model,
};
use crate::pending_agent_discovery::PendingAgentDiscovery;
use crate::tool_turn::ToolTurnCategories;
use crate::turn::{PromptSubmission, TurnState};
use crate::{AgentId, event_log as path_crate_event_log, extension as path_crate_extension};

#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
enum TestProtocolItem {
    Event(Event),
    Message(TestMessage),
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
enum TestMessage {
    Hello(tau_proto::Hello),
    Subscribe(Subscribe),
    Intercept(Intercept),
    Ready(tau_proto::Ready),
    Disconnect(Disconnect),
    ConfigError(tau_proto::ConfigError),
    Emit(tau_proto::Emit),
    InterceptReply(InterceptReply),
    GetCurrentSession(tau_proto::GetCurrentSession),
    GetSessionAgentList(tau_proto::GetSessionAgentList),
    Configure(tau_proto::Configure),
    UiSessionAccepted(tau_proto::UiSessionAccepted),
    InterceptRequest(tau_proto::InterceptRequest),
    LiveDelivery(EventDelivery),
    AgentPromptCreatedResult(Box<tau_proto::AgentPromptCreatedResult>),
    RenderedSystemPromptResult(Box<tau_proto::RenderedSystemPromptResult>),
    RenderedPromptResult(Box<tau_proto::RenderedPromptResult>),
    RenderedToolDefinitionsResult(Box<tau_proto::RenderedToolDefinitionsResult>),
    CurrentSessionResult(tau_proto::CurrentSessionResult),
    SessionAgentListResult(Box<tau_proto::SessionAgentListResult>),
    ExtensionDataResult(Box<tau_proto::ExtensionDataResult>),
    ExternalAgentMessageResult(tau_proto::ExternalAgentMessageResult),
    ExternalAgentMessageAuthResult(tau_proto::ExternalAgentMessageAuthResult),
    PeerSessionProbeResult(tau_proto::PeerSessionProbeResult),
}

/// Configure deterministic receiver authority directly for focused harness
/// tests that do not load user role configuration.
fn configure_inter_session_receivers(harness: &mut Harness, receivers: &[(&str, bool)]) {
    harness.config.inter_session_receivers = receivers
        .iter()
        .map(|(role, auto_start)| InterSessionReceiverRole {
            role: (*role).to_owned(),
            auto_start: *auto_start,
        })
        .collect();
}

impl TestProtocolItem {
    fn into_input_message(self) -> HarnessInputMessage {
        match self {
            Self::Event(event) => HarnessInputMessage::emit(event),
            Self::Message(message) => message.into_input_message(),
        }
    }

    fn from_output_message(message: HarnessOutputMessage) -> Self {
        match message {
            HarnessOutputMessage::Configure(message) => {
                Self::Message(TestMessage::Configure(message))
            }
            HarnessOutputMessage::UiSessionAccepted(message) => {
                Self::Message(TestMessage::UiSessionAccepted(message))
            }
            HarnessOutputMessage::Disconnect(message) => {
                Self::Message(TestMessage::Disconnect(message))
            }
            HarnessOutputMessage::Deliver(delivery) => {
                if !delivery.replay && delivery.recorded_at.is_some() {
                    Self::Message(TestMessage::LiveDelivery(delivery))
                } else {
                    Self::Event(delivery.into_event())
                }
            }
            HarnessOutputMessage::InterceptRequest(message) => {
                Self::Message(TestMessage::InterceptRequest(message))
            }
            HarnessOutputMessage::AgentPromptCreatedResult(message) => {
                Self::Message(TestMessage::AgentPromptCreatedResult(message))
            }
            HarnessOutputMessage::RenderedSystemPromptResult(message) => {
                Self::Message(TestMessage::RenderedSystemPromptResult(message))
            }
            HarnessOutputMessage::RenderedPromptResult(message) => {
                Self::Message(TestMessage::RenderedPromptResult(message))
            }
            HarnessOutputMessage::RenderedToolDefinitionsResult(message) => {
                Self::Message(TestMessage::RenderedToolDefinitionsResult(message))
            }
            HarnessOutputMessage::CurrentSessionResult(message) => {
                Self::Message(TestMessage::CurrentSessionResult(message))
            }
            HarnessOutputMessage::SessionAgentListResult(message) => {
                Self::Message(TestMessage::SessionAgentListResult(message))
            }
            HarnessOutputMessage::ExtensionDataResult(message) => {
                Self::Message(TestMessage::ExtensionDataResult(message))
            }
            HarnessOutputMessage::ExternalAgentMessageResult(message) => {
                Self::Message(TestMessage::ExternalAgentMessageResult(message))
            }
            HarnessOutputMessage::ExternalAgentMessageAuthResult(message) => {
                Self::Message(TestMessage::ExternalAgentMessageAuthResult(message))
            }
            HarnessOutputMessage::PeerSessionProbeResult(message) => {
                Self::Message(TestMessage::PeerSessionProbeResult(message))
            }
        }
    }

    fn into_event_frame(self) -> Self {
        match self {
            Self::Message(TestMessage::LiveDelivery(delivery)) => {
                Self::Event(delivery.into_event())
            }
            other => other,
        }
    }
}

impl From<TestMessage> for HarnessInputMessage {
    fn from(message: TestMessage) -> Self {
        message.into_input_message()
    }
}

impl TestMessage {
    fn into_input_message(self) -> HarnessInputMessage {
        match self {
            Self::Hello(message) => HarnessInputMessage::Hello(message),
            Self::Subscribe(mut message) => {
                if message.historical_selectors.is_empty() {
                    message.historical_selectors = message.live_selectors.clone();
                }
                HarnessInputMessage::Subscribe(message)
            }
            Self::Intercept(message) => HarnessInputMessage::Intercept(message),
            Self::Ready(message) => HarnessInputMessage::Ready(message),
            Self::Disconnect(message) => HarnessInputMessage::Disconnect(message),
            Self::ConfigError(message) => HarnessInputMessage::ConfigError(message),
            Self::Emit(message) => HarnessInputMessage::Emit(message),
            Self::InterceptReply(message) => HarnessInputMessage::InterceptReply(message),
            Self::GetCurrentSession(message) => HarnessInputMessage::GetCurrentSession(message),
            Self::GetSessionAgentList(message) => HarnessInputMessage::GetSessionAgentList(message),
            Self::Configure(_)
            | Self::UiSessionAccepted(_)
            | Self::InterceptRequest(_)
            | Self::LiveDelivery(_)
            | Self::AgentPromptCreatedResult(_)
            | Self::RenderedSystemPromptResult(_)
            | Self::RenderedPromptResult(_)
            | Self::RenderedToolDefinitionsResult(_)
            | Self::CurrentSessionResult(_)
            | Self::SessionAgentListResult(_)
            | Self::ExtensionDataResult(_)
            | Self::ExternalAgentMessageResult(_)
            | Self::ExternalAgentMessageAuthResult(_)
            | Self::PeerSessionProbeResult(_) => {
                panic!("test frame shim cannot send harness-output message as input")
            }
        }
    }
}

struct TestOutputReader<R> {
    inner: HarnessOutputReader<R>,
}

impl<R> TestOutputReader<R>
where
    R: Read,
{
    fn new(inner: R) -> Self {
        Self {
            inner: HarnessOutputReader::new(inner),
        }
    }

    fn read_frame(&mut self) -> Result<Option<TestProtocolItem>, tau_proto::DecodeError> {
        self.inner
            .read_message()
            .map(|message| message.map(TestProtocolItem::from_output_message))
    }
}

struct TestInputWriter<W> {
    inner: HarnessInputWriter<W>,
}

impl<W> TestInputWriter<W>
where
    W: Write,
{
    fn new(inner: W) -> Self {
        Self {
            inner: HarnessInputWriter::new(inner),
        }
    }

    fn write_frame(&mut self, frame: &TestProtocolItem) -> Result<(), tau_proto::EncodeError> {
        self.inner
            .write_message(&frame.clone().into_input_message())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

trait HarnessTestProtocolExt {
    fn handle_extension_event(
        &mut self,
        source_id: &str,
        frame: TestProtocolItem,
    ) -> Result<(), HarnessError>;

    fn handle_client_event(
        &mut self,
        client_id: &str,
        frame: TestProtocolItem,
    ) -> Result<bool, HarnessError>;
}

impl HarnessTestProtocolExt for Harness {
    fn handle_extension_event(
        &mut self,
        source_id: &str,
        frame: TestProtocolItem,
    ) -> Result<(), HarnessError> {
        self.handle_extension_message(
            &crate::test_connection_id(source_id),
            frame.into_input_message(),
        )
    }

    fn handle_client_event(
        &mut self,
        client_id: &str,
        frame: TestProtocolItem,
    ) -> Result<bool, HarnessError> {
        self.handle_client_message(
            &crate::test_connection_id(client_id),
            frame.into_input_message(),
        )
    }
}

fn echo_runner(r: UnixStream, w: UnixStream) -> Result<(), String> {
    crate::harness::run_echo_provider(r, w).map_err(|e| e.to_string())
}

fn assert_agent_id_chars(agent_id: &str) {
    assert!(!agent_id.is_empty());
    assert!(agent_id.len() <= tau_proto::AGENT_ID_MAX_LEN);
    assert!(
        agent_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    );
}

fn assert_role_hex_agent_id(agent_id: &str, _role: &str) {
    assert_agent_id_chars(agent_id);
}

fn test_discovered_skill(
    source_id: &str,
    description: &str,
    modified_secs: u64,
) -> DiscoveredSkill {
    DiscoveredSkill {
        source_id: crate::test_connection_id(source_id),
        description: description.to_owned(),
        source: DiscoveredSkillSource::File(PathBuf::from(format!("/tmp/{description}.md"))),
        add_to_prompt: false,
        user_invocable: true,
        disable_model_invocation: false,
        argument_hint: None,
        modified: Some(std::time::UNIX_EPOCH + Duration::from_secs(modified_secs)),
    }
}

fn set_test_agent_context_wait(
    h: &mut Harness,
    agent_id: tau_proto::AgentId,
    waiting_on: std::collections::HashSet<tau_proto::ConnectionId>,
) {
    h.prompt_coordination
        .context_discovery
        .frozen_agents
        .remove(&agent_id);
    h.prompt_coordination
        .context_discovery
        .pending_agents
        .insert(
            agent_id,
            PendingAgentDiscovery {
                initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                    .expect("test identifier must be valid"),
                skill_candidates: h
                    .prompt_coordination
                    .context_discovery
                    .skill_candidates
                    .clone(),
                skills: h.prompt_coordination.context_discovery.skills.clone(),
                agents_files: h.prompt_coordination.context_discovery.agents_files.clone(),
                waiting_on,
            },
        );
}

fn test_agent_context_waits<'a>(
    h: &'a Harness,
    agent_id: &tau_proto::AgentId,
) -> Option<&'a std::collections::HashSet<tau_proto::ConnectionId>> {
    h.prompt_coordination
        .context_discovery
        .pending_agents
        .get(agent_id)
        .map(|pending| &pending.waiting_on)
}

fn finish_test_agent_context_wait(h: &mut Harness, agent_id: &tau_proto::AgentId) {
    let Some(mut pending) = h
        .prompt_coordination
        .context_discovery
        .pending_agents
        .remove(agent_id)
    else {
        return;
    };
    pending.waiting_on.clear();
    h.prompt_coordination
        .context_discovery
        .frozen_agents
        .insert(
            agent_id.clone(),
            crate::frozen_agent_discovery::FrozenAgentDiscovery {
                initialization_id: pending.initialization_id,
                skills: pending.skills,
            },
        );
}

/// Ensures build timestamps used for built-in skill freshness parse to exact
/// instants and reject malformed inputs before collision comparison.
#[test]
fn build_last_modified_parser_validates_packaged_format() {
    assert_eq!(
        super::parse_build_last_modified("1970-01-01 00:00"),
        Some(std::time::UNIX_EPOCH)
    );
    assert_eq!(
        super::parse_build_last_modified("2024-06-12 09:30"),
        Some(std::time::UNIX_EPOCH + Duration::from_secs(1_718_184_600))
    );
    assert!(super::parse_build_last_modified("2024/06/12 09:30").is_none());
    assert!(super::parse_build_last_modified("2024-1x-12 09:30").is_none());
    assert!(super::parse_build_last_modified("2024-06-aa 09:30").is_none());
    assert!(super::parse_build_last_modified("2024-13-12 09:30").is_none());
    assert!(super::parse_build_last_modified("2024-06-12 24:00").is_none());
}

/// Ensures skill candidate selection chooses the newest timestamp and keeps the
/// earlier candidate when timestamps tie.
#[test]
fn selected_skill_candidate_prefers_newest_with_stable_tie_break() {
    let first = test_discovered_skill("first", "first", 100);
    let newer = test_discovered_skill("newer", "newer", 200);
    let same_as_first = test_discovered_skill("same", "same", 100);

    let candidates = [first.clone(), newer];
    let selected = super::selected_skill_candidate(&candidates).expect("selected newest");
    assert_eq!(selected.description, "newer");

    let candidates = [first, same_as_first];
    let selected = super::selected_skill_candidate(&candidates).expect("selected tie");
    assert_eq!(selected.description, "first");
}

/// Ensures the harness keeps fallback candidates so disconnecting the newest
/// skill provider restores the next-best skill instead of losing the name.
#[test]
fn skill_winner_disconnect_restores_next_best_candidate() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let name = tau_proto::SkillName::new("same-skill");
    let older = test_discovered_skill("old-ext", "older", 100);
    let newer = test_discovered_skill("new-ext", "newer", 200);

    h.prompt_coordination
        .context_discovery
        .skill_candidates
        .insert(name.clone(), vec![older, newer]);
    h.recompute_discovered_skill_winner(&name);
    assert_eq!(
        h.prompt_coordination.context_discovery.skills[&name].description,
        "newer"
    );

    h.remove_discovered_context(&crate::test_connection_id("new-ext"));
    assert_eq!(
        h.prompt_coordination.context_discovery.skills[&name].description,
        "older"
    );

    h.shutdown().expect("shutdown");
}

/// Ensures an escaped literal targeting an existing agent retains canonical
/// `:skill` text without invoking the harness-owned skill command.
#[test]
fn literal_existing_agent_skill_text_bypasses_skill_expansion() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    h.config.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);

    h.handle_authenticated_ui_prompt_submitted(
        crate::harness::harness_connection_id(),
        UiPromptSubmitted {
            literal: true,
            session_id: "s1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            text: ":skill definitely-not-installed".to_owned(),
            agent_id,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: Some("literal-existing".to_owned()),
        },
    )
    .expect("submit literal skill text");

    let submitted = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentPromptSubmitted(submitted)
                if submitted.ctx_id.as_deref() == Some("literal-existing") =>
            {
                Some(submitted)
            }
            _ => None,
        })
        .expect("literal prompt submitted");
    assert_eq!(submitted.text, ":skill definitely-not-installed");
    assert_literal_provider_projection(&h, &submitted.agent_id, ":skill definitely-not-installed");
}

/// Ensures UI prompt classification transfers, rather than clones, the
/// expanded text buffer while preserving every constructor's prompt metadata.
#[test]
fn authenticated_ui_pending_prompt_classification_moves_text_buffer() {
    let cases = [
        (
            tau_proto::PromptMessageClass::Internal,
            false,
            tau_proto::PromptMessageClass::Internal,
            PendingPromptSource::General,
            tau_proto::PromptSubmissionSource::HarnessInternal,
        ),
        (
            tau_proto::PromptMessageClass::User,
            true,
            tau_proto::PromptMessageClass::User,
            PendingPromptSource::WatchNotifiedUser,
            tau_proto::PromptSubmissionSource::HumanUi,
        ),
        (
            tau_proto::PromptMessageClass::User,
            false,
            tau_proto::PromptMessageClass::User,
            PendingPromptSource::General,
            tau_proto::PromptSubmissionSource::HumanUi,
        ),
    ];

    for (
        index,
        (
            message_class,
            is_user_interaction,
            expected_class,
            expected_source,
            expected_submission_source,
        ),
    ) in cases.into_iter().enumerate()
    {
        let text = format!("prompt-{index}-{}", "x".repeat(1024));
        let text_ptr = text.as_ptr();
        let pending = Harness::pending_authenticated_ui_prompt(
            text,
            message_class,
            is_user_interaction,
            Some(format!("ctx-{index}")),
        );

        assert_eq!(
            pending.text.as_ptr(),
            text_ptr,
            "selected constructor must retain the owned text allocation"
        );
        assert_eq!(pending.text, format!("prompt-{index}-{}", "x".repeat(1024)));
        assert_eq!(pending.message_class, expected_class);
        assert_eq!(pending.source, expected_source);
        assert_eq!(pending.submission_source, expected_submission_source);
        assert_eq!(
            pending.ctx_id.as_deref(),
            Some(format!("ctx-{index}").as_str())
        );
    }
}

/// Ensures an escaped literal used as a new agent's first prompt retains
/// canonical `:skill` text without invoking the harness-owned skill command.
#[test]
fn literal_new_agent_skill_text_bypasses_skill_expansion() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    h.config.selected_model = Some("test/model".into());

    h.handle_ui_create_agent_from(
        &crate::test_connection_id("ui-create-test"),
        tau_proto::UiCreateAgent {
            request_id: "test-create-request".to_owned(),
            literal: true,
            parent_agent: None,
            session_id: "s1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            role: h.config.selected_role.clone(),
            model_override: None,
            metadata: Vec::new(),
            initial_prompt: Some(":skill definitely-not-installed".to_owned()),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: Some("literal-new".to_owned()),
            ephemeral: false,
        },
    )
    .expect("create agent with literal skill text");

    let submitted = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentPromptSubmitted(submitted)
                if submitted.ctx_id.as_deref() == Some("literal-new") =>
            {
                Some(submitted)
            }
            _ => None,
        })
        .expect("literal initial prompt submitted");
    assert_eq!(submitted.text, ":skill definitely-not-installed");
    assert_literal_provider_projection(&h, &submitted.agent_id, ":skill definitely-not-installed");
}

fn assert_literal_provider_projection(h: &Harness, agent_id: &AgentId, text: &str) {
    let provider_prompt = event_log_events(h)
        .into_iter()
        .rev()
        .find_map(|event| match event {
            Event::AgentPromptCreated(prompt) if &prompt.agent_id == agent_id => Some(prompt),
            _ => None,
        })
        .expect("provider prompt");
    assert_eq!(
        provider_prompt.context.flatten(),
        vec![ContextItem::Message(MessageItem {
            role: ContextRole::User,
            content: vec![ContentPart::Text {
                text: format!("<user>{text}</user>"),
            }],
            phase: None,
            responses_raw_json: None,
        })]
    );
}

#[test]
fn extension_data_paths_reject_escape_components() {
    assert!(super::sanitize_extension_data_path("notes/file.txt", false).is_ok());
    assert!(super::sanitize_extension_data_path("", true).is_ok());
    assert_eq!(
        super::sanitize_extension_data_path("", false)
            .expect_err("empty file path")
            .kind,
        tau_proto::ExtensionDataErrorKind::InvalidPath
    );
    assert_eq!(
        super::sanitize_extension_data_path("../secret", false)
            .expect_err("parent escape")
            .kind,
        tau_proto::ExtensionDataErrorKind::InvalidPath
    );
    assert!(super::sanitize_extension_data_path("notes/../secret", false).is_err());
    assert!(super::sanitize_extension_data_path("/tmp/secret", false).is_err());
    assert!(super::sanitize_extension_data_path("./secret", false).is_err());
}

#[test]
fn extension_data_list_skips_symlinks_and_returns_relative_entries() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().join("root");
    std::fs::create_dir_all(root.join("nested")).expect("mkdir");
    std::fs::write(root.join("file.txt"), b"abc").expect("write file");
    #[cfg(unix)]
    path_std_os_unix::fs::symlink("/tmp", root.join("outside")).expect("symlink");

    let entries = super::list_extension_data_entries(&root, &root).expect("list entries");
    assert!(
        entries.iter().any(|entry| entry.path.as_str() == "file.txt"
            && !entry.is_dir
            && entry.len == Some(3))
    );
    assert!(
        entries
            .iter()
            .any(|entry| entry.path.as_str() == "nested" && entry.is_dir)
    );
    assert!(!entries.iter().any(|entry| entry.path.as_str() == "outside"));
}

#[test]
fn extension_data_checked_path_rejects_symlink_leaf_and_ancestor() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().join("root");
    std::fs::create_dir_all(&root).expect("mkdir root");
    #[cfg(unix)]
    {
        path_std_os_unix::fs::symlink("/tmp", root.join("leaf")).expect("leaf symlink");
        path_std_os_unix::fs::symlink("/tmp", root.join("parent")).expect("parent symlink");
        assert!(super::checked_extension_data_path(&root, Path::new("leaf"), false).is_err());
        assert!(super::checked_extension_data_path(&root, Path::new("parent/file"), true).is_err());
    }
}
#[test]
fn extension_data_checked_path_rejects_symlink_root() {
    let tmp = TempDir::new().expect("tempdir");
    let real = tmp.path().join("real");
    let root = tmp.path().join("root");
    std::fs::create_dir_all(&real).expect("mkdir real");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&real, path_std_fs::Permissions::from_mode(0o755))
            .expect("chmod real");
        path_std_os_unix::fs::symlink(&real, &root).expect("root symlink");
        assert!(super::checked_extension_data_path(&root, Path::new("file"), true).is_err());
        assert!(super::checked_extension_data_path(&root, Path::new(""), true).is_err());
        let real_mode = std::fs::metadata(&real)
            .expect("real metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(real_mode, 0o755);
    }
}
#[test]
fn extension_data_file_helpers_create_append_replace_delete_private_files() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().join("root");
    let file = root.join("nested/file.txt");

    super::create_extension_data_file(&file, b"first").expect("create file");
    assert_eq!(std::fs::read(&file).expect("read created"), b"first");
    let duplicate = super::create_extension_data_file(&file, b"second").expect_err("duplicate");
    assert_eq!(duplicate.kind(), std::io::ErrorKind::AlreadyExists);

    super::append_extension_data_file(&file, b"\nappended").expect("append file");
    assert_eq!(
        std::fs::read(&file).expect("read appended"),
        b"first\nappended"
    );

    super::atomic_replace_extension_data_file(&file, b"replaced").expect("replace file");
    assert_eq!(std::fs::read(&file).expect("read replaced"), b"replaced");
    let renamed = root.join("nested/renamed.txt");
    super::rename_extension_data_file(&file, &renamed).expect("rename file");
    assert!(!file.exists());
    assert_eq!(std::fs::read(&renamed).expect("read renamed"), b"replaced");

    super::delete_extension_data_file(&renamed).expect("delete file");
    assert!(!renamed.exists());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let dir_mode = std::fs::metadata(root.join("nested"))
            .expect("nested metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
        super::create_extension_data_file(&file, b"private").expect("recreate file");
        let file_mode = std::fs::metadata(&file)
            .expect("file metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600);
    }
}
#[test]
fn minted_agent_ids_use_default_random_alphanumeric_template() {
    let agent_id = super::mint_agent_id_for_role("engineer");

    assert_eq!(agent_id.len(), 6);
    assert_agent_id_chars(&agent_id);
}

#[test]
fn minted_agent_ids_use_deterministic_test_rng_sequence() {
    // Harness tests install a fixed RNG seed. The sequence should be stable
    // across harnesses while still advancing between agent creations.
    let mint_pair = || {
        let tmp = TempDir::new().expect("tempdir");
        let mut h = echo_harness(tmp.path()).expect("harness");
        let role = h.config.selected_role.clone();
        let first = h.create_durable_user_agent(
            "s1".parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            &role,
        );
        let second = h.create_durable_user_agent(
            "s1".parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            &role,
        );
        (first.to_string(), second.to_string())
    };

    let first_run = mint_pair();
    let second_run = mint_pair();

    assert_eq!(first_run, second_run);
    assert_ne!(first_run.0, first_run.1);
}

#[test]
fn minting_agent_ids_renders_configured_template() {
    let mut warnings = Vec::new();
    let agent_id = super::mint_available_agent_id_for_role_with(
        "engineer",
        "engineer",
        "{{role}}-{{random_alphanumeric 6}}",
        |_| false,
        &mut super::deterministic_agent_id_rng(),
        |kind, warning| warnings.push((kind, warning)),
    );

    assert!(agent_id.starts_with("engineer-"));
    assert_eq!(agent_id.len(), "engineer-".len() + 6);
    assert_agent_id_chars(&agent_id);
    assert!(warnings.is_empty());
}

#[test]
fn minting_agent_ids_renders_role_group_in_configured_template() {
    // Agent ID templates can include the navigation role group so related
    // roles share an ID prefix while still retaining the exact role name.
    let mut warnings = Vec::new();
    let agent_id = super::mint_available_agent_id_for_role_with(
        "engineer-senior",
        "engineer",
        "{{role_group}}-{{role}}-{{random_alphanumeric 4}}",
        |_| false,
        &mut super::deterministic_agent_id_rng(),
        |kind, warning| warnings.push((kind, warning)),
    );

    assert!(agent_id.starts_with("engineer-engineer-senior-"));
    assert_eq!(agent_id.len(), "engineer-engineer-senior-".len() + 4);
    assert_agent_id_chars(&agent_id);
    assert!(warnings.is_empty());
}

#[test]
fn minting_agent_ids_reject_display_name_only_template_fields() {
    // Agent ID templates must stay strict and only expose ID-safe context.
    // Display-name-only fields would otherwise silently render as empty strings.
    let mut warnings = Vec::new();
    let agent_id = super::mint_available_agent_id_for_role_with(
        "engineer",
        "engineer",
        "{{role}}-{{task_name}}",
        |_| false,
        &mut super::deterministic_agent_id_rng(),
        |kind, warning| warnings.push((kind, warning)),
    );

    assert_eq!(agent_id.len(), 6);
    assert_agent_id_chars(&agent_id);
    assert!(matches!(
        warnings.as_slice(),
        [(
            super::AgentIdTemplateKind::Configured,
            super::AgentIdMintWarning::RenderFailed { .. }
        )]
    ));
}

/// For newly created agents, the real built-in template must render no name
/// when the request supplies no explicit task.
#[test]
fn built_in_agent_template_omits_display_name_when_task_name_is_absent() {
    let mut rng = super::deterministic_agent_id_rng();
    let template = path_tau_config_settings::HarnessSettings::built_in()
        .agent_display_name_template
        .expect("built-in display-name template");
    let rendered = super::render_agent_template(
        &template,
        "engineer-senior",
        "engineer",
        "engineer-Ab12",
        None,
        0,
        &mut rng,
    )
    .expect("render");

    assert!(rendered.is_empty());
}

#[test]
fn agent_template_renders_display_name_context() {
    let mut rng = super::deterministic_agent_id_rng();
    let rendered = super::render_agent_template(
        "{{role_group}}/{{role}}/{{agent_id}}/{{task_name}}/{{task_name_present}}/{{random_alphanumeric 4}}",
        "engineer-senior",
        "engineer",
        "engineer-Ab12",
        Some("review fix"),
        0,
        &mut rng,
    )
    .expect("render");

    assert!(rendered.starts_with("engineer/engineer-senior/engineer-Ab12/review fix/true/"));
    assert_eq!(
        rendered.len(),
        "engineer/engineer-senior/engineer-Ab12/review fix/true/".len() + 4
    );
}

#[test]
fn minting_agent_ids_falls_back_immediately_on_invalid_rendered_id() {
    // Invalid configured output must not be retried; it falls back to the safe
    // default template and reports a warning the harness can surface to users.
    let mut warnings = Vec::new();
    let agent_id = super::mint_available_agent_id_for_role_with(
        "engineer",
        "engineer",
        "bad/id",
        |_| false,
        &mut super::deterministic_agent_id_rng(),
        |kind, warning| warnings.push((kind, warning)),
    );

    assert_eq!(agent_id.len(), 6);
    assert_agent_id_chars(&agent_id);
    assert!(matches!(
        warnings.as_slice(),
        [(
            super::AgentIdTemplateKind::Configured,
            super::AgentIdMintWarning::InvalidRendered { .. }
        )]
    ));
}

#[test]
fn minting_agent_ids_falls_back_after_configured_template_collisions() {
    // A configured template that keeps producing a reserved id should not loop
    // forever. After the configured attempt budget, minting falls back to the
    // default random template.
    let mut warnings = Vec::new();
    let agent_id = super::mint_available_agent_id_for_role_with(
        "engineer",
        "engineer",
        "taken",
        |agent_id| agent_id == "taken",
        &mut super::deterministic_agent_id_rng(),
        |kind, warning| warnings.push((kind, warning)),
    );

    assert_ne!(agent_id, "taken");
    assert_agent_id_chars(&agent_id);
    assert!(warnings.iter().any(|(kind, warning)| matches!(
        (kind, warning),
        (
            super::AgentIdTemplateKind::Configured,
            super::AgentIdMintWarning::CollisionsExceeded { attempts }
        ) if *attempts == super::AGENT_ID_TEMPLATE_COLLISION_ATTEMPTS
    )));
}

#[test]
fn minting_agent_ids_skips_persisted_agent_dirs() {
    // A rendered id already present on disk must stay reserved even when the
    // lazy store has not loaded that agent tree into memory yet.
    let td = TempDir::new().expect("tempdir");
    let agents_dir = td.path().join("agents");
    let store = AgentStore::open_lazy(agents_dir.clone()).expect("agent store");
    let reserved_dir = agents_dir.join("engineer_0");
    std::fs::create_dir_all(&reserved_dir).expect("agent dir");
    std::fs::write(reserved_dir.join("meta.json"), "{}").expect("agent meta");

    let mut warnings = Vec::new();
    let agent_id = super::mint_available_agent_id_for_role_with(
        "engineer",
        "engineer",
        "engineer_0",
        |agent_id| store.agent_exists(agent_id),
        &mut super::deterministic_agent_id_rng(),
        |kind, warning| warnings.push((kind, warning)),
    );

    assert_ne!(agent_id, "engineer_0");
    assert_agent_id_chars(&agent_id);
    assert!(warnings.iter().any(|(kind, warning)| matches!(
        (kind, warning),
        (
            super::AgentIdTemplateKind::Configured,
            super::AgentIdMintWarning::CollisionsExceeded { .. }
        )
    )));
}

/// Bundled self-knowledge must quote the effective configuration defaults so
/// agents do not advise users based on stale templates.
#[test]
fn render_self_knowledge_config_content_inserts_config_defaults() {
    let rendered = crate::harness::render_self_knowledge_config_content();

    assert!(!rendered.contains("{harness_config}"));
    assert!(!rendered.contains("{ui_config}"));
    assert!(rendered.contains("${XDG_RUNTIME_DIR}/tau/harnesses/"));
    assert!(rendered.contains("session_retention_days: 60"));
    assert!(rendered.contains("show_thinking: true"));
    assert!(rendered.contains("{{role}}-{{random_alphanumeric 4}}"));
    assert!(rendered.contains("{{role_group}}: {{task_name}}"));
    assert!(rendered.contains("{{#if task_name_present}}{{task_name}}{{/if}}"));
}

#[test]
fn render_self_knowledge_pim_content_inserts_config_defaults() {
    let rendered = crate::harness::render_self_knowledge_pim_content();

    assert!(!rendered.contains("{pim_config}"));
    assert!(rendered.contains("std-pim:"));
    assert!(rendered.contains("calendar:"));
}

fn agent_tree_for_conversation<'a>(h: &'a Harness, cid: &AgentId) -> &'a AgentTree {
    let agent_id = h
        .agent_runtime
        .agent_registry
        .agents
        .get(cid)
        .and_then(|conv| conv.identity.agent_id.as_deref())
        .expect("conversation has agent id");
    h.session_runtime
        .agent_store
        .agent(agent_id)
        .expect("agent tree")
}

pub(super) fn ensure_test_user_agent(h: &mut Harness) -> AgentId {
    let cid = h
        .agent_runtime
        .agent_registry
        .agents
        .iter()
        .find_map(|(cid, conv)| conv.identity.originator.is_user().then_some(cid.clone()))
        .unwrap_or_else(|| {
            let session_id = h.session_runtime.current_session_id.clone();
            let role = h.config.selected_role.clone();
            h.create_durable_user_agent(session_id, &role)
        });
    // Most harness unit tests use this helper to focus on tool/provider state,
    // not extension-provided prompt context. Treat the synthetic agent as if
    // registered context providers have already acknowledged it; tests that
    // exercise context readiness drive `session.agent_loaded` explicitly.
    if let Some(agent_id) = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .and_then(|conv| conv.identity.agent_id.as_deref())
        .map(crate::parse_agent_id)
    {
        finish_test_agent_context_wait(h, &agent_id);
    }
    cid
}

/// Rejects the next durable semantic fact at the pre-publication admission cut.
fn reject_next_semantic_admission(h: &Harness) {
    h.session_runtime
        .persistence_owner
        .as_ref()
        .expect("durable test harness has one persistence owner")
        .reject_next_admission_for_test();
}

/// Rejects a bounded sequence of semantic admissions at the same canonical cut.
fn reject_semantic_admissions(h: &Harness, count: usize) {
    h.session_runtime
        .persistence_owner
        .as_ref()
        .expect("durable test harness has one persistence owner")
        .reject_admissions_for_test(count);
}

fn test_user_agent(h: &Harness) -> AgentId {
    h.agent_runtime
        .agent_registry
        .agents
        .iter()
        .find_map(|(cid, conv)| conv.identity.originator.is_user().then_some(cid.clone()))
        .expect("test should create a user agent first")
}

fn durable_agent_id_for_conversation(h: &Harness, cid: &AgentId) -> tau_proto::AgentId {
    crate::parse_agent_id(
        h.agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|conv| conv.identity.agent_id.clone())
            .expect("conversation has durable agent id"),
    )
}

fn default_agent_tree(h: &Harness) -> &AgentTree {
    let cid = test_user_agent(h);
    agent_tree_for_conversation(h, &cid)
}

fn agent_branch_for_conversation<'a>(h: &'a Harness, cid: &AgentId) -> Vec<&'a AgentEntry> {
    let head = h
        .agent_runtime
        .agent_registry
        .agents
        .get(cid)
        .and_then(|conv| conv.identity.head);
    agent_tree_for_conversation(h, cid).branch_from(head)
}

fn default_agent_branch(h: &Harness) -> Vec<&AgentEntry> {
    let cid = test_user_agent(h);
    agent_branch_for_conversation(h, &cid)
}

fn default_agent_node(h: &Harness, id: NodeId) -> &tau_core::AgentNode {
    default_agent_tree(h).node(id).expect("agent node")
}

fn event_log_events(h: &Harness) -> Vec<Event> {
    let mut events = Vec::new();
    let mut seq = path_crate_event_log::EventLogSeq::new(0);
    while let Some(entry) = h.runtime_io.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        events.push(entry.event);
    }
    events
}

fn loaded_agent_events(h: &Harness, session_id: &str) -> Vec<Event> {
    let Some(session) = h.session_runtime.store.session(session_id) else {
        return Vec::new();
    };

    session
        .loaded_agents()
        .into_iter()
        .filter_map(|agent_id| {
            h.session_runtime
                .agent_store
                .agent_events(agent_id.as_str())
                .ok()
        })
        .flatten()
        .map(|entry| entry.event)
        .collect()
}

fn persisted_agent_branch(state_dir: &Path, session_id: &str) -> Vec<AgentEntry> {
    persisted_agent_branches(state_dir, session_id)
        .into_iter()
        .next()
        .expect("loaded agent")
}

fn persisted_agent_branches(state_dir: &Path, session_id: &str) -> Vec<Vec<AgentEntry>> {
    let sessions_dir = tau_config::settings::sessions_dir_of(state_dir);
    let store = open_session_store(&sessions_dir).expect("session store");
    let session = store.session(session_id).expect("session membership");
    let mut agent_store = AgentStore::open(state_dir.join("agents")).expect("agent store");
    session
        .loaded_agents()
        .into_iter()
        .map(|agent_id| {
            let tree = agent_store
                .load_agent(agent_id.as_str())
                .expect("load agent")
                .expect("agent tree");
            tree.current_branch().into_iter().cloned().collect()
        })
        .collect()
}

/// Test-only helper that appends a user message through the harness's normal
/// agent-transcript publish path without driving a provider turn.
fn append_user_message_via_event(h: &mut Harness, session_id: &str, text: &str) {
    assert_eq!(session_id, h.session_runtime.current_session_id.as_str());
    let cid = ensure_test_user_agent(h);
    h.publish_pending_prompt_for_agent(&cid, PendingPrompt::user(text.to_owned()))
        .expect("append user message");
}

pub(super) fn echo_harness(state_dir: impl Into<PathBuf>) -> Result<Harness, HarnessError> {
    echo_harness_with_storage_mode(state_dir, crate::HarnessStorageMode::Durable)
}

/// Builds the echo-provider fixture without persistent session or agent stores
/// for tests whose assertions do not cover durability.
fn echo_harness_memory_only(state_dir: impl Into<PathBuf>) -> Result<Harness, HarnessError> {
    echo_harness_with_storage_mode(state_dir, crate::HarnessStorageMode::MemoryOnly)
}

fn echo_harness_with_storage_mode(
    state_dir: impl Into<PathBuf>,
    storage_mode: crate::HarnessStorageMode,
) -> Result<Harness, HarnessError> {
    echo_harness_for_with_storage_mode("s1", state_dir, storage_mode)
}

fn echo_harness_for(
    session_id: &str,
    state_dir: impl Into<PathBuf>,
) -> Result<Harness, HarnessError> {
    echo_harness_for_with_storage_mode(session_id, state_dir, crate::HarnessStorageMode::Durable)
}

fn echo_harness_for_with_storage_mode(
    session_id: &str,
    state_dir: impl Into<PathBuf>,
    storage_mode: crate::HarnessStorageMode,
) -> Result<Harness, HarnessError> {
    let state_dir = state_dir.into();
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(state_dir.join("config")),
        state_dir: Some(state_dir.join("runtime")),
    };
    echo_harness_with_dirs_and_start_reason(
        session_id,
        state_dir,
        dirs,
        tau_proto::SessionStartReason::Initial,
        storage_mode,
    )
}

fn echo_harness_with_dirs(
    session_id: &str,
    state_dir: impl Into<PathBuf>,
    dirs: tau_config::settings::TauDirs,
) -> Result<Harness, HarnessError> {
    echo_harness_with_dirs_and_start_reason(
        session_id,
        state_dir,
        dirs,
        tau_proto::SessionStartReason::Initial,
        crate::HarnessStorageMode::Durable,
    )
}

fn echo_harness_with_start_reason(
    session_id: &str,
    state_dir: impl Into<PathBuf>,
    start_reason: tau_proto::SessionStartReason,
) -> Result<Harness, HarnessError> {
    let state_dir = state_dir.into();
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(state_dir.join("config")),
        state_dir: Some(state_dir.join("runtime")),
    };
    echo_harness_with_dirs_and_start_reason(
        session_id,
        state_dir,
        dirs,
        start_reason,
        crate::HarnessStorageMode::Durable,
    )
}

fn echo_harness_with_dirs_and_start_reason(
    session_id: &str,
    state_dir: impl Into<PathBuf>,
    dirs: tau_config::settings::TauDirs,
    start_reason: tau_proto::SessionStartReason,
    storage_mode: crate::HarnessStorageMode,
) -> Result<Harness, HarnessError> {
    fn shell_runner(r: UnixStream, w: UnixStream, project_root: PathBuf) -> Result<(), String> {
        tau_ext_shell::run_for_test_harness(r, w, project_root).map_err(|e| e.to_string())
    }
    let mut h = Harness::new_with_provider(
        state_dir,
        dirs,
        echo_runner,
        vec![crate::harness::InProcessTool {
            name: "shell",
            runner: shell_runner,
        }],
        session_id,
        start_reason,
        storage_mode,
    )?;
    h.agent_runtime.agent_registry.id_rng = super::deterministic_agent_id_rng();
    h.enable_echo_tool_for_tests();
    // Keep the generic echo helper independent from any test fixture discovery.
    // Readiness and AGENTS.md injection tests add their own deterministic
    // context directly.
    h.prompt_coordination.context_discovery.agents_files.clear();
    // Do not let shell's startup context-provider registration defer unrelated
    // prompt dispatch assertions; readiness-specific tests register providers
    // directly.
    h.prompt_coordination
        .context_discovery
        .agent_context_providers
        .clear();
    h.prompt_coordination
        .context_discovery
        .session_context_providers
        .clear();
    let pending_agents = h
        .prompt_coordination
        .context_discovery
        .pending_agents
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    for agent_id in pending_agents {
        if let Some(pending) = h
            .prompt_coordination
            .context_discovery
            .pending_agents
            .get_mut(&agent_id)
        {
            pending.waiting_on.clear();
        }
        h.finalize_agent_discovery(&agent_id)?;
    }
    Ok(h)
}

/// Ensures the shared in-process provider fixture ignores poisoned startup
/// transports and uses only its explicitly supplied directories. The parent
/// process runs the ignored child so changing process-wide environment cannot
/// race parallel unit tests.
#[test]
fn provider_harness_ignores_ambient_startup_environment() {
    let ambient_home = TempDir::new().expect("ambient home");
    let ambient_skill_dir = ambient_home
        .path()
        .join(".config/agents/skills/ambient-skill");
    std::fs::create_dir_all(&ambient_skill_dir).expect("ambient skill directory");
    std::fs::write(
        ambient_skill_dir.join("SKILL.md"),
        "---\nname: ambient-skill\ndescription: ambient fixture\n---\n",
    )
    .expect("ambient skill");
    let ambient_cwd = ambient_home.path().join("ambient-cwd");
    std::fs::create_dir(&ambient_cwd).expect("ambient working directory");
    let output = path_std_process_Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--ignored",
            "--exact",
            "harness::tests::provider_harness_ignores_ambient_startup_environment_child",
        ])
        .env(tau_config::settings::TAU_PROFILE_ENV, "ambient-profile")
        .env(crate::ROLE_CLI_OVERRIDES_ENV, r#"["ambient-role"]"#)
        .env(
            crate::HARNESS_CONFIG_CLI_OVERRIDES_ENV,
            r#"["ambient-config"]"#,
        )
        .env(crate::STARTUP_ROLE_ENV, "ambient-role")
        .env("HOME", ambient_home.path())
        .env("XDG_CONFIG_HOME", ambient_home.path().join(".config"))
        .env("XDG_STATE_HOME", ambient_home.path().join(".state"))
        .env("XDG_RUNTIME_DIR", ambient_home.path().join(".runtime"))
        .current_dir(&ambient_cwd)
        .output()
        .expect("run isolated fixture child");
    assert!(
        output.status.success(),
        "fixture child failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Runs the shared provider fixture with poisoned startup transport in a
/// single-test child process and proves its explicit configuration wins.
#[test]
#[ignore = "run only through the isolated parent regression"]
fn provider_harness_ignores_ambient_startup_environment_child() {
    assert_eq!(
        std::env::var(tau_config::settings::TAU_PROFILE_ENV).as_deref(),
        Ok("ambient-profile")
    );
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("config");
    let state_dir = temp.path().join("state");
    std::fs::create_dir_all(&config_dir).expect("config directory");
    std::fs::write(
        config_dir.join("harness.yaml"),
        r#"
agents:
  default_role: fixture
  role_groups:
    fixture:
      roles:
        fixture: {}
"#,
    )
    .expect("fixture harness configuration");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(config_dir),
        state_dir: Some(state_dir.clone()),
    };

    let mut harness = echo_harness_with_dirs("fixture-session", &state_dir, dirs)
        .expect("fixture must ignore poisoned startup environment");
    assert_eq!(harness.config.selected_role, "fixture");
    assert_eq!(
        harness.session_runtime.project_root,
        state_dir
            .join("test-project")
            .canonicalize()
            .expect("isolated test project root")
    );
    assert!(
        !harness
            .prompt_coordination
            .context_discovery
            .skills
            .contains_key("ambient-skill")
    );
    assert!(
        harness
            .prompt_coordination
            .context_discovery
            .agents_files
            .is_empty()
    );
    let expected_project_root = harness.session_runtime.project_root.clone();
    let outcome = harness
        .send_user_message("fixture-session", "shell pwd", None)
        .expect("shell command must run in the fixture project root");
    assert!(
        format!("{:?}", outcome.tool_results)
            .contains(expected_project_root.to_string_lossy().as_ref()),
        "shell pwd result must use the fixture project root"
    );
    let tree = agent_tree_for_conversation(&harness, &test_user_agent(&harness));
    assert_eq!(
        tree.metadata()
            .get(&tau_proto::AgentMetadataKey::new("ext_shell_cwd"))
            .map(|entry| &entry.value),
        Some(&CborValue::Text(
            expected_project_root.display().to_string()
        ))
    );
    harness.shutdown().expect("shutdown");
}

fn quiet_provider_harness(state_dir: impl Into<PathBuf>) -> Result<Harness, HarnessError> {
    quiet_provider_harness_with_start_reason_and_storage_mode(
        state_dir,
        tau_proto::SessionStartReason::Initial,
        crate::HarnessStorageMode::Durable,
    )
}

/// Builds the quiet-provider fixture without persistent session or agent stores
/// for tests whose assertions do not cover durability.
fn quiet_provider_harness_memory_only(
    state_dir: impl Into<PathBuf>,
) -> Result<Harness, HarnessError> {
    quiet_provider_harness_with_start_reason_and_storage_mode(
        state_dir,
        tau_proto::SessionStartReason::Initial,
        crate::HarnessStorageMode::MemoryOnly,
    )
}

fn quiet_provider_harness_with_start_reason(
    state_dir: impl Into<PathBuf>,
    start_reason: tau_proto::SessionStartReason,
) -> Result<Harness, HarnessError> {
    quiet_provider_harness_with_start_reason_and_storage_mode(
        state_dir,
        start_reason,
        crate::HarnessStorageMode::Durable,
    )
}

fn quiet_provider_harness_ephemeral(
    state_dir: impl Into<PathBuf>,
) -> Result<Harness, HarnessError> {
    quiet_provider_harness_with_start_reason_and_storage_mode(
        state_dir,
        tau_proto::SessionStartReason::Initial,
        crate::HarnessStorageMode::SessionEphemeral,
    )
}

fn quiet_provider_harness_with_start_reason_and_storage_mode(
    state_dir: impl Into<PathBuf>,
    start_reason: tau_proto::SessionStartReason,
    storage_mode: crate::HarnessStorageMode,
) -> Result<Harness, HarnessError> {
    quiet_provider_harness_for_with_start_reason_and_storage_mode(
        "s1",
        state_dir,
        start_reason,
        storage_mode,
    )
}

fn quiet_provider_harness_for_with_start_reason_and_storage_mode(
    session_id: &str,
    state_dir: impl Into<PathBuf>,
    start_reason: tau_proto::SessionStartReason,
    storage_mode: crate::HarnessStorageMode,
) -> Result<Harness, HarnessError> {
    fn quiet_provider_runner(r: UnixStream, w: UnixStream) -> Result<(), String> {
        fn inner(r: UnixStream, w: UnixStream) -> Result<(), Box<dyn std::error::Error>> {
            let mut reader = TestOutputReader::new(BufReader::new(r));
            let mut writer = TestInputWriter::new(BufWriter::new(w));

            writer.write_frame(&TestProtocolItem::Message(TestMessage::Hello(
                tau_proto::Hello {
                    protocol_version: tau_proto::PROTOCOL_VERSION,
                    client_name: crate::test_extension_name("tau-quiet-provider"),
                    client_kind: tau_proto::ClientKind::Provider,
                    expected_session_id: None,
                    capabilities: Default::default(),
                },
            )))?;
            writer.write_frame(&TestProtocolItem::Event(Event::ProviderModelsDeclared(
                tau_proto::ProviderModelsDeclared {
                    models: vec![tau_proto::ProviderModelInfo {
                        id: "test/model".into(),
                        display_name: Some("Test".to_owned()),
                        tags: Vec::new(),
                        hosted_tool_capabilities: Vec::new(),
                        supported_tool_types: vec![tau_proto::ToolType::Function],
                        input_modalities: Vec::new(),
                        tool_result_modalities: Vec::new(),
                        supports_parallel_tool_calls: true,
                        default_affinity: 0,
                        context_window: tau_proto::TokenCount::new(1_000),
                        efforts: vec![tau_proto::Effort::Medium],
                        verbosities: vec![tau_proto::Verbosity::Medium],
                        thinking_summaries: vec![tau_proto::ThinkingSummary::Auto],
                        supports_compaction: true,
                        supports_standalone_compaction: false,
                        standalone_compaction_generation_negative: false,
                        standalone_compaction_threshold: None,
                        standalone_compaction_prefix_budget: None,
                        cache_policy: None,
                        est_uncached_input_cost_1m_usd: Default::default(),
                        est_cached_input_cost_1m_usd: Default::default(),
                        est_cache_write_input_cost_1m_usd: Default::default(),
                        est_output_cost_1m_usd: Default::default(),
                        est_cache_storage_cost_1m_token_hour_usd: None,
                    }],
                },
            )))?;
            writer.write_frame(&TestProtocolItem::Message(TestMessage::Ready(
                tau_proto::Ready {
                    message: Some("quiet provider ready".to_owned()),
                },
            )))?;
            writer.flush()?;

            while let Some(frame) = reader.read_frame()? {
                let frame = frame.into_event_frame();
                if matches!(frame, TestProtocolItem::Message(TestMessage::Disconnect(_))) {
                    return Ok(());
                }
            }
            Ok(())
        }

        inner(r, w).map_err(|e| e.to_string())
    }

    let state_dir = state_dir.into();
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(state_dir.join("config")),
        state_dir: Some(state_dir.join("runtime")),
    };
    let mut h = Harness::new_with_provider(
        state_dir,
        dirs,
        quiet_provider_runner,
        Vec::new(),
        session_id,
        start_reason,
        storage_mode,
    )?;
    h.agent_runtime.agent_registry.id_rng = super::deterministic_agent_id_rng();
    Ok(h)
}

struct TestSink {
    events: Arc<Mutex<Vec<RoutedFrame>>>,
}

impl ConnectionSink for TestSink {
    fn send(&mut self, event: RoutedFrame) -> Result<(), ConnectionSendError> {
        self.events.lock().expect("sink mutex").push(event);
        Ok(())
    }
}

fn connect_test_client(
    h: &mut Harness,
    name: &str,
    kind: tau_proto::ClientKind,
) -> Arc<Mutex<Vec<RoutedFrame>>> {
    connect_test_client_with_origin(h, name, kind, ConnectionOrigin::InMemory)
}

/// Connect one test peer with an explicit harness-assigned origin.
fn connect_test_client_with_origin(
    h: &mut Harness,
    name: &str,
    kind: tau_proto::ClientKind,
    origin: ConnectionOrigin,
) -> Arc<Mutex<Vec<RoutedFrame>>> {
    let events = Arc::new(Mutex::new(Vec::new()));
    h.runtime_io.bus.connect(Connection::new(
        PendingConnectionMetadata {
            id: Some(crate::test_connection_id(name)),
            name: crate::test_extension_name(name),
            kind,
            origin,
        },
        Box::new(TestSink {
            events: Arc::clone(&events),
        }),
    ));
    events
}

pub(super) fn connect_test_tool(h: &mut Harness, name: &str) -> Arc<Mutex<Vec<RoutedFrame>>> {
    connect_test_client(h, name, tau_proto::ClientKind::Tool)
}

/// Connect one configured extension that has completed activation.
fn connect_ready_configured_extension(
    h: &mut Harness,
    connection_id: &str,
    name: &str,
    kind: tau_proto::ClientKind,
) -> Arc<Mutex<Vec<RoutedFrame>>> {
    let sink = connect_test_client(h, connection_id, kind.clone());
    mark_connected_test_extension_configured(h, connection_id, name, kind);
    sink
}

/// Attach configured-extension identity to an already connected test peer.
fn mark_connected_test_extension_configured(
    h: &mut Harness,
    connection_id: &str,
    name: &str,
    kind: tau_proto::ClientKind,
) {
    let connection_id: tau_proto::ConnectionId = crate::test_connection_id(connection_id);
    h.extensions.entries.insert(
        connection_id.clone(),
        crate::extension::ExtensionEntry {
            tool_prefix: None,
            name: crate::test_extension_name(name),
            instance_id: 42.into(),
            connection_id: connection_id.clone(),
            kind,
            peer_capabilities: Default::default(),
            require: true,
            respawn_allowed: true,
            pid: None,
            in_process_thread: None,
            supervised_config: None,
            secrets: path_std_collections::BTreeMap::new(),
            restart_attempt: 0,
            state: path_crate_extension::ExtensionState::Ready,
            protocol_io: tau_client::ProtocolIoMeter::default(),
        },
    );
    h.extensions.order.push(connection_id);
}

/// Connect one ready tool extension with a stable configured publisher name.
fn connect_ready_message_publisher(h: &mut Harness, connection_id: &str, name: &str) {
    let _sink =
        connect_ready_configured_extension(h, connection_id, name, tau_proto::ClientKind::Tool);
    h.extensions
        .entries
        .get_mut(connection_id)
        .expect("configured message publisher")
        .peer_capabilities = [tau_proto::PeerCapability::MessageBridge]
        .into_iter()
        .collect();
}

/// Pre-seed the per-conversation `AgentThinking` state for tests that
/// bypass `dispatch_prompt_for_agent` and call response handlers
/// directly.
fn seed_agent_thinking(h: &mut Harness, cid: &crate::AgentId, spid: &str) {
    // Tests that bypass prompt dispatch still need the same loaded-agent and
    // session-membership side effects that a real dispatch would establish.
    let agent_id = h
        .ensure_agent_id_for_agent(cid)
        .expect("conversation agent id");
    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(cid)
        .expect("conversation present");
    let role = h.role_name_for_agent(conv).to_owned();
    let model = h
        .model_for_agent_role(conv)
        .or_else(|| h.config.selected_model.clone());
    let tool_specs = h.gather_effective_tool_specs_for_role_model(&role, model.as_ref());
    h.prompt_coordination.prompt_runtime.tool_specs.insert(
        spid.parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
        tool_specs,
    );
    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get_mut(cid)
        .expect("conversation present");
    if let Some(next_index) = spid
        .rsplit_once('-')
        .and_then(|(_, index)| index.parse::<u64>().ok())
        .map(|index| index.saturating_add(1))
    {
        conv.dispatch.next_prompt_index = conv.dispatch.next_prompt_index.max(next_index);
    }
    conv.turn.turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: spid
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
    };
    h.agent_runtime
        .agent_registry
        .agent_routes
        .insert(crate::parse_agent_id(&agent_id), cid.clone());
    if let Some(model) = model {
        h.prompt_coordination.prompt_runtime.models.insert(
            spid.parse::<tau_proto::AgentPromptId>()
                .expect("known-safe AgentPromptId must be valid"),
            model,
        );
    }
}

/// Pre-seed the per-conversation `ToolsRunning` state for tests that
/// bypass the agent-response path and call tool handlers directly.
fn seed_tools_running(h: &mut Harness, cid: &crate::AgentId, remaining: Vec<ToolCallId>) {
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(cid)
        .expect("conversation present")
        .turn
        .turn_state = AgentTurnState::ToolsRunning {
        remaining_calls: remaining.into(),
    };
}

/// Seed the transcript and turn state as if the assistant had just
/// emitted one or more tool calls for this conversation.
fn seed_assistant_tool_round(h: &mut Harness, cid: &crate::AgentId, calls: &[(&str, &str)]) {
    let agent_id = h
        .agent_runtime
        .agent_registry
        .agents
        .get(cid)
        .and_then(|conv| conv.identity.agent_id.clone())
        .unwrap_or_else(|| crate::parse_agent_id("main"));
    h.publish_for_agent(
        cid,
        Event::ProviderResponseFinished(ProviderResponseFinished {
            automatic_compaction_decision: None,
            estimated_api_cost_rates: None,
            estimated_api_cost_increment: None,

            agent_prompt_id: "sp-seeded-tools"
                .parse::<tau_proto::AgentPromptId>()
                .expect("known-safe AgentPromptId must be valid"),
            agent_id: crate::parse_agent_id(&agent_id),
            output_items: calls
                .iter()
                .map(|(call_id, tool_name)| {
                    ContextItem::ToolCall(ToolCallItem {
                        call_id: (*call_id).into(),
                        name: ToolName::new(*tool_name),
                        tool_type: tau_proto::ToolType::Function,
                        arguments: CborValue::Map(Vec::new()),
                        raw_arguments_json: None,
                        responses_envelope: None,
                    })
                })
                .collect(),
            stop_reason: tau_proto::ProviderStopReason::ToolCalls,
            error: None,
            failure_kind: None,
            context_limit_telemetry: None,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            output_length_disposition: tau_proto::OutputLengthDisposition::None,
            usage: None,
            originator: tau_proto::PromptOriginator::User,
            compaction_original_input_tokens: None,
            compaction_output_tokens: None,
            backend: None,
            provider_attempt: Default::default(),
            provider_response_id: None,
            ws_pool_delta: None,
        }),
    );
    seed_tools_running(
        h,
        cid,
        calls.iter().map(|(call_id, _)| (*call_id).into()).collect(),
    );
}

/// Harness tool-call normalization may rewrite ids or routed tool metadata
/// before persisting the provider response, but it must not discard provider
/// replay sidecars used later for Responses cache identity.
#[test]
fn rewrite_finished_response_tool_call_items_preserves_provider_replay_sidecars() {
    let raw_arguments = "{ \"z\" : 1, \"a\" : [2, 3] }";
    let responses_envelope = ResponsesToolCallEnvelope {
        item_id: Some("fc_provider_item".to_owned()),
        status: Some("completed".to_owned()),
        extra_fields: Some(CborValue::Map(vec![(
            CborValue::Text("provider_future".to_owned()),
            CborValue::Bool(true),
        )])),
    };
    let mut response = ProviderResponseFinished {
        automatic_compaction_decision: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: "sp-raw"
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
        agent_id: crate::parse_agent_id("main"),
        output_items: vec![
            ContextItem::Message(MessageItem {
                role: ContextRole::Assistant,
                content: vec![ContentPart::Text {
                    text: "before".to_owned(),
                }],
                phase: None,
                responses_raw_json: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "call-original".into(),
                name: ToolName::new("shell"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: Some(raw_arguments.to_owned()),
                responses_envelope: Some(responses_envelope.clone()),
            }),
            ContextItem::Message(MessageItem {
                role: ContextRole::Assistant,
                content: vec![ContentPart::Text {
                    text: "after".to_owned(),
                }],
                phase: None,
                responses_raw_json: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "call-original".into(),
                name: ToolName::new("shell"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: Some(r#"{"second":true}"#.to_owned()),
                responses_envelope: None,
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    };
    let mut normalized_calls = ["call-normalized", "invalid_tool_call_sp-raw_2"]
        .into_iter()
        .map(|id| NormalizedFinishedToolCall {
            turn_categories: ToolTurnCategories::default(),
            call: AgentToolCall {
                call_ref: None,
                id: id.into(),
                name: ToolName::new("shell"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
            },
            background_support: tau_proto::BackgroundSupport::Never,
        })
        .collect::<Vec<_>>();
    let output_items_ptr = response.output_items.as_ptr();
    let output_items_capacity = response.output_items.capacity();

    let declaration = tau_proto::ObservationId::random();
    Harness::rewrite_finished_response_tool_call_items(
        &mut response,
        &mut normalized_calls,
        declaration,
    );

    let ContextItem::ToolCall(call) = &response.output_items[1] else {
        panic!("expected rewritten tool call");
    };
    assert_eq!(call.call_id.as_str(), "call-normalized");
    assert_eq!(
        response.output_items.as_ptr(),
        output_items_ptr,
        "in-place normalization must retain the canonical output allocation"
    );
    assert_eq!(response.output_items.capacity(), output_items_capacity);
    assert_eq!(
        normalized_calls[0].call.call_ref,
        Some(tau_proto::ToolCallRef {
            declaration,
            item_index: 1,
        })
    );
    assert_eq!(
        normalized_calls[1].call.call_ref,
        Some(tau_proto::ToolCallRef {
            declaration,
            item_index: 3,
        })
    );
    assert_eq!(call.raw_arguments_json.as_deref(), Some(raw_arguments));
    assert_eq!(call.responses_envelope, Some(responses_envelope));
    let ContextItem::ToolCall(second_call) = &response.output_items[3] else {
        panic!("expected second rewritten tool call");
    };
    assert_eq!(second_call.call_id.as_str(), "invalid_tool_call_sp-raw_2");
    assert_eq!(
        second_call.raw_arguments_json.as_deref(),
        Some(r#"{"second":true}"#)
    );
    assert!(matches!(
        (&response.output_items[0], &response.output_items[2]),
        (ContextItem::Message(before), ContextItem::Message(after))
            if matches!(&before.content[0], ContentPart::Text { text } if text == "before")
                && matches!(&after.content[0], ContentPart::Text { text } if text == "after")
    ));
}

/// Receives the next harness event after exposing the causal wait cut to a
/// test.
fn recv_next_harness_event_after(h: &Harness, before_wait: impl FnOnce()) -> HarnessEvent {
    before_wait();
    h.expand_component_ingress_wake(
        h.runtime_io
            .rx
            .recv()
            .expect("harness event channel should remain connected"),
    )
}

/// Receives the next harness event without imposing a scheduler-sensitive
/// wall-clock deadline on its producer.
fn recv_next_harness_event(h: &Harness) -> HarnessEvent {
    recv_next_harness_event_after(h, || {})
}

/// Pumps the harness event loop until the named tool call's result
/// or error is received and handled.
fn drive_harness_until_call_completes(h: &mut Harness, target_call_id: &str) {
    loop {
        let event = recv_next_harness_event(h);
        match event {
            HarnessEvent::FromConnection {
                connection_id,
                message,
                ..
            } => {
                let is_target = match message.as_ref() {
                    HarnessInputMessage::Emit(emit) => match emit.event.as_ref() {
                        Event::ToolResultReported(r) | Event::ToolResult(r) => {
                            r.call_id.as_str() == target_call_id
                        }
                        Event::ToolErrorReported(e) | Event::ToolError(e) => {
                            e.call_id.as_str() == target_call_id
                        }
                        _ => false,
                    },
                    _ => false,
                };
                h.handle_extension_message(&connection_id, *message)
                    .expect("handle");
                if is_target {
                    return;
                }
            }
            HarnessEvent::Disconnected { connection_id } => {
                h.handle_disconnect(&connection_id);
            }
            HarnessEvent::ReadFailed { connection_id, .. } => {
                h.handle_disconnect(&connection_id);
            }
            HarnessEvent::NewClient(_) => {}
            HarnessEvent::SupervisedWriterCleanupComplete { connection_id } => h
                .handle_supervised_writer_cleanup_complete_at(&connection_id, Instant::now())
                .expect("supervised cleanup"),
            HarnessEvent::ComponentIngressReady => unreachable!("wake expanded"),
            HarnessEvent::Command(command) => h.handle_harness_command(command).expect("handle"),
        }
    }
}

fn drive_harness_until_tool_turn_empty(h: &mut Harness) {
    loop {
        if h.tool_routing.tool_runtime.tool_turn.is_empty() {
            return;
        }
        let event = recv_next_harness_event(h);
        match event {
            HarnessEvent::FromConnection {
                connection_id,
                message,
                ..
            } => h
                .handle_extension_message(&connection_id, *message)
                .expect("handle"),
            HarnessEvent::Disconnected { connection_id } => {
                h.handle_disconnect(&connection_id);
            }
            HarnessEvent::ReadFailed { connection_id, .. } => {
                h.handle_disconnect(&connection_id);
            }
            HarnessEvent::NewClient(_) => {}
            HarnessEvent::SupervisedWriterCleanupComplete { connection_id } => h
                .handle_supervised_writer_cleanup_complete_at(&connection_id, Instant::now())
                .expect("supervised cleanup"),
            HarnessEvent::ComponentIngressReady => unreachable!("wake expanded"),
            HarnessEvent::Command(command) => h.handle_harness_command(command).expect("handle"),
        }
    }
}

/// A harness helper must await causally delayed ingress rather than declaring a
/// timeout while a runnable producer is descheduled under host contention.
#[test]
fn tool_result_driver_waits_for_causally_released_ingress() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let tx = h.runtime_io.tx.clone();
    let (producer_ready_tx, producer_ready_rx) = mpsc::sync_channel(0);
    let (release_tx, release_rx) = mpsc::sync_channel(0);
    let delayed_connection = crate::test_connection_id("causally-delayed");
    let sent_connection = delayed_connection.clone();
    let producer = thread::spawn(move || {
        producer_ready_tx
            .send(())
            .expect("report producer parked at the causal cut");
        release_rx
            .recv()
            .expect("test should release the parked producer");
        tx.send(HarnessEvent::Disconnected {
            connection_id: sent_connection,
        })
        .expect("send causally delayed ingress");
    });

    producer_ready_rx
        .recv()
        .expect("producer should reach the causal cut");
    let event = recv_next_harness_event_after(&h, || {
        release_tx
            .send(())
            .expect("release producer only once the helper begins its wait");
    });
    assert!(matches!(
        event,
        HarnessEvent::Disconnected { connection_id }
            if connection_id == delayed_connection
    ));

    producer.join().expect("delayed producer");
    h.shutdown().expect("shutdown");
}

fn wait_for_session_unlock(state_dir: &Path, session_id: &str) {
    let sessions_dir = tau_config::settings::sessions_dir_of(state_dir);
    let started = Instant::now();
    loop {
        let locked =
            tau_core::session_is_locked(&sessions_dir, session_id).expect("session lock probe");
        if !locked {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "timed out waiting for session `{session_id}` lock to clear"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Find the conversation id of the outer side conversation (the one
/// whose originator is the delegate extension's first query). Used by
/// the cross-conversation regression test above to disambiguate
/// nested-vs-outer side prompt ids.
fn outer_side_cid_str(h: &Harness) -> &str {
    h.agent_runtime
        .agent_registry
        .agents
        .iter()
        .find_map(|(cid, conv)| {
            matches!(
                &conv.identity.originator,
                tau_proto::PromptOriginator::Extension { query_id, .. }
                    if query_id == "q-outer"
            )
            .then_some(cid.as_str())
        })
        .unwrap_or("")
}

/// Peel a routed message to its bus-event payload. Returns `None` for
/// non-event output messages (configure, intercept request, …).
fn peel_inner_event(message: &HarnessOutputMessage) -> Option<&Event> {
    message.delivered_event()
}

fn read_raw_prompt_created(h: &Harness, spid: &AgentPromptId) -> AgentPromptCreated {
    let mut cursor = path_crate_event_log::EventLogSeq::new(0);
    loop {
        let entry = h
            .runtime_io
            .event_log
            .get_next_from(cursor)
            .expect("prompt event in log");
        cursor = entry.seq.next();
        match entry.event {
            Event::AgentPromptCreated(prompt) if &prompt.agent_prompt_id == spid => {
                return prompt;
            }
            _ => {}
        }
    }
}

fn read_nth_prompt_created(h: &Harness, index: usize) -> AgentPromptCreated {
    let mut cursor = path_crate_event_log::EventLogSeq::new(0);
    let mut seen = 0;
    loop {
        let entry = h
            .runtime_io
            .event_log
            .get_next_from(cursor)
            .expect("prompt event in log");
        cursor = entry.seq.next();
        if let Event::AgentPromptCreated(prompt) = entry.event {
            if seen == index {
                return h
                    .read_agent_prompt_created(&prompt.session_id, &prompt.agent_prompt_id)
                    .expect("materialized prompt event");
            }
            seen += 1;
        }
    }
}

/// Commit one ordinary response with exact nonzero provider usage.
fn establish_exact_provider_usage(
    h: &mut Harness,
    cid: &AgentId,
    tokens: u64,
) -> tau_proto::AgentPromptId {
    let prompt_index =
        dispatch::event_log_count(h, |event| matches!(event, Event::AgentPromptCreated(_)));
    h.dispatch_prompt_for_agent(cid, PendingPrompt::user("exact usage baseline".to_owned()))
        .expect("dispatch exact usage baseline");
    let prompt = read_nth_prompt_created(h, prompt_index);
    let mut response = dispatch::provider_text_response(
        &prompt.agent_prompt_id,
        prompt.agent_id,
        "baseline accepted",
    );
    response.usage = Some(tau_proto::ProviderTokenUsage {
        model: Some(prompt.model),
        prompt_sent_tokens: tokens,
        prompt_cached_tokens: 0,
        prompt_cache_read_ceiling_tokens: None,
        cache: None,
        response_received_tokens: 1,
        stats: Default::default(),
    });
    h.handle_provider_response_finished(response)
        .expect("accept exact usage baseline");
    prompt.agent_prompt_id
}

fn publish_exact_automatic_start(
    h: &mut Harness,
    cid: &AgentId,
    cut: tau_proto::AgentHead,
    model: tau_proto::ModelId,
    transaction: &str,
    compact_prompt: &str,
    provider_prompt_id: tau_proto::AgentPromptId,
) {
    let agent = &h.agent_runtime.agent_registry.agents[cid];
    let provider_input_tokens = agent.execution.context_input_tokens.expect("exact usage");
    h.publish_for_agent(
        cid,
        Event::AgentStandaloneCompactionStarted(tau_proto::AgentStandaloneCompactionStarted {
            compact_prompt_id: tau_proto::AgentPromptId::parse(compact_prompt)
                .expect("compact prompt"),
            operation: tau_proto::PromptOperation::StandaloneCompaction,
            agent_id: tau_proto::AgentId::parse(
                agent.identity.agent_id.as_deref().expect("durable agent"),
            )
            .expect("agent id"),
            transaction_id: tau_proto::CompactionTransactionId::parse(transaction)
                .expect("transaction"),
            cut,
            resume_through: Some(cut),
            model,
            originator: agent.identity.originator.clone(),
            supersedes: None,
            trigger: tau_proto::StandaloneCompactionTrigger::AutomaticThresholdEvidence {
                evidence: tau_proto::ProactiveCompactionEvidence {
                    provider_prompt_id,
                    provider_input_tokens,
                    threshold: tau_proto::TokenCount::new(1),
                    threshold_source: tau_proto::CompactionThresholdSource::ProviderDefault,
                },
            },
        }),
    );
}

fn read_prompt_created(h: &Harness, spid: &AgentPromptId) -> AgentPromptCreated {
    let raw = read_raw_prompt_created(h, spid);
    h.read_agent_prompt_created(&raw.session_id, spid)
        .expect("materialized prompt event")
}

fn intercepted_payload(events: &Arc<Mutex<Vec<RoutedFrame>>>) -> (Event, bool) {
    let events = events.lock().expect("events mutex");
    let intercepted = events
        .iter()
        .find_map(|routed| match &routed.frame {
            HarnessOutputMessage::InterceptRequest(req) => Some(req),
            _ => None,
        })
        .expect("intercept request delivered");
    ((*intercepted.event).clone(), intercepted.persist)
}

fn draft_event(text: &str) -> Event {
    Event::UiPromptDraft(UiPromptDraft {
        session_id: "s1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        target_agent_id: None,
        text: Some(text.to_owned()),
    })
}

mod action;
mod agent_list;
mod agent_watch_wait;
mod dedup;
mod dispatch;
mod interception;
mod lifecycle;
mod mode;
mod model;
mod provider_execution_reports;
mod quota;
mod replay;
mod retry;
mod session_init;
mod strict_compaction_provider;

use strict_compaction_provider::{
    strict_compaction_provider_harness, strict_compaction_provider_harness_with_start_reason,
    validate_closed_tool_timeline,
};

/// Harness-owned correlation producers preserve their complete maximum inputs
/// within each validated identifier cap.
#[test]
fn generated_runtime_initialization_and_shell_ids_accept_maximum_inputs() {
    let runtime = super::accounting_runtime_id(u64::MAX);
    assert_eq!(runtime.as_str(), "ffffffffffffffff");
    assert!(runtime.as_str().len() <= 32);

    let initialization =
        super::agent_initialization_id(&runtime, SessionGeneration::from_raw(u64::MAX), u64::MAX);
    assert_eq!(
        initialization.as_str(),
        "ffffffffffffffff-ffffffffffffffff-ffffffffffffffff"
    );
    assert!(initialization.as_str().len() <= 64);

    let shell = super::shell_route_id(u64::MAX, u64::MAX);
    assert_eq!(
        shell.as_str(),
        "harness-shell-ffffffffffffffffffffffffffffffff"
    );
    assert!(shell.as_str().len() <= 64);
}
