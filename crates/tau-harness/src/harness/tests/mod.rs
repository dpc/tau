//! Test suite for the harness. Split by concern around major harness behaviors
//! such as interception, replay, dispatch, lifecycle, and tool handling.
//!
//! The shared helpers and imports live here so each submodule can
//! pull them in with `use super::*;`.

use std::io::{BufReader, BufWriter, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tau_core::{
    AgentEntry, AgentStore, AgentTree, Connection, ConnectionMetadata, ConnectionOrigin,
    ConnectionSendError, ConnectionSink, RoutedFrame,
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
use tau_session_inspect::{
    default_session_id, format_session_entry, open_session_store, policy_lines, session_lines,
    session_list_lines,
};
use tempfile::TempDir;

use super::{AgentToolCall, HARNESS_CONNECTION_ID, Harness, NormalizedFinishedToolCall};
use crate::AgentId;
use crate::agent::{AgentTurnState, PendingPrompt};
use crate::daemon::{
    ServeOptions, bind_listener, get_daemon_rendered_system_prompt,
    get_daemon_rendered_tool_definitions, run_daemon_with_echo, run_embedded_message_with_echo,
    send_daemon_message, send_daemon_message_with_trace,
};
use crate::discovery::{DiscoveredAgentsFile, DiscoveredSkill, DiscoveredSkillSource};
use crate::error::HarnessError;
use crate::event::HarnessEvent;
use crate::model::{
    InterSessionReceiverRole, clamp_effort, efforts_for_model, load_roles, role_infos,
    select_model_for_role, selected_params_for_role, thinking_summaries_for_model,
    verbosities_for_model,
};
use crate::turn::{PromptSubmission, TurnState};

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
    harness.inter_session_receivers = receivers
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
        self.handle_extension_message(source_id, frame.into_input_message())
    }

    fn handle_client_event(
        &mut self,
        client_id: &str,
        frame: TestProtocolItem,
    ) -> Result<bool, HarnessError> {
        self.handle_client_message(client_id, frame.into_input_message())
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
        source_id: source_id.into(),
        description: description.to_owned(),
        source: DiscoveredSkillSource::File(PathBuf::from(format!("/tmp/{description}.md"))),
        add_to_prompt: false,
        user_invocable: true,
        disable_model_invocation: false,
        modified: Some(std::time::UNIX_EPOCH + Duration::from_secs(modified_secs)),
    }
}

fn write_skill_file(dir: &Path, name: &str, description: &str, mtime: Option<u64>) -> PathBuf {
    let path = dir.join(format!("{description}.md"));
    std::fs::write(
        &path,
        format!("---\nname: {name}\ndescription: {description}\n---\n"),
    )
    .expect("write skill file");
    if let Some(mtime) = mtime {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open skill file");
        let modified = std::time::UNIX_EPOCH + Duration::from_secs(mtime);
        file.set_times(std::fs::FileTimes::new().set_modified(modified))
            .expect("set skill mtime");
    }
    path
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

    h.discovered_skill_candidates
        .insert(name.clone(), vec![older, newer]);
    h.recompute_discovered_skill_winner(&name);
    assert_eq!(h.discovered_skills[&name].description, "newer");

    h.remove_discovered_context("new-ext");
    assert_eq!(h.discovered_skills[&name].description, "older");

    h.shutdown().expect("shutdown");
}

/// Ensures cross-source skill collisions emit useful trace diagnostics for both
/// replacement by newer mtimes and ignoring equal/unavailable timestamps
/// without becoming mandatory visible notices.
#[test]
fn skill_collision_diagnostics_describe_replaced_and_ignored_candidates() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let old_path = write_skill_file(tmp.path(), "collision-skill", "old", Some(100));
    let new_path = write_skill_file(tmp.path(), "collision-skill", "new", Some(200));
    let tie_path = write_skill_file(tmp.path(), "collision-skill", "tie", Some(200));

    h.record_discovered_skill(
        "old-ext",
        &tau_proto::ExtSkillAvailable {
            name: "collision-skill".into(),
            description: "old".to_owned(),
            file_path: old_path,
            add_to_prompt: false,
            user_invocable: true,
            disable_model_invocation: false,
            argument_hint: None,
        },
    );
    h.record_discovered_skill(
        "new-ext",
        &tau_proto::ExtSkillAvailable {
            name: "collision-skill".into(),
            description: "new".to_owned(),
            file_path: new_path,
            add_to_prompt: false,
            user_invocable: true,
            disable_model_invocation: false,
            argument_hint: None,
        },
    );
    h.record_discovered_skill(
        "tie-ext",
        &tau_proto::ExtSkillAvailable {
            name: "collision-skill".into(),
            description: "tie".to_owned(),
            file_path: tie_path,
            add_to_prompt: false,
            user_invocable: true,
            disable_model_invocation: false,
            argument_hint: None,
        },
    );

    let infos = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::HarnessNotice(info) => Some(info),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(infos.iter().any(|info| {
        info.kind == tau_proto::notice_kind::SKILL_COLLISION
            && info.level == tau_proto::NoticeLevel::Trace
            && !info.always_show
            && info.message.contains("skill collision: collision-skill")
            && info.message.contains("replaces")
            && info.message.contains("newer modified time")
    }));
    assert!(infos.iter().any(|info| {
        info.kind == tau_proto::notice_kind::SKILL_COLLISION
            && info.level == tau_proto::NoticeLevel::Trace
            && !info.always_show
            && info.message.contains("skill collision: collision-skill")
            && info.message.contains("ignored")
            && info.message.contains("same or unavailable modified time")
    }));

    h.shutdown().expect("shutdown");
}

/// Ensures user slash invocation expands to a Pi-style prompt block and appends
/// opaque arguments without making disabled-model skills unavailable to users.
#[test]
fn user_skill_command_expands_prompt_block() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let path = tmp.path().join("manual.md");
    std::fs::write(
        &path,
        "---\nname: manual\ndescription: Manual\ndisable-model-invocation: true\n---\nUse manual steps.\n",
    )
    .expect("write skill");
    h.record_discovered_skill(
        "ext",
        &tau_proto::ExtSkillAvailable {
            name: "manual".into(),
            description: "Manual".to_owned(),
            file_path: path.clone(),
            add_to_prompt: true,
            user_invocable: true,
            disable_model_invocation: true,
            argument_hint: Some("[topic]".to_owned()),
        },
    );

    let expanded = h
        .expand_user_skill_command("/skill manual do this")
        .expect("expanded");
    assert!(expanded.contains("<skill name=\"manual\" location="));
    assert!(expanded.contains("References are relative to"));
    assert!(expanded.contains("Use manual steps."));
    assert!(expanded.ends_with("</skill>\n\ndo this"));
}

/// Ensures `disable-model-invocation` is treated as manual-only rather than
/// unreachable, even if an extension also sends `user-invocable: false`.
#[test]
fn disable_model_invocation_implies_user_invocable() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let path = tmp.path().join("manual-only.md");
    std::fs::write(
        &path,
        "---\nname: manual-only\ndescription: Manual only\n---\nManual only body.\n",
    )
    .expect("write skill");
    h.record_discovered_skill(
        "ext",
        &tau_proto::ExtSkillAvailable {
            name: "manual-only".into(),
            description: "Manual only".to_owned(),
            file_path: path,
            add_to_prompt: true,
            user_invocable: false,
            disable_model_invocation: true,
            argument_hint: None,
        },
    );

    assert!(h.discovered_skills["manual-only"].user_invocable);
    assert!(h.discovered_skills["manual-only"].disable_model_invocation);
    assert!(
        h.expand_user_skill_command("/skill manual-only")
            .expect("expanded")
            .contains("Manual only body.")
    );
}

/// Ensures the harness normalizes manual-only skill policy before publishing
/// availability events that UI clients use for `/skill` completion.
#[test]
fn published_skill_event_normalizes_disable_model_invocation() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let path = tmp.path().join("manual-event.md");
    std::fs::write(
        &path,
        "---\nname: manual-event\ndescription: Manual event\n---\nManual event body.\n",
    )
    .expect("write skill");

    h.publish_extension_skill_available(
        "ext",
        tau_proto::ExtSkillAvailable {
            name: "manual-event".into(),
            description: "Manual event".to_owned(),
            file_path: path,
            add_to_prompt: false,
            user_invocable: false,
            disable_model_invocation: true,
            argument_hint: Some("<task>".to_owned()),
        },
    );

    let published = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::ExtSkillAvailable(skill) if skill.name.as_str() == "manual-event" => Some(skill),
            _ => None,
        })
        .expect("published skill event");
    assert!(published.user_invocable);
    assert!(published.disable_model_invocation);
    assert_eq!(published.argument_hint.as_deref(), Some("<task>"));
}

/// Ensures non-user-invocable skill commands are rejected before any prompt is
/// submitted to the model path.
#[test]
fn user_skill_command_rejects_non_user_invocable_skill() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let path = tmp.path().join("hidden.md");
    std::fs::write(
        &path,
        "---\nname: hidden\ndescription: Hidden\n---\nHidden body.\n",
    )
    .expect("write skill");
    h.record_discovered_skill(
        "ext",
        &tau_proto::ExtSkillAvailable {
            name: "hidden".into(),
            description: "Hidden".to_owned(),
            file_path: path,
            add_to_prompt: false,
            user_invocable: false,
            disable_model_invocation: false,
            argument_hint: None,
        },
    );

    assert!(h.expand_user_skill_command("/skill hidden").is_none());
    let infos = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::HarnessNotice(info) => Some(info.message),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        infos
            .iter()
            .any(|message| message.contains("not user-invocable"))
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
    std::os::unix::fs::symlink("/tmp", root.join("outside")).expect("symlink");

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
        std::os::unix::fs::symlink("/tmp", root.join("leaf")).expect("leaf symlink");
        std::os::unix::fs::symlink("/tmp", root.join("parent")).expect("parent symlink");
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
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755))
            .expect("chmod real");
        std::os::unix::fs::symlink(&real, &root).expect("root symlink");
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
        let role = h.selected_role.clone();
        let first = h.create_durable_user_agent("s1".into(), &role);
        let second = h.create_durable_user_agent("s1".into(), &role);
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
    let template = tau_config::settings::HarnessSettings::built_in()
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
        .agents
        .get(cid)
        .and_then(|conv| conv.agent_id.as_deref())
        .expect("conversation has agent id");
    h.agent_store.agent(agent_id).expect("agent tree")
}

pub(super) fn ensure_test_user_agent(h: &mut Harness) -> AgentId {
    let cid = h
        .agents
        .iter()
        .find_map(|(cid, conv)| conv.originator.is_user().then_some(cid.clone()))
        .unwrap_or_else(|| {
            let session_id = h.current_session_id.clone();
            let role = h.selected_role.clone();
            h.create_durable_user_agent(session_id, &role)
        });
    // Most harness unit tests use this helper to focus on tool/provider state,
    // not extension-provided prompt context. Treat the synthetic agent as if
    // registered context providers have already acknowledged it; tests that
    // exercise context readiness drive `session.agent_loaded` explicitly.
    if let Some(agent_id) = h
        .agents
        .get(&cid)
        .and_then(|conv| conv.agent_id.as_deref())
        .map(crate::parse_agent_id)
    {
        h.pending_agent_context_ready.remove(&agent_id);
    }
    cid
}

fn test_user_agent(h: &Harness) -> AgentId {
    h.agents
        .iter()
        .find_map(|(cid, conv)| conv.originator.is_user().then_some(cid.clone()))
        .expect("test should create a user agent first")
}

fn durable_agent_id_for_conversation(h: &Harness, cid: &AgentId) -> tau_proto::AgentId {
    crate::parse_agent_id(
        h.agents
            .get(cid)
            .and_then(|conv| conv.agent_id.clone())
            .expect("conversation has durable agent id"),
    )
}

fn default_agent_tree(h: &Harness) -> &AgentTree {
    let cid = test_user_agent(h);
    agent_tree_for_conversation(h, &cid)
}

fn agent_branch_for_conversation<'a>(h: &'a Harness, cid: &AgentId) -> Vec<&'a AgentEntry> {
    let head = h.agents.get(cid).and_then(|conv| conv.head);
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
    let mut seq = crate::event_log::EventLogSeq::new(0);
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        events.push(entry.event);
    }
    events
}

fn loaded_agent_events(h: &Harness, session_id: &str) -> Vec<Event> {
    let Some(session) = h.store.session(session_id) else {
        return Vec::new();
    };

    session
        .loaded_agents()
        .into_iter()
        .filter_map(|agent_id| h.agent_store.agent_events(agent_id.as_str()).ok())
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
    assert_eq!(session_id, h.current_session_id.as_str());
    let cid = ensure_test_user_agent(h);
    h.publish_pending_prompt_for_agent(&cid, PendingPrompt::user(text.to_owned()))
        .expect("append user message");
}

pub(super) fn echo_harness(state_dir: impl Into<PathBuf>) -> Result<Harness, HarnessError> {
    echo_harness_for("s1", state_dir)
}

fn echo_harness_for(
    session_id: &str,
    state_dir: impl Into<PathBuf>,
) -> Result<Harness, HarnessError> {
    let state_dir = state_dir.into();
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(state_dir.join("config")),
        state_dir: Some(state_dir.join("runtime")),
    };
    echo_harness_with_dirs(session_id, state_dir, dirs)
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
    echo_harness_with_dirs_and_start_reason(session_id, state_dir, dirs, start_reason)
}

fn echo_harness_with_dirs_and_start_reason(
    session_id: &str,
    state_dir: impl Into<PathBuf>,
    dirs: tau_config::settings::TauDirs,
    start_reason: tau_proto::SessionStartReason,
) -> Result<Harness, HarnessError> {
    fn shell_runner(r: UnixStream, w: UnixStream) -> Result<(), String> {
        tau_ext_shell::run(r, w).map_err(|e| e.to_string())
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
        tau_core::SessionPersistenceMode::Durable,
    )?;
    h.agent_id_rng = super::deterministic_agent_id_rng();
    h.enable_echo_tool_for_tests();
    // Keep the generic echo helper independent from the repository AGENTS.md
    // files discovered by ext-shell from the test process cwd. Readiness and
    // AGENTS.md injection tests add their own deterministic context directly.
    h.discovered_agents_files.clear();
    // Do not let shell's startup context-provider registration defer unrelated
    // prompt dispatch assertions; readiness-specific tests register providers
    // directly.
    h.agent_context_providers.clear();
    h.session_context_providers.clear();
    h.pending_agent_context_ready.clear();
    Ok(h)
}

fn quiet_provider_harness(state_dir: impl Into<PathBuf>) -> Result<Harness, HarnessError> {
    quiet_provider_harness_with_start_reason_and_persistence(
        state_dir,
        tau_proto::SessionStartReason::Initial,
        tau_core::SessionPersistenceMode::Durable,
    )
}

fn quiet_provider_harness_with_start_reason(
    state_dir: impl Into<PathBuf>,
    start_reason: tau_proto::SessionStartReason,
) -> Result<Harness, HarnessError> {
    quiet_provider_harness_with_start_reason_and_persistence(
        state_dir,
        start_reason,
        tau_core::SessionPersistenceMode::Durable,
    )
}

fn quiet_provider_harness_ephemeral(
    state_dir: impl Into<PathBuf>,
) -> Result<Harness, HarnessError> {
    quiet_provider_harness_with_start_reason_and_persistence(
        state_dir,
        tau_proto::SessionStartReason::Initial,
        tau_core::SessionPersistenceMode::Ephemeral,
    )
}

fn quiet_provider_harness_with_start_reason_and_persistence(
    state_dir: impl Into<PathBuf>,
    start_reason: tau_proto::SessionStartReason,
    session_persistence: tau_core::SessionPersistenceMode,
) -> Result<Harness, HarnessError> {
    quiet_provider_harness_for_with_start_reason_and_persistence(
        "s1",
        state_dir,
        start_reason,
        session_persistence,
    )
}

fn quiet_provider_harness_for_with_start_reason_and_persistence(
    session_id: &str,
    state_dir: impl Into<PathBuf>,
    start_reason: tau_proto::SessionStartReason,
    session_persistence: tau_core::SessionPersistenceMode,
) -> Result<Harness, HarnessError> {
    fn quiet_provider_runner(r: UnixStream, w: UnixStream) -> Result<(), String> {
        fn inner(r: UnixStream, w: UnixStream) -> Result<(), Box<dyn std::error::Error>> {
            let mut reader = TestOutputReader::new(BufReader::new(r));
            let mut writer = TestInputWriter::new(BufWriter::new(w));

            writer.write_frame(&TestProtocolItem::Message(TestMessage::Hello(
                tau_proto::Hello {
                    protocol_version: tau_proto::PROTOCOL_VERSION,
                    client_name: "tau-quiet-provider".into(),
                    client_kind: tau_proto::ClientKind::Provider,
                },
            )))?;
            writer.write_frame(&TestProtocolItem::Event(Event::ProviderModelsUpdated(
                tau_proto::ProviderModelsUpdated {
                    models: vec![tau_proto::ProviderModelInfo {
                        id: "test/model".into(),
                        display_name: Some("Test".to_owned()),
                        tags: Vec::new(),
                        supported_tool_types: vec![],
                        input_modalities: Vec::new(),
                        tool_result_modalities: Vec::new(),
                        supports_parallel_tool_calls: true,
                        default_affinity: 0,
                        context_window: 1_000,
                        efforts: vec![tau_proto::Effort::Medium],
                        verbosities: vec![tau_proto::Verbosity::Medium],
                        thinking_summaries: vec![tau_proto::ThinkingSummary::Auto],
                        supports_compaction: true,
                        supports_standalone_compaction: false,
                        standalone_compaction_threshold: None,
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
        session_persistence,
    )?;
    h.agent_id_rng = super::deterministic_agent_id_rng();
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
    let events = Arc::new(Mutex::new(Vec::new()));
    h.bus.connect(Connection::new(
        ConnectionMetadata {
            id: name.into(),
            name: name.to_owned(),
            kind,
            origin: ConnectionOrigin::InMemory,
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

/// Connect one ready tool extension with a stable configured publisher name.
fn connect_ready_message_publisher(h: &mut Harness, connection_id: &str, name: &str) {
    let _sink = connect_test_tool(h, connection_id);
    let connection_id: tau_proto::ConnectionId = connection_id.into();
    h.extensions.entries.insert(
        connection_id.clone(),
        crate::extension::ExtensionEntry {
            tool_prefix: None,
            name: name.to_owned(),
            instance_id: 42.into(),
            connection_id: connection_id.clone(),
            kind: tau_proto::ClientKind::Tool,
            require: true,
            respawn_allowed: true,
            pid: None,
            in_process_thread: None,
            supervised_config: None,
            secrets: std::collections::BTreeMap::new(),
            restart_attempt: 0,
            state: crate::extension::ExtensionState::Ready,
            protocol_io: tau_client::ProtocolIoMeter::default(),
        },
    );
    h.extensions.order.push(connection_id);
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
    let conv = h.agents.get(cid).expect("conversation present");
    let role = h.role_name_for_agent(conv).to_owned();
    let model = h
        .model_for_agent_role(conv)
        .or_else(|| h.selected_model.clone());
    let tool_specs = h.gather_effective_tool_specs_for_role_model(&role, model.as_ref());
    h.prompt_tool_specs.insert(spid.into(), tool_specs);
    let conv = h.agents.get_mut(cid).expect("conversation present");
    if let Some(next_index) = spid
        .rsplit_once('-')
        .and_then(|(_, index)| index.parse::<u64>().ok())
        .map(|index| index.saturating_add(1))
    {
        conv.next_prompt_index = conv.next_prompt_index.max(next_index);
    }
    conv.turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: spid.into(),
    };
    h.agent_routes.insert(agent_id.clone(), cid.clone());
    if let Some(model) = model {
        h.prompt_models.insert(spid.into(), model);
    }
}

/// Pre-seed the per-conversation `ToolsRunning` state for tests that
/// bypass the agent-response path and call tool handlers directly.
fn seed_tools_running(h: &mut Harness, cid: &crate::AgentId, remaining: Vec<ToolCallId>) {
    h.agents
        .get_mut(cid)
        .expect("conversation present")
        .turn_state = AgentTurnState::ToolsRunning {
        remaining_calls: remaining,
    };
}

/// Seed the transcript and turn state as if the assistant had just
/// emitted one or more tool calls for this conversation.
fn seed_assistant_tool_round(h: &mut Harness, cid: &crate::AgentId, calls: &[(&str, &str)]) {
    let agent_id = h
        .agents
        .get(cid)
        .and_then(|conv| conv.agent_id.clone())
        .unwrap_or_else(|| "main".to_owned());
    h.publish_for_agent(
        cid,
        Event::ProviderResponseFinished(ProviderResponseFinished {
            agent_prompt_id: "sp-seeded-tools".into(),
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
            usage: None,
            originator: tau_proto::PromptOriginator::User,
            compaction_original_input_tokens: None,
            compaction_compacted_input_tokens: None,
            backend: None,
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
        agent_prompt_id: "sp-raw".into(),
        agent_id: crate::parse_agent_id("main"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "call-original".into(),
            name: ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: Some(raw_arguments.to_owned()),
            responses_envelope: Some(responses_envelope.clone()),
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    };
    let normalized_calls = vec![NormalizedFinishedToolCall {
        call: AgentToolCall {
            id: "call-normalized".into(),
            name: ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
        },
        background_support: tau_proto::BackgroundSupport::Never,
    }];

    Harness::rewrite_finished_response_tool_call_items(&mut response, &normalized_calls);

    let ContextItem::ToolCall(call) = &response.output_items[0] else {
        panic!("expected rewritten tool call");
    };
    assert_eq!(call.call_id.as_str(), "call-normalized");
    assert_eq!(call.raw_arguments_json.as_deref(), Some(raw_arguments));
    assert_eq!(call.responses_envelope, Some(responses_envelope));
}

/// Pumps the harness event loop until the named tool call's result
/// or error is received and handled. Panics on timeout.
fn drive_harness_until_call_completes(h: &mut Harness, target_call_id: &str) {
    let started = Instant::now();
    loop {
        if started.elapsed() >= Duration::from_secs(3) {
            panic!("timed out waiting for {target_call_id} to complete");
        }
        let event =
            h.rx.recv_timeout(Duration::from_secs(1))
                .expect("tool result should arrive");
        match event {
            HarnessEvent::FromConnection {
                connection_id,
                message,
                ..
            } => {
                let is_target = match message.as_ref() {
                    HarnessInputMessage::Emit(emit) => match emit.event.as_ref() {
                        Event::ToolResult(r) => r.call_id.as_str() == target_call_id,
                        Event::ToolError(e) => e.call_id.as_str() == target_call_id,
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
            HarnessEvent::Command(command) => h.handle_harness_command(command).expect("handle"),
        }
    }
}

fn drive_harness_until_tool_turn_empty(h: &mut Harness) {
    let started = Instant::now();
    loop {
        if h.tool_turn.is_empty() {
            return;
        }
        if started.elapsed() >= Duration::from_secs(3) {
            panic!("timed out waiting for tool turn to empty");
        }
        let event =
            h.rx.recv_timeout(Duration::from_secs(1))
                .expect("tool result should arrive");
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
            HarnessEvent::Command(command) => h.handle_harness_command(command).expect("handle"),
        }
    }
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
    h.agents
        .iter()
        .find_map(|(cid, conv)| {
            matches!(
                &conv.originator,
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
    let mut cursor = crate::event_log::EventLogSeq::new(0);
    loop {
        let entry = h
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
    let mut cursor = crate::event_log::EventLogSeq::new(0);
    let mut seen = 0;
    loop {
        let entry = h
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
    ((*intercepted.event).clone(), intercepted.transient)
}

fn draft_event(text: &str) -> Event {
    Event::UiPromptDraft(UiPromptDraft {
        session_id: "s1".into(),
        target_agent_id: None,
        text: text.to_owned(),
    })
}

mod action;
mod agent_list;
mod dedup;
mod dispatch;
mod format;
mod interception;
mod lifecycle;
mod mode;
mod model;
mod quota;
mod replay;
mod retry;
mod strict_compaction_provider;

use strict_compaction_provider::{
    strict_compaction_provider_harness, validate_closed_tool_timeline,
};
