use std::io::{BufReader, BufWriter};
use std::os::unix::fs::symlink;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use std::{fs, thread};

use tau_proto::{
    CborValue, EventName, EventSelector, HarnessInputMessage, HarnessInputReader,
    HarnessOutputMessage, HarnessOutputWriter, ToolCancelRequest, ToolStarted, ToolUsePayload,
    ToolUseStatus,
};
use tempfile::TempDir;

use super::*;
use crate::agents::{
    discover_agents_files_from, discover_agents_files_from_roots, user_agents_roots,
};
use crate::argument::{
    cbor_map_int, cbor_map_text, optional_argument_bool, optional_argument_text,
};
use crate::dir_lock::DIR_LOCK_TOOL_NAME;
use crate::tools::edit::edit_file as edit_file_with_world;
use crate::tools::find::run_find;
use crate::tools::grep::{RipgrepError, classify_ripgrep_stderr, grep_result_map, run_grep};
use crate::tools::ls::run_ls;
use crate::tools::read::{format_read_range, read_file as read_file_with_world, slice_lines};
use crate::tools::read_image::read_image as read_image_with_world;
use crate::tools::shell::{
    CommandDetails, CommandOutcome, command_details_value, run_command_live,
};
use crate::tools::{
    APPLY_PATCH_TOOL_NAME, EDIT_TOOL_NAME, FIND_TOOL_NAME, GPT_SHELL_TOOL_NAME, LS_TOOL_NAME,
    READ_IMAGE_TOOL_NAME, READ_TOOL_NAME, SHELL_TOOL_NAME,
};
use crate::truncate::{
    MAX_OUTPUT_BYTES, MAX_OUTPUT_LINES, mark_line, truncate_head, truncate_tail,
};

const TEST_SAFE_FILE_READ_LIMIT: u64 = 10 * 1024 * 1024;

#[derive(Clone, Default)]
struct SharedWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("writer").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl SharedWriter {
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().expect("writer").clone()
    }
}

fn read_file(
    arguments: &CborValue,
) -> Result<crate::display::ToolOutput, crate::display::ToolFailure> {
    let mut world = crate::tools::world::ShellWorld::real();
    read_file_with_world(arguments, &mut world)
}

fn edit_file(
    arguments: &CborValue,
) -> Result<crate::display::ToolOutput, crate::display::ToolFailure> {
    let mut world = crate::tools::world::ShellWorld::real();
    edit_file_with_world(arguments, &mut world)
}

fn read_image(
    arguments: &CborValue,
) -> Result<crate::display::ToolOutput, crate::display::ToolFailure> {
    let mut world = crate::tools::world::ShellWorld::real();
    read_image_with_world(arguments, &mut world)
}

/// Ensures the public shell tool path reads a real local image and keeps pixels
/// exclusively in typed provider content while returning safe text metadata.
#[test]
fn read_image_returns_typed_provider_content() {
    let tempdir = TempDir::new().expect("tempdir");
    let path = tempdir.path().join("fixture.png");
    let source = image::DynamicImage::new_rgb8(4, 3);
    let mut bytes = std::io::Cursor::new(Vec::new());
    source
        .write_to(&mut bytes, image::ImageFormat::Png)
        .expect("encode PNG");
    std::fs::write(&path, bytes.into_inner()).expect("write PNG");

    let output = read_image(&cbor_text_map(vec![(
        "path",
        path.to_str().expect("temporary path is UTF-8"),
    )]))
    .expect("read image");
    assert_eq!(output.provider_content.len(), 1);
    let tau_proto::ToolResultContentPart::Image(image) = &output.provider_content[0];
    assert_eq!(image.media_type, tau_proto::ImageMediaType::Png);
    assert_eq!((image.width, image.height), (4, 3));
    assert_eq!(cbor_map_text(&output.result, "mode"), Some("high"));
    assert_eq!(cbor_map_int(&output.result, "patches"), Some(1));
    assert_eq!(
        cbor_map_int(&output.result, "bytes"),
        i64::try_from(image.data.len()).ok()
    );
    assert_eq!(output.display.stats.bytes, Some(image.data.len() as u64));
    assert_eq!(
        cbor_map_value(&output.result, "region"),
        Some(&CborValue::Map(vec![
            (
                CborValue::Text("x".to_owned()),
                CborValue::Integer(0.into())
            ),
            (
                CborValue::Text("y".to_owned()),
                CborValue::Integer(0.into())
            ),
            (
                CborValue::Text("width".to_owned()),
                CborValue::Integer(4.into())
            ),
            (
                CborValue::Text("height".to_owned()),
                CborValue::Integer(3.into())
            ),
        ]))
    );
    let decoded = image::load_from_memory_with_format(&image.data, image::ImageFormat::Png)
        .expect("prepared image remains a decodable PNG");
    assert_eq!((decoded.width(), decoded.height()), (4, 3));
    assert!(matches!(output.result, CborValue::Map(_)));
    assert!(!format!("{:?}", output.result).contains("137, 80, 78, 71"));
}

/// Locks bare-call compatibility against explicit high at a resizing boundary,
/// including canonical bytes, dimensions, and patch accounting.
#[test]
fn read_image_bare_call_matches_explicit_high_for_large_image() {
    let tempdir = TempDir::new().expect("tempdir");
    let path = tempdir.path().join("large.png");
    let source = image::DynamicImage::new_rgb8(1601, 1601);
    let mut bytes = std::io::Cursor::new(Vec::new());
    source
        .write_to(&mut bytes, image::ImageFormat::Png)
        .expect("encode PNG");
    std::fs::write(&path, bytes.into_inner()).expect("write PNG");
    let path = path.display().to_string();
    let bare = read_image(&cbor_text_map(vec![("path", &path)])).expect("bare high");
    let explicit = read_image(&CborValue::Map(vec![
        (CborValue::Text("path".to_owned()), CborValue::Text(path)),
        (
            CborValue::Text("mode".to_owned()),
            CborValue::Text("high".to_owned()),
        ),
    ]))
    .expect("explicit high");
    assert_eq!(
        (provider_image(&bare).width, provider_image(&bare).height),
        (1569, 1569)
    );
    assert_eq!(cbor_map_int(&bare.result, "patches"), Some(2500));
    assert_eq!(provider_image(&bare).data, provider_image(&explicit).data);
    assert_eq!(bare.result, explicit.result);
}

/// Ensures the public tool reports exact transform metadata and directs only
/// the cropped canonical raster into provider content.
#[test]
fn read_image_overview_crop_reports_transform_metadata() {
    let tempdir = TempDir::new().expect("tempdir");
    let path = tempdir.path().join("fixture.png");
    let source = image::DynamicImage::new_rgb8(100, 80);
    let mut bytes = std::io::Cursor::new(Vec::new());
    source
        .write_to(&mut bytes, image::ImageFormat::Png)
        .expect("encode PNG");
    std::fs::write(&path, bytes.into_inner()).expect("write PNG");
    let region = CborValue::Map(vec![
        (
            CborValue::Text("x".to_owned()),
            CborValue::Integer(10.into()),
        ),
        (
            CborValue::Text("y".to_owned()),
            CborValue::Integer(20.into()),
        ),
        (
            CborValue::Text("width".to_owned()),
            CborValue::Integer(64.into()),
        ),
        (
            CborValue::Text("height".to_owned()),
            CborValue::Integer(32.into()),
        ),
    ]);
    let output = read_image(&CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(path.display().to_string()),
        ),
        (
            CborValue::Text("mode".to_owned()),
            CborValue::Text("overview".to_owned()),
        ),
        (CborValue::Text("region".to_owned()), region.clone()),
    ]))
    .expect("read overview crop");

    assert_eq!(cbor_map_text(&output.result, "mode"), Some("overview"));
    assert_eq!(cbor_map_int(&output.result, "source_width"), Some(100));
    assert_eq!(cbor_map_int(&output.result, "source_height"), Some(80));
    assert_eq!(cbor_map_int(&output.result, "oriented_width"), Some(100));
    assert_eq!(cbor_map_int(&output.result, "oriented_height"), Some(80));
    assert_eq!(cbor_map_int(&output.result, "width"), Some(64));
    assert_eq!(cbor_map_int(&output.result, "height"), Some(32));
    assert_eq!(cbor_map_int(&output.result, "patches"), Some(2));
    let result_region = cbor_map_value(&output.result, "region");
    assert_eq!(result_region, Some(&region));
    let tau_proto::ToolResultContentPart::Image(image) = &output.provider_content[0];
    assert_eq!((image.width, image.height), (64, 32));
    assert_eq!(image.detail, tau_proto::ImageDetail::High);
    assert_eq!(
        cbor_map_int(&output.result, "bytes"),
        i64::try_from(image.data.len()).ok()
    );
    assert_eq!(output.display.stats.bytes, Some(image.data.len() as u64));
}

fn cbor_map_value<'a>(map: &'a CborValue, name: &str) -> Option<&'a CborValue> {
    let CborValue::Map(entries) = map else {
        return None;
    };
    entries.iter().find_map(|(key, value)| {
        matches!(key, CborValue::Text(key) if key == name).then_some(value)
    })
}

fn provider_image(output: &crate::display::ToolOutput) -> &tau_proto::ImageContent {
    let tau_proto::ToolResultContentPart::Image(image) = &output.provider_content[0];
    image
}

/// Locks the model-visible experimental mode and complete oriented-region
/// schema.
#[test]
fn read_image_schema_exposes_bounded_overview_and_complete_region() {
    let tool = registered_tool_specs(false)
        .into_iter()
        .find(|tool| tool.name == READ_IMAGE_TOOL_NAME)
        .expect("read_image tool");
    let parameters = tool.parameters.expect("parameters");
    assert_eq!(
        parameters["properties"]["mode"]["enum"],
        serde_json::json!(["high", "overview"])
    );
    assert_eq!(
        parameters["properties"]["region"]["required"],
        serde_json::json!(["x", "y", "width", "height"])
    );
    assert_eq!(
        parameters["properties"]["region"]["additionalProperties"],
        serde_json::json!(false)
    );
}

type TestExtensionReader = EventReader<BufReader<UnixStream>>;
type TestExtensionWriter = EventWriter<BufWriter<UnixStream>>;
type TestExtensionDone = std::sync::mpsc::Receiver<Result<(), String>>;

/// Test-side wrapper around [`HarnessInputReader`] that exposes an
/// `Event`-flavoured API so the existing tests can stay mechanical. Non-event
/// messages are skipped by `read_event`.
struct EventReader<R> {
    inner: HarnessInputReader<R>,
}

impl<R: std::io::Read> EventReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner: HarnessInputReader::new(inner),
        }
    }

    fn read_event(&mut self) -> Result<Option<Event>, tau_proto::DecodeError> {
        loop {
            match self.inner.read_message()? {
                None => return Ok(None),
                Some(HarnessInputMessage::Emit(emit)) => match *emit.event {
                    Event::ToolProgressReported(progress)
                        if progress.message.is_none()
                            && progress.display.is_some()
                            && progress.tool_name != SHELL_TOOL_NAME
                            && progress.tool_name != GPT_SHELL_TOOL_NAME =>
                    {
                        continue;
                    }
                    event => return Ok(Some(event)),
                },
                Some(_) => continue,
            }
        }
    }

    fn read_message(&mut self) -> Result<Option<HarnessInputMessage>, tau_proto::DecodeError> {
        loop {
            match self.inner.read_message()? {
                None => return Ok(None),
                Some(HarnessInputMessage::Emit(_)) => continue,
                Some(message) => return Ok(Some(message)),
            }
        }
    }

    fn read_raw_message(&mut self) -> Result<Option<HarnessInputMessage>, tau_proto::DecodeError> {
        self.inner.read_message()
    }
}

/// Test-side wrapper around [`HarnessOutputWriter`] that accepts `Event`
/// directly.
struct EventWriter<W> {
    inner: HarnessOutputWriter<W>,
}

impl<W: std::io::Write> EventWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner: HarnessOutputWriter::new(inner),
        }
    }

    fn write_event(&mut self, event: &Event) -> Result<(), tau_proto::EncodeError> {
        self.inner
            .write_message(&HarnessOutputMessage::deliver(event.clone()))
    }

    fn write_frame(&mut self, frame: &HarnessOutputMessage) -> Result<(), tau_proto::EncodeError> {
        self.inner.write_message(frame)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Build a disconnect output message for tests that previously sent
/// `Event::LifecycleDisconnect`.
fn disconnect_frame(reason: Option<String>) -> HarnessOutputMessage {
    HarnessOutputMessage::Disconnect(tau_proto::Disconnect { reason })
}

fn cbor_int_field(value: &CborValue, key: &str) -> Option<i128> {
    match value {
        CborValue::Map(entries) => entries.iter().find_map(|(k, v)| match (k, v) {
            (CborValue::Text(k), CborValue::Integer(n)) if k == key => Some((*n).into()),
            _ => None,
        }),
        _ => None,
    }
}

fn cbor_bool_field(value: &CborValue, key: &str) -> Option<bool> {
    match value {
        CborValue::Map(entries) => entries.iter().find_map(|(k, v)| match (k, v) {
            (CborValue::Text(k), CborValue::Bool(n)) if k == key => Some(*n),
            _ => None,
        }),
        _ => None,
    }
}

fn cbor_map_field<'a>(value: &'a CborValue, key: &str) -> Option<&'a CborValue> {
    match value {
        CborValue::Map(entries) => entries.iter().find_map(|(k, v)| match k {
            CborValue::Text(k) if k == key => Some(v),
            _ => None,
        }),
        _ => None,
    }
}

fn spawn_extension() -> (TestExtensionReader, TestExtensionWriter) {
    let (reader, writer, _done_rx) = spawn_extension_with_exit();
    (reader, writer)
}

fn spawn_extension_with_exit() -> (TestExtensionReader, TestExtensionWriter, TestExtensionDone) {
    spawn_extension_with_exit_and_prefix(None)
}

fn spawn_extension_with_exit_and_prefix(
    tool_prefix: Option<tau_proto::ToolNamePrefix>,
) -> (TestExtensionReader, TestExtensionWriter, TestExtensionDone) {
    let (runtime_stream, harness_stream) = UnixStream::pair().expect("stream pair should open");
    let reader_stream = runtime_stream
        .try_clone()
        .expect("runtime reader clone should succeed");
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let result = run_impl(reader_stream, runtime_stream)
            .map_err(|error| format!("extension should run: {error}"));
        let _ = done_tx.send(result);
    });
    let reader = EventReader::new(BufReader::new(
        harness_stream
            .try_clone()
            .expect("harness reader clone should succeed"),
    ));
    let mut writer = EventWriter::new(BufWriter::new(harness_stream));
    writer
        .write_frame(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix,
            instance_name: tau_proto::ExtensionName::new("test-extension"),
            config: CborValue::Map(Vec::new()),
            state_dir: None,
            secrets: Default::default(),
        }))
        .expect("write initial configure");
    writer.flush().expect("flush initial configure");
    (reader, writer, done_rx)
}

/// Static dispatch and dynamic registration refreshes both preserve a
/// configured wire namespace while matching logical Shell tool names
/// internally.
#[test]
fn prefixed_shell_dispatch_and_dir_lock_refresh_use_wire_names() {
    let prefix = tau_proto::ToolNamePrefix::parse("work").expect("prefix");
    let (mut reader, mut writer, done_rx) =
        spawn_extension_with_exit_and_prefix(Some(prefix.clone()));
    drain_startup(&mut reader);

    writer
        .write_frame(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: Some(prefix),
            instance_name: tau_proto::ExtensionName::new("test-extension"),
            config: cbor_map(vec![(
                "dir_lock",
                cbor_map(vec![("enable", CborValue::Bool(true))]),
            )]),
            state_dir: None,
            secrets: Default::default(),
        }))
        .expect("enable dir_lock");
    writer.flush().expect("flush config");
    let refreshed = reader.read_event().expect("read").expect("refresh");
    let Event::ToolRegistrationDeclared(register) = refreshed else {
        panic!("expected ToolRegistrationDeclared, got {refreshed:?}");
    };
    assert_eq!(register.tool.name.as_str(), "work_dir_lock");

    writer
        .write_event(&tool_started(
            "prefixed-echo",
            "work_echo",
            cbor_map(vec![("text", CborValue::Text("hello".to_owned()))]),
            "agent-a",
        ))
        .expect("invoke");
    writer.flush().expect("flush invocation");
    let result = loop {
        let event = reader.read_event().expect("read").expect("result");
        match event {
            Event::ToolResult(result) => break result,
            Event::AgentMetadataSet(metadata) => {
                panic!("workdir read emitted metadata: {metadata:?}")
            }
            Event::AgentUserMessageInjected(notice) => {
                panic!("workdir read emitted persistent-change notice: {notice:?}")
            }
            _ => {}
        }
    };
    assert_eq!(result.tool_name.as_str(), "work_echo");

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush disconnect");
    done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("extension exit")
        .expect("extension ok");
}

fn cbor_map(entries: Vec<(&str, CborValue)>) -> CborValue {
    CborValue::Map(
        entries
            .into_iter()
            .map(|(key, value)| (CborValue::Text(key.to_owned()), value))
            .collect(),
    )
}

fn cbor_text_map(entries: Vec<(&str, &str)>) -> CborValue {
    cbor_map(
        entries
            .into_iter()
            .map(|(key, value)| (key, CborValue::Text(value.to_owned())))
            .collect(),
    )
}

fn edit_arguments(path: &Path, edits: Vec<CborValue>) -> CborValue {
    cbor_map(vec![
        ("path", CborValue::Text(path.display().to_string())),
        ("edits", CborValue::Array(edits)),
    ])
}

fn line_edit(start_line: i64, end_line: i64, new_text: &str) -> CborValue {
    cbor_map(vec![
        ("start_line", CborValue::Integer(start_line.into())),
        (
            "end_line_exclusive",
            CborValue::Integer((end_line + 1).into()),
        ),
        ("newText", CborValue::Text(new_text.to_owned())),
    ])
}

fn context_line_edit(
    start_line: i64,
    end_line: i64,
    new_text: &str,
    context_line: &str,
) -> CborValue {
    let end_line_exclusive = end_line + 1;
    cbor_map(vec![
        ("start_line", CborValue::Integer(start_line.into())),
        (
            "end_line_exclusive",
            CborValue::Integer(end_line_exclusive.into()),
        ),
        ("newText", CborValue::Text(new_text.to_owned())),
        ("context_line", CborValue::Text(context_line.to_owned())),
    ])
}

fn context_half_open_edit(
    start_line: i64,
    end_line_exclusive: i64,
    new_text: &str,
    context_line: &str,
) -> CborValue {
    cbor_map(vec![
        ("start_line", CborValue::Integer(start_line.into())),
        (
            "end_line_exclusive",
            CborValue::Integer(end_line_exclusive.into()),
        ),
        ("newText", CborValue::Text(new_text.to_owned())),
        ("context_line", CborValue::Text(context_line.to_owned())),
    ])
}

fn read_range(start_line: i64, end_line: i64) -> CborValue {
    cbor_map(vec![
        ("start_line", CborValue::Integer(start_line.into())),
        ("end_line", CborValue::Integer(end_line.into())),
    ])
}
fn send_dir_lock_config(writer: &mut EventWriter<BufWriter<UnixStream>>, enable: bool) {
    writer
        .write_frame(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::new("test-extension"),
            config: cbor_map(vec![(
                "dir_lock",
                cbor_map(vec![("enable", CborValue::Bool(enable))]),
            )]),
            state_dir: None,
            secrets: Default::default(),
        }))
        .expect("configure dir_lock");
    writer.flush().expect("flush config");
}

fn tool_started(call_id: &str, tool_name: &str, arguments: CborValue, agent_id: &str) -> Event {
    Event::ToolStarted(ToolStarted {
        call_id: tau_proto::ToolCallId::new(call_id),
        tool_name: tau_proto::ToolName::new(tool_name),
        arguments,
        agent_id: tau_proto::AgentId::parse(agent_id).expect("agent id"),
        originator: tau_proto::PromptOriginator::User,
    })
}

fn action_invoke(invocation_id: &str, action_id: &str, directory: &str) -> Event {
    Event::ActionInvoke(tau_proto::ActionInvoke {
        invocation_id: invocation_id.into(),
        session_id: "session-1".into(),
        extension_name: "tau-ext-shell".into(),
        instance_id: 0.into(),
        action_id: action_id.to_owned(),
        raw_line: format!("/shell-dir-force-unlock {directory}"),
        argv: vec![directory.to_owned()],
        arguments: cbor_text_map(vec![("directory", directory)]),
    })
}

fn ui_shell_command(command_id: &str, command: &str) -> Event {
    Event::UiShellCommand(tau_proto::UiShellCommand {
        session_id: "session-1".into(),
        command_id: command_id.into(),
        command: command.to_owned(),
        include_in_context: true,
        target_agent_id: None,
    })
}

/// A targeted `!`/`!!` command snapshots that agent's committed instance
/// workdir.
#[test]
fn targeted_user_shell_runs_from_agent_workdir() {
    let workdir = TempDir::new().expect("workdir");
    let agent_id = tau_proto::AgentId::parse("agent-user-shell-workdir").expect("agent id");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    writer
        .write_event(&Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
            agent_id: agent_id.clone(),
            key: tau_proto::AgentMetadataKey::new("ext_test-extension_cwd"),
            value: CborValue::Text(workdir.path().display().to_string()),
            mutation_id: None,
            inheritable: true,
        }))
        .expect("seed workdir");
    writer.flush().expect("flush seed");
    let _ = reader.read_event().expect("context").expect("context");
    writer
        .write_event(&Event::UiShellCommand(tau_proto::UiShellCommand {
            session_id: "session-1".into(),
            command_id: "ui-targeted-pwd".into(),
            command: "pwd".to_owned(),
            include_in_context: false,
            target_agent_id: Some(agent_id),
        }))
        .expect("targeted user shell");
    writer.flush().expect("flush command");
    let finished = wait_for_user_shell_finished(&mut reader, "ui-targeted-pwd");
    assert!(
        finished
            .output
            .contains(workdir.path().to_str().expect("UTF-8 path"))
    );
}

/// Consumes startup events (tool registration declarations). The
/// hello/subscribe/ready messages are filtered out by the test-side
/// `EventReader` wrapper.
fn drain_startup(reader: &mut EventReader<BufReader<UnixStream>>) {
    for expected in [
        EventName::TOOL_REGISTRATION_DECLARED,                  // echo
        EventName::TOOL_REGISTRATION_DECLARED,                  // read
        EventName::TOOL_REGISTRATION_DECLARED,                  // read_image
        EventName::TOOL_REGISTRATION_DECLARED,                  // edit
        EventName::TOOL_REGISTRATION_DECLARED,                  // apply_patch
        EventName::TOOL_REGISTRATION_DECLARED,                  // dir_lock
        EventName::TOOL_REGISTRATION_DECLARED,                  // grep
        EventName::TOOL_REGISTRATION_DECLARED,                  // find
        EventName::TOOL_REGISTRATION_DECLARED,                  // ls
        EventName::TOOL_REGISTRATION_DECLARED,                  // cd
        EventName::TOOL_REGISTRATION_DECLARED,                  // shell
        EventName::TOOL_REGISTRATION_DECLARED,                  // gpt_shell
        EventName::EXTENSION_CONTEXT_PROVIDER_REGISTER,         // shell cwd context
        EventName::EXTENSION_SESSION_CONTEXT_PROVIDER_REGISTER, // session skills/AGENTS.md
        EventName::EXTENSION_PROMPT_FRAGMENT_PUBLISH,           // shell.cwd
        EventName::ACTION_SCHEMA_PUBLISHED,                     // shell-dir-force-unlock
    ] {
        let event = reader
            .read_event()
            .expect("read")
            .expect("startup event should arrive");
        assert_eq!(event.name(), expected);
    }
}

fn wait_for_user_shell_progress(reader: &mut TestExtensionReader, command_id: &str, text: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for user shell progress"
        );
        match reader.read_event().expect("read progress") {
            Some(Event::ShellCommandProgress(progress))
                if progress.command_id == command_id && progress.chunk.contains(text) =>
            {
                return;
            }
            Some(_) => {}
            None => panic!("extension closed before user shell progress"),
        }
    }
}

fn wait_for_user_shell_finished(
    reader: &mut TestExtensionReader,
    command_id: &str,
) -> tau_proto::ShellCommandFinished {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for user shell finish"
        );
        match reader.read_event().expect("read finish") {
            Some(Event::ShellCommandFinished(finished)) if finished.command_id == command_id => {
                return finished;
            }
            Some(_) => {}
            None => panic!("extension closed before user shell finish"),
        }
    }
}

#[test]
fn startup_declares_exact_shell_subscriptions_and_ready_after_publications() {
    let (mut reader, mut writer) = spawn_extension();

    let hello = reader
        .read_raw_message()
        .expect("read hello")
        .expect("hello frame");
    assert!(matches!(hello, HarnessInputMessage::Hello(_)));

    let subscribe = reader
        .read_raw_message()
        .expect("read subscribe")
        .expect("subscribe frame");
    let HarnessInputMessage::Subscribe(subscribe) = subscribe else {
        panic!("expected Subscribe after Hello");
    };
    assert_eq!(
        subscribe.live_selectors,
        [
            EventSelector::Exact(EventName::TOOL_STARTED),
            EventSelector::Exact(EventName::TOOL_CANCEL_REQUEST),
            EventSelector::Exact(EventName::ACTION_INVOKE),
            EventSelector::Exact(EventName::SESSION_STARTED),
            EventSelector::Exact(EventName::SESSION_AGENT_LOADED),
            EventSelector::Exact(EventName::SESSION_AGENT_UNLOADED),
            EventSelector::Exact(EventName::AGENT_REPLAY_COMPLETE),
            EventSelector::Exact(EventName::AGENT_METADATA_SET),
            EventSelector::Exact(EventName::AGENT_METADATA_UNSET),
            EventSelector::Exact(EventName::SESSION_SHUTDOWN),
            EventSelector::Exact(EventName::AGENT_START_ACCEPTED),
            EventSelector::Exact(EventName::AGENT_START_RESULT),
            EventSelector::Exact(EventName::UI_SHELL_COMMAND),
        ]
    );
    assert_eq!(
        subscribe.historical_selectors,
        [
            EventSelector::Exact(EventName::SESSION_STARTED),
            EventSelector::Exact(EventName::SESSION_AGENT_LOADED),
            EventSelector::Exact(EventName::SESSION_AGENT_UNLOADED),
            EventSelector::Exact(EventName::AGENT_METADATA_SET),
            EventSelector::Exact(EventName::AGENT_METADATA_UNSET),
        ]
    );

    for expected_tool in [
        ECHO_TOOL_NAME,
        READ_TOOL_NAME,
        READ_IMAGE_TOOL_NAME,
        EDIT_TOOL_NAME,
        APPLY_PATCH_TOOL_NAME,
        DIR_LOCK_TOOL_NAME,
        GREP_TOOL_NAME,
        FIND_TOOL_NAME,
        LS_TOOL_NAME,
        WORKDIR_TOOL_NAME,
        SHELL_TOOL_NAME,
        GPT_SHELL_TOOL_NAME,
    ] {
        let message = reader
            .read_raw_message()
            .expect("read startup tool")
            .expect("startup tool frame");
        let HarnessInputMessage::Emit(emit) = message else {
            panic!("expected tool registration before Ready, got {message:?}");
        };
        let Event::ToolRegistrationDeclared(register) = *emit.event else {
            panic!(
                "expected tool registration before Ready, got {:?}",
                emit.event
            );
        };
        assert_eq!(register.tool.name, expected_tool);
    }

    for expected in [
        EventName::EXTENSION_CONTEXT_PROVIDER_REGISTER,
        EventName::EXTENSION_SESSION_CONTEXT_PROVIDER_REGISTER,
        EventName::EXTENSION_PROMPT_FRAGMENT_PUBLISH,
        EventName::ACTION_SCHEMA_PUBLISHED,
    ] {
        let message = reader
            .read_raw_message()
            .expect("read startup publication")
            .expect("startup publication frame");
        let HarnessInputMessage::Emit(emit) = message else {
            panic!("expected startup Emit before Ready, got {message:?}");
        };
        assert_eq!(emit.event.name(), expected);
    }

    let ready = reader
        .read_raw_message()
        .expect("read ready")
        .expect("ready frame");
    assert!(
        matches!(ready, HarnessInputMessage::Ready(_)),
        "Ready must follow all startup publications, got {ready:?}"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn replayed_shell_tool_delivery_does_not_run_tool() {
    let tempdir = TempDir::new().expect("tempdir");
    let path = tempdir.path().join("replay-must-not-write.txt");
    let (mut reader, mut writer, done_rx) = spawn_extension_with_exit();
    drain_startup(&mut reader);

    let Event::ToolStarted(invoke) = tool_started(
        "replayed-edit",
        EDIT_TOOL_NAME,
        edit_arguments(&path, vec![context_half_open_edit(1, 1, "created\n", "")]),
        "agent-a",
    ) else {
        unreachable!();
    };
    writer
        .write_frame(&HarnessOutputMessage::deliver_replay(
            tau_proto::UnixMicros::new(1),
            Event::ToolStarted(invoke),
        ))
        .expect("replayed edit");
    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");

    done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("extension exit")
        .expect("extension ok");
    assert!(
        !path.exists(),
        "replayed tool delivery must not mutate files"
    );
}

/// Covers startup registration defaults, groups, names, and core tool schemas
/// through the real extension protocol publication path.
#[test]
fn startup_registers_echo_disabled_by_default_and_gpt_shell_visible_name() {
    let (mut reader, mut writer) = spawn_extension();

    let mut found_echo_disabled = false;
    let mut found_gpt_shell_visible_name = false;
    let mut found_read_schema = false;
    let mut found_read_image_foreground_only = false;
    let mut found_edit_schema = false;
    let mut found_write = false;
    for _ in 0..12 {
        let event = reader
            .read_event()
            .expect("read")
            .expect("startup event should arrive");
        let Event::ToolRegistrationDeclared(register) = event else {
            continue;
        };
        if register.tool.name == ECHO_TOOL_NAME {
            assert!(!register.tool.enabled_by_default);
            assert_eq!(
                register
                    .tool_group
                    .as_ref()
                    .map(|group| group.name.as_str()),
                Some("test")
            );
            found_echo_disabled = true;
        }
        if register.tool.name == GPT_SHELL_TOOL_NAME {
            assert_eq!(
                register.tool.model_visible_name,
                Some(tau_proto::ToolName::new("shell_command"))
            );
            found_gpt_shell_visible_name = true;
        }
        if register.tool.name == READ_TOOL_NAME {
            assert_eq!(
                register
                    .tool_group
                    .as_ref()
                    .map(|group| group.name.as_str()),
                Some("shell")
            );
            let tags = register
                .tool
                .tags
                .iter()
                .map(|tag| tag.as_str())
                .collect::<Vec<_>>();
            assert_eq!(tags, vec!["shell:read"]);
            let parameters = register.tool.parameters.as_ref().expect("parameters");
            let range_item = &parameters["properties"]["ranges"]["items"];
            assert_eq!(
                range_item["required"],
                serde_json::json!(["start_line", "end_line"])
            );
            found_read_schema = true;
        }
        if register.tool.name == READ_IMAGE_TOOL_NAME {
            assert_eq!(
                register.tool.description.as_deref(),
                Some("Read one local image for visual inspection.")
            );
            assert_eq!(
                register.tool.background_support,
                Some(tau_proto::BackgroundSupport::Never)
            );
            found_read_image_foreground_only = true;
        }
        if register.tool.name == EDIT_TOOL_NAME {
            let parameters = register.tool.parameters.as_ref().expect("parameters");
            let edit_item = &parameters["properties"]["edits"]["items"];
            assert_eq!(
                edit_item["required"],
                serde_json::json!([
                    "start_line",
                    "end_line_exclusive",
                    "newText",
                    "context_line"
                ])
            );
            assert_eq!(
                edit_item["properties"]["after_line"],
                serde_json::Value::Null
            );
            assert_eq!(
                edit_item["properties"]["before_line"],
                serde_json::Value::Null
            );
            assert_eq!(edit_item["properties"]["end_line"], serde_json::Value::Null);
            assert_eq!(edit_item["properties"]["oldText"], serde_json::Value::Null);
            assert_eq!(
                edit_item["properties"]["context_line"]["type"],
                serde_json::json!("string")
            );
            found_edit_schema = true;
        }
        if register.tool.name == "write" {
            found_write = true;
        }
    }
    assert!(found_echo_disabled, "expected echo tool registration");
    assert!(
        found_gpt_shell_visible_name,
        "expected gpt_shell tool registration"
    );
    assert!(found_read_schema, "expected multi-range read schema");
    assert!(
        found_read_image_foreground_only,
        "expected foreground-only read_image registration"
    );
    assert!(found_edit_schema, "expected line-oriented edit schema");
    assert!(!found_write, "write tool should not be registered");

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn startup_registers_schema_valid_tool_examples() {
    // Provider-owned examples are shown after failed calls, so every startup
    // registration must keep them aligned with that tool's current schema.
    let (mut reader, mut writer) = spawn_extension();

    let mut checked = Vec::new();
    for _ in 0..12 {
        let event = reader
            .read_event()
            .expect("read")
            .expect("startup event should arrive");
        let Event::ToolRegistrationDeclared(register) = event else {
            continue;
        };
        tau_core::validate_tool_examples(&register.tool)
            .unwrap_or_else(|error| panic!("invalid examples for {}: {error}", register.tool.name));
        checked.push(register.tool.name.to_string());
    }
    for tool_name in [
        READ_TOOL_NAME,
        EDIT_TOOL_NAME,
        APPLY_PATCH_TOOL_NAME,
        DIR_LOCK_TOOL_NAME,
        GREP_TOOL_NAME,
        FIND_TOOL_NAME,
        LS_TOOL_NAME,
        WORKDIR_TOOL_NAME,
        SHELL_TOOL_NAME,
        GPT_SHELL_TOOL_NAME,
    ] {
        assert!(
            checked.iter().any(|name| name == tool_name),
            "expected to validate examples for {tool_name}"
        );
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn shell_tool_cancel_request_stops_running_command_quickly() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let call_id = tau_proto::ToolCallId::new("cancel-shell-call");
    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: call_id.clone(),
            tool_name: tau_proto::ToolName::new(SHELL_TOOL_NAME),
            arguments: CborValue::Map(vec![(
                CborValue::Text("command".to_owned()),
                CborValue::Text("sleep 30".to_owned()),
            )]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke shell");
    writer.flush().expect("flush invoke");

    let started = Instant::now();
    loop {
        assert!(started.elapsed() < Duration::from_secs(2));
        match reader.read_event().expect("read") {
            Some(Event::ToolProgressReported(progress)) if progress.call_id == call_id => break,
            Some(_) => continue,
            None => panic!("extension closed before shell started"),
        }
    }

    writer
        .write_event(&Event::ToolCancelRequest(ToolCancelRequest {
            target_call_id: call_id.clone(),
        }))
        .expect("cancel shell");
    writer.flush().expect("flush cancel");

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        assert!(Instant::now() < deadline, "shell cancellation timed out");
        match reader.read_event().expect("read") {
            Some(Event::ToolCancelled(cancelled)) if cancelled.call_id == call_id => break,
            Some(_) => continue,
            None => panic!("extension closed before cancellation"),
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn disconnect_cancels_active_user_shell_command_before_scheduler_join() {
    let (mut reader, mut writer, done_rx) = spawn_extension_with_exit();
    drain_startup(&mut reader);

    writer
        .write_event(&ui_shell_command(
            "ui-sleep-disconnect",
            "printf started; sleep 30",
        ))
        .expect("ui shell");
    writer.flush().expect("flush ui shell");
    wait_for_user_shell_progress(&mut reader, "ui-sleep-disconnect", "started");

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush disconnect");

    done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("extension should exit after cancelling UI shell command")
        .expect("extension should exit cleanly");
}

#[test]
fn session_shutdown_cancels_active_user_shell_and_keeps_scheduler_usable() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&ui_shell_command(
            "ui-sleep-session",
            "printf started; sleep 30",
        ))
        .expect("ui shell");
    writer.flush().expect("flush ui shell");
    wait_for_user_shell_progress(&mut reader, "ui-sleep-session", "started");

    writer
        .write_event(&Event::SessionShutdown(tau_proto::SessionShutdown {
            session_id: "session-1".into(),
        }))
        .expect("session shutdown");
    writer.flush().expect("flush session shutdown");

    let cancelled = wait_for_user_shell_finished(&mut reader, "ui-sleep-session");
    assert!(
        cancelled.cancelled,
        "session shutdown should cancel running UI shell command: {cancelled:?}"
    );

    writer
        .write_event(&ui_shell_command("ui-after-session", "printf after"))
        .expect("second ui shell");
    writer.flush().expect("flush second ui shell");

    let finished = wait_for_user_shell_finished(&mut reader, "ui-after-session");
    assert_eq!(finished.exit_code, Some(0));
    assert!(!finished.cancelled);
    assert!(finished.output.contains("after"));

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush disconnect");
}

#[test]
fn startup_registers_dir_lock_disabled_by_default() {
    let (mut reader, mut writer) = spawn_extension();

    let mut found_dir_lock = false;
    for _ in 0..12 {
        let event = reader
            .read_event()
            .expect("read")
            .expect("startup event should arrive");
        let Event::ToolRegistrationDeclared(register) = event else {
            continue;
        };
        if register.tool.name == DIR_LOCK_TOOL_NAME {
            assert!(!register.tool.enabled_by_default);
            assert!(
                register.tool.tags.is_empty(),
                "disabled dir_lock must not be re-enabled by shell tag policy"
            );
            found_dir_lock = true;
        }
    }
    assert!(found_dir_lock, "expected dir_lock registration");

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn startup_publishes_shell_dir_force_unlock_action() {
    let (mut reader, mut writer) = spawn_extension();

    let mut found_schema = false;
    for _ in 0..16 {
        let event = reader
            .read_event()
            .expect("read")
            .expect("startup event should arrive");
        let Event::ActionSchemaPublished(published) = event else {
            continue;
        };
        published.schema.validate().expect("schema validates");
        let root = published
            .schema
            .roots
            .iter()
            .find(|root| root.name == "/shell-dir-force-unlock")
            .expect("force unlock action root");
        assert_eq!(
            root.action_id.as_deref(),
            Some(SHELL_DIR_FORCE_UNLOCK_ACTION_ID)
        );
        assert_eq!(root.args.len(), 1);
        assert_eq!(root.args[0].name, "directory");
        assert!(matches!(
            root.args[0].kind,
            tau_actions::ActionArgKind::RestString
        ));
        found_schema = true;
    }
    assert!(found_schema, "expected shell action schema");

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn shell_dir_force_unlock_releases_overlapping_manual_lock() {
    let tempdir = TempDir::new().expect("tempdir");
    let lock_dir = tempdir.path().join("root");
    let child_dir = lock_dir.join("child");
    fs::create_dir_all(&child_dir).expect("child dir");
    let edit_path = child_dir.join("file.txt");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    send_dir_lock_config(&mut writer, true);

    writer
        .write_event(&tool_started(
            "lock-root",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &lock_dir.display().to_string()),
            ]),
            "agent-a",
        ))
        .expect("dir_lock update");
    writer.flush().expect("flush lock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "lock-root" => break,
            Some(_) => continue,
            None => panic!("extension closed before lock result"),
        }
    }

    writer
        .write_event(&action_invoke(
            "force-unlock-1",
            SHELL_DIR_FORCE_UNLOCK_ACTION_ID,
            &child_dir.display().to_string(),
        ))
        .expect("force unlock");
    writer.flush().expect("flush force unlock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ActionResult(result))
                if result.invocation_id.as_str() == "force-unlock-1" =>
            {
                let tau_proto::ActionOutput::Text { text } = result.output else {
                    panic!("expected text output");
                };
                assert!(text.contains("Force-unlocked 1 manual directory lock"));
                assert!(text.contains("owner=agent-a"));
                assert!(text.contains(&lock_dir.display().to_string()));
                break;
            }
            Some(Event::ActionError(error)) if error.invocation_id.as_str() == "force-unlock-1" => {
                panic!("force unlock failed: {}", error.message);
            }
            Some(_) => continue,
            None => panic!("extension closed before force unlock result"),
        }
    }

    writer
        .write_event(&tool_started(
            "edit-after-force-unlock",
            EDIT_TOOL_NAME,
            edit_arguments(&edit_path, vec![context_half_open_edit(1, 1, "hello", "")]),
            "agent-b",
        ))
        .expect("edit");
    writer.flush().expect("flush edit");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result))
                if result.call_id.as_str() == "edit-after-force-unlock" =>
            {
                break;
            }
            Some(Event::ToolProgressReported(progress))
                if progress.call_id.as_str() == "edit-after-force-unlock" =>
            {
                panic!("edit still waited after force unlock: {progress:?}");
            }
            Some(_) => continue,
            None => panic!("extension closed before edit result"),
        }
    }
    assert_eq!(
        fs::read_to_string(&edit_path).expect("edited file"),
        "hello\n"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn dir_lock_config_re_registers_tool_enabled_when_config_true() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    send_dir_lock_config(&mut writer, true);

    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolRegistrationDeclared(register))
                if register.tool.name == DIR_LOCK_TOOL_NAME =>
            {
                assert!(register.tool.enabled_by_default);
                assert!(
                    register
                        .tool
                        .tags
                        .iter()
                        .any(|tag| tag.as_str() == "shell:lock")
                );
                break;
            }
            Some(_) => continue,
            None => panic!("extension closed before dir_lock re-registration"),
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// Initial configuration refreshes override static default declarations before
/// Ready, leaving the effective dir_lock registration enabled and tagged.
#[test]
fn initial_dir_lock_override_is_final_before_ready() {
    let mut input = Vec::new();
    let mut input_writer = tau_proto::HarnessOutputWriter::new(&mut input);
    input_writer
        .write_message(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::new("test-extension"),
            config: cbor_map(vec![(
                "dir_lock",
                cbor_map(vec![("enable", CborValue::Bool(true))]),
            )]),
            state_dir: None,
            secrets: Default::default(),
        }))
        .expect("configure");
    input_writer.flush().expect("flush input");
    let output = SharedWriter::default();
    let written = output.clone();
    run_impl(std::io::Cursor::new(input), output).expect("run shell");

    let mut reader = tau_proto::HarnessInputReader::new(std::io::Cursor::new(written.bytes()));
    let mut frames = Vec::new();
    while let Some(frame) = reader.read_message().expect("frame") {
        frames.push(frame);
    }
    let registrations = frames
        .iter()
        .enumerate()
        .filter_map(|(index, frame)| match frame {
            HarnessInputMessage::Emit(emit) => match emit.event.as_ref() {
                Event::ToolRegistrationDeclared(register)
                    if register.tool.name.as_str() == DIR_LOCK_TOOL_NAME =>
                {
                    Some((index, &register.tool))
                }
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    let (override_index, final_tool) = registrations.last().expect("dir_lock registration");
    assert!(final_tool.enabled_by_default);
    assert!(
        final_tool
            .tags
            .iter()
            .any(|tag| tag.as_str() == "shell:lock")
    );
    let ready_index = frames
        .iter()
        .position(|frame| matches!(frame, HarnessInputMessage::Ready(_)))
        .expect("Ready");
    assert!(*override_index < ready_index);
}

/// Ensures the dispatch path returns a clear bounded-backpressure ToolError
/// when scheduler admission rejects excess work.
#[test]
fn schedule_tool_started_reports_queue_full_error() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let output = Output::channel(tx);
    let scheduler = WorkScheduler::new(
        output.clone(),
        crate::scheduler::SchedulerConfig {
            total_limit: 0,
            control_workers: 0,
            user_workers: 0,
            cheap_workers: 0,
            general_workers: 0,
            ..Default::default()
        },
    );
    let Event::ToolStarted(invoke) = tool_started(
        "queue-full-read",
        READ_TOOL_NAME,
        cbor_text_map(vec![("path", "Cargo.toml")]),
        "agent-a",
    ) else {
        panic!("expected tool started");
    };
    let local_tool_name = invoke.tool_name.clone();

    let Err(error) = schedule_tool_started(
        (invoke, &local_tool_name),
        &scheduler,
        &output,
        ExtConfig::default(),
        DirLockManager::default(),
        Arc::new(Mutex::new(HashMap::new())),
        CwdState::new(),
    ) else {
        panic!("queue-full call should be rejected");
    };
    let (returned, failure) = *error;

    assert_eq!(returned.call_id.as_str(), "queue-full-read");
    assert!(failure.message.contains("queue limit is 0"));
}

/// Ensures a model tool canceled while queued by the scheduler never reaches
/// the mutation implementation.
#[test]
fn schedule_tool_started_cancel_before_start_prevents_mutation() {
    let tempdir = TempDir::new().expect("tempdir");
    let edit_path = tempdir.path().join("queued-edit.txt");
    fs::write(&edit_path, "old\n").expect("initial file");
    let (tx, rx) = std::sync::mpsc::channel();
    let output = Output::channel(tx);
    let scheduler = WorkScheduler::new(
        output.clone(),
        crate::scheduler::SchedulerConfig {
            control_workers: 0,
            user_workers: 0,
            cheap_workers: 0,
            general_workers: 0,
            ..Default::default()
        },
    );
    let Event::ToolStarted(invoke) = tool_started(
        "queued-edit",
        EDIT_TOOL_NAME,
        edit_arguments(&edit_path, vec![line_edit(1, 2, "new\n")]),
        "agent-a",
    ) else {
        panic!("expected tool started");
    };
    let call_id = invoke.call_id.clone();
    let local_tool_name = invoke.tool_name.clone();

    schedule_tool_started(
        (invoke, &local_tool_name),
        &scheduler,
        &output,
        ExtConfig::default(),
        DirLockManager::default(),
        Arc::new(Mutex::new(HashMap::new())),
        CwdState::new(),
    )
    .expect("edit queued");
    assert!(scheduler.cancel_queued_call(&call_id));

    let HarnessInputMessage::Emit(emit) = rx.recv().expect("cancel event") else {
        panic!("expected emit");
    };
    let Event::ToolCancelled(cancelled) = *emit.event else {
        panic!("expected ToolCancelled");
    };
    assert_eq!(cancelled.call_id, call_id);
    assert_eq!(fs::read_to_string(&edit_path).expect("file"), "old\n");
}

#[test]
fn dir_lock_tool_can_be_disabled_by_config() {
    let tempdir = TempDir::new().expect("tempdir");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    send_dir_lock_config(&mut writer, false);

    writer
        .write_event(&tool_started(
            "lock-disabled",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &tempdir.path().display().to_string()),
            ]),
            "agent-a",
        ))
        .expect("dir_lock update");
    writer.flush().expect("flush invoke");

    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolError(error)) if error.call_id.as_str() == "lock-disabled" => {
                assert!(error.message.contains("dir_lock is disabled"));
                break;
            }
            Some(_) => continue,
            None => panic!("extension closed before dir_lock error"),
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// Keeps the concise model-facing purpose text separate from configuration and
/// argument details recorded elsewhere in the schema.
#[test]
fn dir_lock_tool_has_concise_model_facing_description() {
    assert_eq!(
        dir_lock_tool_spec(false).description.as_deref(),
        Some(
            "Lock or unlock a directory and its contents for updates. Waits for the lock when \
             necessary."
        )
    );
}

#[test]
fn dir_lock_blocks_conflicting_edit_until_unlock() {
    let tempdir = TempDir::new().expect("tempdir");
    let lock_dir = tempdir.path().to_path_buf();
    let edit_path = lock_dir.join("file.txt");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    send_dir_lock_config(&mut writer, true);
    writer
        .write_event(&tool_started(
            "lock-root",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &lock_dir.display().to_string()),
            ]),
            "agent-a",
        ))
        .expect("dir_lock update");
    writer.flush().expect("flush lock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "lock-root" => break,
            Some(_) => continue,
            None => panic!("extension closed before lock result"),
        }
    }

    writer
        .write_event(&tool_started(
            "blocked-edit",
            EDIT_TOOL_NAME,
            edit_arguments(&edit_path, vec![context_half_open_edit(1, 1, "hello", "")]),
            "agent-b",
        ))
        .expect("edit");
    writer.flush().expect("flush edit");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolProgressReported(progress))
                if progress.call_id.as_str() == "blocked-edit" =>
            {
                assert!(progress.message.as_deref().is_some_and(|message| {
                    message.contains(lock_dir.to_str().expect("lock dir path is UTF-8"))
                }));
                break;
            }
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "blocked-edit" => {
                panic!("edit completed before conflicting lock was released: {result:?}");
            }
            Some(_) => continue,
            None => panic!("extension closed before edit progress"),
        }
    }

    writer
        .write_event(&tool_started(
            "unlock-root",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "unlock"),
                ("directory", &lock_dir.display().to_string()),
            ]),
            "agent-a",
        ))
        .expect("dir_lock unlock");
    writer.flush().expect("flush unlock");

    let mut saw_unlock = false;
    let mut saw_edit = false;
    let deadline = Instant::now() + Duration::from_secs(3);
    while !(saw_unlock && saw_edit) {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for unlock/edit"
        );
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "unlock-root" => {
                saw_unlock = true;
            }
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "blocked-edit" => {
                saw_edit = true;
            }
            Some(_) => continue,
            None => panic!("extension closed before edit result"),
        }
    }
    assert_eq!(
        fs::read_to_string(&edit_path).expect("edited file"),
        "hello\n"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn disconnect_cancels_active_dir_lock_waiter_before_scheduler_join() {
    let tempdir = TempDir::new().expect("tempdir");
    let lock_dir = tempdir.path().to_path_buf();
    let edit_path = lock_dir.join("file.txt");
    let (mut reader, mut writer, done_rx) = spawn_extension_with_exit();
    drain_startup(&mut reader);
    send_dir_lock_config(&mut writer, true);

    writer
        .write_event(&tool_started(
            "lock-root",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &lock_dir.display().to_string()),
            ]),
            "agent-a",
        ))
        .expect("dir_lock update");
    writer.flush().expect("flush lock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "lock-root" => break,
            Some(_) => continue,
            None => panic!("extension closed before lock result"),
        }
    }

    writer
        .write_event(&tool_started(
            "blocked-edit",
            EDIT_TOOL_NAME,
            edit_arguments(&edit_path, vec![context_half_open_edit(1, 1, "hello", "")]),
            "agent-b",
        ))
        .expect("edit");
    writer.flush().expect("flush edit");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolProgressReported(progress))
                if progress.call_id.as_str() == "blocked-edit" =>
            {
                break;
            }
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "blocked-edit" => {
                panic!("edit completed before conflicting lock was released: {result:?}");
            }
            Some(_) => continue,
            None => panic!("extension closed before edit progress"),
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush disconnect");

    done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("extension should exit promptly after disconnect")
        .expect("extension run should succeed");
}

/// A full-dispatch mutation waiting on a directory lock must retain the workdir
/// captured at admission even when metadata commits a different path meanwhile.
#[test]
fn locked_apply_patch_uses_workdir_frozen_at_admission() {
    let tempdir = TempDir::new().expect("tempdir");
    let cwd_a = tempdir.path().join("a");
    let cwd_b = tempdir.path().join("b");
    fs::create_dir_all(&cwd_a).expect("create a");
    fs::create_dir_all(&cwd_b).expect("create b");
    fs::write(cwd_a.join("file.txt"), "before\n").expect("write a");
    fs::write(cwd_b.join("file.txt"), "before\n").expect("write b");
    let agent_id = tau_proto::AgentId::parse("agent-patch-cwd-lock").expect("agent id");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    send_dir_lock_config(&mut writer, true);

    writer
        .write_event(&Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
            agent_id: agent_id.clone(),
            key: tau_proto::AgentMetadataKey::new("ext_test-extension_cwd"),
            value: CborValue::Text(cwd_a.display().to_string()),
            mutation_id: None,
            inheritable: true,
        }))
        .expect("seed cwd");
    writer.flush().expect("flush seed");
    let _ = reader.read_event().expect("read context").expect("context");

    writer
        .write_event(&tool_started(
            "lock-a",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &cwd_a.display().to_string()),
            ]),
            "agent-locker",
        ))
        .expect("dir_lock update");
    writer.flush().expect("flush lock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "lock-a" => break,
            Some(_) => continue,
            None => panic!("extension closed before lock result"),
        }
    }

    let patch = "*** Begin Patch\n*** Update File: file.txt\n@@\n-before\n+after\n*** End Patch";
    writer
        .write_event(&tool_started(
            "blocked-patch",
            APPLY_PATCH_TOOL_NAME,
            CborValue::Text(patch.to_owned()),
            agent_id.as_str(),
        ))
        .expect("apply_patch");
    writer.flush().expect("flush patch");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolProgressReported(progress))
                if progress.call_id.as_str() == "blocked-patch" =>
            {
                assert!(progress.message.as_deref().is_some_and(|message| {
                    message.contains(cwd_a.to_str().expect("cwd a path is UTF-8"))
                }));
                break;
            }
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "blocked-patch" => {
                panic!("apply_patch completed before conflicting lock was released: {result:?}");
            }
            Some(_) => continue,
            None => panic!("extension closed before apply_patch progress"),
        }
    }

    writer
        .write_event(&Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
            agent_id: agent_id.clone(),
            key: tau_proto::AgentMetadataKey::new("ext_test-extension_cwd"),
            value: CborValue::Text(cwd_b.display().to_string()),
            mutation_id: None,
            inheritable: true,
        }))
        .expect("move cwd while waiting");
    writer.flush().expect("flush cwd b");
    let _ = reader
        .read_event()
        .expect("read cwd b context")
        .expect("context");

    writer
        .write_event(&tool_started(
            "unlock-a",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "unlock"),
                ("directory", &cwd_a.display().to_string()),
            ]),
            "agent-locker",
        ))
        .expect("dir_lock unlock");
    writer.flush().expect("flush unlock");

    let mut saw_unlock = false;
    let mut saw_patch = false;
    let deadline = Instant::now() + Duration::from_secs(3);
    while !(saw_unlock && saw_patch) {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for unlock/apply_patch"
        );
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "unlock-a" => {
                saw_unlock = true
            }
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "blocked-patch" => {
                saw_patch = true
            }
            Some(_) => continue,
            None => panic!("extension closed before apply_patch result"),
        }
    }
    assert_eq!(
        fs::read_to_string(cwd_a.join("file.txt")).expect("read a"),
        "after\n"
    );
    assert_eq!(
        fs::read_to_string(cwd_b.join("file.txt")).expect("read b"),
        "before\n"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn shell_without_manual_lock_does_not_wait_for_update_lock() {
    let tempdir = TempDir::new().expect("tempdir");
    let cwd_a = tempdir.path().join("a");
    let cwd_b = tempdir.path().join("b");
    fs::create_dir_all(&cwd_a).expect("create a");
    fs::create_dir_all(&cwd_b).expect("create b");
    let agent_id = tau_proto::AgentId::parse("agent-shell-cwd-lock").expect("agent id");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    send_dir_lock_config(&mut writer, true);

    writer
        .write_event(&Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
            agent_id: agent_id.clone(),
            key: tau_proto::AgentMetadataKey::new("ext_test-extension_cwd"),
            value: CborValue::Text(cwd_a.display().to_string()),
            mutation_id: None,
            inheritable: true,
        }))
        .expect("seed cwd");
    writer.flush().expect("flush seed");
    let _ = reader.read_event().expect("read context").expect("context");

    writer
        .write_event(&tool_started(
            "lock-shell-a",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &cwd_a.display().to_string()),
            ]),
            "agent-locker",
        ))
        .expect("dir_lock update");
    writer.flush().expect("flush lock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "lock-shell-a" => break,
            Some(_) => continue,
            None => panic!("extension closed before lock result"),
        }
    }

    writer
        .write_event(&tool_started(
            "blocked-shell",
            SHELL_TOOL_NAME,
            cbor_text_map(vec![("command", "printf after > shell.txt")]),
            agent_id.as_str(),
        ))
        .expect("shell");
    writer.flush().expect("flush shell");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "blocked-shell" => {
                let display = result.display.expect("display");
                assert_eq!(display.mode, "ro");
                break;
            }
            Some(_) => continue,
            None => panic!("extension closed before shell result"),
        }
    }

    writer
        .write_event(&tool_started(
            "unlock-shell-a",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "unlock"),
                ("directory", &cwd_a.display().to_string()),
            ]),
            "agent-locker",
        ))
        .expect("dir_lock unlock");
    writer.flush().expect("flush unlock");
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        assert!(Instant::now() < deadline, "timed out waiting for unlock");
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "unlock-shell-a" => break,
            Some(_) => continue,
            None => panic!("extension closed before unlock result"),
        }
    }
    assert!(!cwd_b.join("shell.txt").exists());

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn shell_with_covering_manual_lock_displays_inferred_read_write_mode() {
    let tempdir = TempDir::new().expect("tempdir");
    let lock_dir = tempdir.path().to_path_buf();
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    send_dir_lock_config(&mut writer, true);

    writer
        .write_event(&tool_started(
            "lock-root",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &lock_dir.display().to_string()),
            ]),
            "agent-a",
        ))
        .expect("dir_lock update");
    writer.flush().expect("flush lock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "lock-root" => break,
            Some(_) => continue,
            None => panic!("extension closed before lock result"),
        }
    }

    writer
        .write_event(&tool_started(
            "rw-shell",
            SHELL_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "printf rw-ok"),
                ("cwd", &lock_dir.display().to_string()),
            ]),
            "agent-a",
        ))
        .expect("rw shell");
    writer.flush().expect("flush shell");

    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolProgressReported(progress))
                if progress.call_id.as_str() == "rw-shell" =>
            {
                assert_eq!(progress.display.expect("display").mode, "rw");
                break;
            }
            Some(_) => continue,
            None => panic!("extension closed before shell progress"),
        }
    }
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "rw-shell" => {
                assert_eq!(result.display.expect("display").mode, "rw");
                break;
            }
            Some(_) => continue,
            None => panic!("extension closed before shell result"),
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn dir_lock_releases_delegate_locks_on_start_agent_result() {
    let tempdir = TempDir::new().expect("tempdir");
    let lock_dir = tempdir.path().to_path_buf();
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    send_dir_lock_config(&mut writer, true);

    writer
        .write_event(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
            query_id: "delegate-locker".to_owned(),
            agent_id: tau_proto::AgentId::parse("agent-locker").expect("agent id"),
        }))
        .expect("start accepted");
    writer
        .write_event(&tool_started(
            "lock-by-delegate",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &lock_dir.display().to_string()),
            ]),
            "agent-locker",
        ))
        .expect("dir_lock update");
    writer.flush().expect("flush delegate lock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "lock-by-delegate" => {
                break;
            }
            Some(_) => continue,
            None => panic!("extension closed before delegate lock result"),
        }
    }

    // Delegates can finish without issuing an explicit unlock. Tau keeps their
    // session agent loaded for history, so ext-shell must release manual locks
    // on the start-result lifecycle event rather than waiting only for a later
    // SessionAgentUnloaded event.
    writer
        .write_event(&Event::StartAgentResult(tau_proto::StartAgentResult {
            query_id: "delegate-locker".to_owned(),
            text: "done".to_owned(),
            error: None,
        }))
        .expect("start result");
    writer
        .write_event(&tool_started(
            "lock-after-delegate-result",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &lock_dir.display().to_string()),
            ]),
            "agent-b",
        ))
        .expect("dir_lock update after result");
    writer.flush().expect("flush after result lock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result))
                if result.call_id.as_str() == "lock-after-delegate-result" =>
            {
                break;
            }
            Some(Event::ToolProgressReported(progress))
                if progress.call_id.as_str() == "lock-after-delegate-result" =>
            {
                panic!("lock waited after delegate lifecycle release: {progress:?}");
            }
            Some(_) => continue,
            None => panic!("extension closed before second lock result"),
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// Ensures committed session unload is ext-shell's general agent cleanup
/// authority: it releases the unloaded agent's manual lock before a queued
/// waiter is granted ownership.
#[test]
fn dir_lock_releases_agent_locks_on_session_agent_unloaded() {
    let tempdir = TempDir::new().expect("tempdir");
    let lock_dir = tempdir.path().to_path_buf();
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    send_dir_lock_config(&mut writer, true);

    writer
        .write_event(&tool_started(
            "lock-before-unload",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &lock_dir.display().to_string()),
            ]),
            "agent-unloaded",
        ))
        .expect("dir_lock update");
    writer.flush().expect("flush owner lock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "lock-before-unload" => {
                break;
            }
            Some(_) => continue,
            None => panic!("extension closed before owner lock result"),
        }
    }

    writer
        .write_event(&tool_started(
            "lock-after-unload",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &lock_dir.display().to_string()),
            ]),
            "agent-waiter",
        ))
        .expect("queued dir_lock update");
    writer.flush().expect("flush queued lock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolProgressReported(progress))
                if progress.call_id.as_str() == "lock-after-unload" =>
            {
                break;
            }
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "lock-after-unload" => {
                panic!("waiter acquired lock before unload: {result:?}");
            }
            Some(_) => continue,
            None => panic!("extension closed before queued lock progress"),
        }
    }

    writer
        .write_event(&Event::SessionAgentUnloaded(
            tau_proto::SessionAgentUnloaded {
                session_id: tau_proto::SessionId::new("session-unload-lock"),
                agent_id: tau_proto::AgentId::parse("agent-unloaded").expect("agent id"),
            },
        ))
        .expect("session agent unloaded");
    writer.flush().expect("flush unload");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "lock-after-unload" => {
                break;
            }
            Some(_) => continue,
            None => panic!("extension closed before waiter lock result"),
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn dir_lock_unlock_can_target_another_owner() {
    let tempdir = TempDir::new().expect("tempdir");
    let lock_dir = tempdir.path().to_path_buf();
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    send_dir_lock_config(&mut writer, true);

    writer
        .write_event(&tool_started(
            "lock-owner",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &lock_dir.display().to_string()),
            ]),
            "agent-a",
        ))
        .expect("dir_lock update");
    writer.flush().expect("flush lock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "lock-owner" => break,
            Some(_) => continue,
            None => panic!("extension closed before lock result"),
        }
    }

    writer
        .write_event(&tool_started(
            "force-unlock-owner",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "unlock"),
                ("directory", &lock_dir.display().to_string()),
                ("owner_agent_id", "agent-a"),
            ]),
            "agent-b",
        ))
        .expect("dir_lock force unlock");
    writer.flush().expect("flush force unlock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "force-unlock-owner" => {
                break;
            }
            Some(Event::ToolError(error)) if error.call_id.as_str() == "force-unlock-owner" => {
                panic!("force unlock failed: {error:?}");
            }
            Some(_) => continue,
            None => panic!("extension closed before force unlock result"),
        }
    }

    writer
        .write_event(&tool_started(
            "lock-after-force-unlock",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &lock_dir.display().to_string()),
            ]),
            "agent-c",
        ))
        .expect("dir_lock after force unlock");
    writer.flush().expect("flush second lock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result))
                if result.call_id.as_str() == "lock-after-force-unlock" =>
            {
                break;
            }
            Some(Event::ToolProgressReported(progress))
                if progress.call_id.as_str() == "lock-after-force-unlock" =>
            {
                panic!("second lock waited after force unlock: {progress:?}");
            }
            Some(_) => continue,
            None => panic!("extension closed before second lock result"),
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn dir_lock_unlock_rejects_wrong_type_owner_agent_id() {
    let tempdir = TempDir::new().expect("tempdir");
    let lock_dir = tempdir.path().to_path_buf();
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    send_dir_lock_config(&mut writer, true);

    writer
        .write_event(&tool_started(
            "unlock-bad-owner-type",
            DIR_LOCK_TOOL_NAME,
            cbor_map(vec![
                ("command", CborValue::Text("unlock".to_owned())),
                ("directory", CborValue::Text(lock_dir.display().to_string())),
                ("owner_agent_id", CborValue::Integer(1.into())),
            ]),
            "agent-b",
        ))
        .expect("dir_lock unlock");
    writer.flush().expect("flush unlock");

    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolError(error)) if error.call_id.as_str() == "unlock-bad-owner-type" => {
                assert_eq!(error.message, "argument `owner_agent_id` must be a string");
                break;
            }
            Some(Event::ToolResult(result))
                if result.call_id.as_str() == "unlock-bad-owner-type" =>
            {
                panic!("unlock should fail, got {result:?}");
            }
            Some(_) => continue,
            None => panic!("extension closed before unlock result"),
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn dir_lock_update_errors_when_same_agent_already_holds_overlapping_lock() {
    let tempdir = TempDir::new().expect("tempdir");
    let lock_dir = tempdir.path().to_path_buf();
    let child_dir = lock_dir.join("child");
    fs::create_dir(&child_dir).expect("child dir");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    send_dir_lock_config(&mut writer, true);

    writer
        .write_event(&tool_started(
            "lock-root",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &lock_dir.display().to_string()),
            ]),
            "agent-a",
        ))
        .expect("dir_lock update");
    writer.flush().expect("flush first lock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "lock-root" => {
                let display = result.display.as_ref().expect("lock display");
                assert_eq!(display.args, format!("update {}", lock_dir.display()));
                assert_eq!(display.status_text, "ok");
                assert!(display.payload.is_none());
                break;
            }
            Some(_) => continue,
            None => panic!("extension closed before first lock result"),
        }
    }

    writer
        .write_event(&tool_started(
            "lock-child-again",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &child_dir.display().to_string()),
            ]),
            "agent-a",
        ))
        .expect("dir_lock duplicate update");
    writer.flush().expect("flush duplicate lock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolError(error)) if error.call_id.as_str() == "lock-child-again" => {
                assert_eq!(error.message, "dir_lock_duplicate");
                assert!(
                    !error
                        .message
                        .contains(lock_dir.to_str().expect("utf8 path"))
                );
                let details = error.details.as_ref().expect("structured details");
                assert_eq!(
                    cbor_map_text(details, "blocking_directory"),
                    Some(lock_dir.to_str().expect("utf8 path"))
                );
                assert_eq!(
                    cbor_map_text(details, "requested_directory"),
                    Some(child_dir.to_str().expect("utf8 path"))
                );
                assert_eq!(cbor_map_text(details, "lock_owner_id"), Some("agent-a"));
                assert_eq!(
                    cbor_map_text(details, "output"),
                    Some(
                        "Directory lock already held by this agent. Unlock the existing lock before locking another overlapping directory."
                    )
                );
                let display = error.display.as_ref().expect("error display");
                assert_eq!(display.args, format!("update {}", child_dir.display()));
                assert_eq!(display.status_text, "dir_lock failed");
                break;
            }
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "lock-child-again" => {
                panic!("duplicate manual lock succeeded: {result:?}");
            }
            Some(_) => continue,
            None => panic!("extension closed before duplicate lock error"),
        }
    }

    writer
        .write_event(&tool_started(
            "same-agent-edit",
            EDIT_TOOL_NAME,
            edit_arguments(
                &child_dir.join("file.txt"),
                vec![context_half_open_edit(1, 1, "hello", "")],
            ),
            "agent-a",
        ))
        .expect("same-agent edit");
    writer.flush().expect("flush same-agent edit");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "same-agent-edit" => {
                assert_eq!(
                    fs::read_to_string(child_dir.join("file.txt")).expect("same-agent edit file"),
                    "hello\n"
                );
                break;
            }
            Some(Event::ToolProgressReported(progress))
                if progress.call_id.as_str() == "same-agent-edit" =>
            {
                panic!("same-agent automatic edit waited on its own manual lock: {progress:?}");
            }
            Some(_) => continue,
            None => panic!("extension closed before same-agent edit result"),
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn dir_lock_waiting_progress_preserves_shell_mode() {
    let tempdir = TempDir::new().expect("tempdir");
    let Event::ToolStarted(invoke) = tool_started(
        "blocked-shell",
        SHELL_TOOL_NAME,
        cbor_text_map(vec![("command", "printf hello")]),
        "agent-b",
    ) else {
        panic!("expected tool started");
    };
    let progress = crate::dir_lock::waiting_progress(
        &invoke,
        &[tempdir.path().to_path_buf()],
        Some(crate::tools::shell::ShellCommandMode::visible(
            crate::tools::shell::ShellAccessMode::ReadWrite,
        )),
    );
    let display = progress.display.expect("waiting display");

    assert_eq!(display.mode, "rw");
    assert_eq!(display.args, tempdir.path().display().to_string());
    assert_eq!(display.info_chips, vec!["dir lock"]);
    assert_eq!(display.status, ToolUseStatus::InProgress);
    assert_eq!(display.status_text, "waiting");
}

#[test]
fn inferred_read_only_shell_bypasses_directory_update_lock() {
    // Shell commands without a covering manual lock behave like read tools for
    // advisory directory locking: they may run while another agent holds an
    // update lock.
    let tempdir = TempDir::new().expect("tempdir");
    let lock_dir = tempdir.path().to_path_buf();
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    send_dir_lock_config(&mut writer, true);
    writer
        .write_event(&tool_started(
            "lock-root",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &lock_dir.display().to_string()),
            ]),
            "agent-a",
        ))
        .expect("dir_lock update");
    writer.flush().expect("flush lock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "lock-root" => break,
            Some(_) => continue,
            None => panic!("extension closed before lock result"),
        }
    }

    writer
        .write_event(&tool_started(
            "read-only-shell",
            SHELL_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "printf ro-ok"),
                ("cwd", &lock_dir.display().to_string()),
            ]),
            "agent-b",
        ))
        .expect("ro shell");
    writer.flush().expect("flush shell");

    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolProgressReported(progress))
                if progress.call_id.as_str() == "read-only-shell" =>
            {
                assert_ne!(
                    progress.message.as_deref(),
                    Some("waiting for directory lock")
                );
                assert!(
                    !progress
                        .message
                        .as_deref()
                        .is_some_and(|message| message.starts_with("waiting for directory lock")),
                    "ro shell unexpectedly waited on directory lock: {progress:?}"
                );
            }
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "read-only-shell" => {
                assert_eq!(
                    optional_argument_text(&result.result, "output"),
                    Ok(Some("out(no_nl) ro-ok".to_owned()))
                );
                break;
            }
            Some(_) => continue,
            None => panic!("extension closed before ro shell result"),
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn same_agent_edit_reenters_manual_lock_while_shell_auto_lock_is_active() {
    let tempdir = TempDir::new().expect("tempdir");
    let lock_dir = tempdir.path().to_path_buf();
    let edit_path = lock_dir.join("while-shell-runs.txt");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    send_dir_lock_config(&mut writer, true);

    writer
        .write_event(&tool_started(
            "lock-root",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "update"),
                ("directory", &lock_dir.display().to_string()),
            ]),
            "agent-a",
        ))
        .expect("dir_lock update");
    writer.flush().expect("flush lock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "lock-root" => break,
            Some(_) => continue,
            None => panic!("extension closed before lock result"),
        }
    }

    writer
        .write_event(&tool_started(
            "same-agent-shell",
            SHELL_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "sleep 1; printf shell-done"),
                ("cwd", &lock_dir.display().to_string()),
            ]),
            "agent-a",
        ))
        .expect("same-agent shell");
    writer.flush().expect("flush shell");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolProgressReported(progress))
                if progress.call_id.as_str() == "same-agent-shell" =>
            {
                break;
            }
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "same-agent-shell" => {
                panic!("shell completed before test edit could be issued: {result:?}");
            }
            Some(_) => continue,
            None => panic!("extension closed before shell progress"),
        }
    }

    writer
        .write_event(&tool_started(
            "same-agent-edit",
            EDIT_TOOL_NAME,
            edit_arguments(&edit_path, vec![context_half_open_edit(1, 1, "hello", "")]),
            "agent-a",
        ))
        .expect("same-agent edit");
    writer.flush().expect("flush edit");

    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "same-agent-edit" => {
                assert_eq!(
                    fs::read_to_string(&edit_path).expect("same-agent edit file"),
                    "hello\n"
                );
                break;
            }
            Some(Event::ToolProgressReported(progress))
                if progress.call_id.as_str() == "same-agent-edit" =>
            {
                panic!("same-agent edit waited on its own active automatic lock: {progress:?}");
            }
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "same-agent-shell" => {
                panic!("same-agent edit was blocked until shell finished: {result:?}");
            }
            Some(_) => continue,
            None => panic!("extension closed before same-agent edit result"),
        }
    }

    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "same-agent-shell" => {
                break;
            }
            Some(_) => continue,
            None => panic!("extension closed before shell result"),
        }
    }

    writer
        .write_event(&tool_started(
            "unlock-root",
            DIR_LOCK_TOOL_NAME,
            cbor_text_map(vec![
                ("command", "unlock"),
                ("directory", &lock_dir.display().to_string()),
            ]),
            "agent-a",
        ))
        .expect("dir_lock unlock");
    writer.flush().expect("flush unlock");
    loop {
        match reader.read_event().expect("read") {
            Some(Event::ToolResult(result)) if result.call_id.as_str() == "unlock-root" => break,
            Some(_) => continue,
            None => panic!("extension closed before unlock result"),
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// The two shell registrations must advertise only their respective
/// provider-facing directory spellings and keep timeout schema differences.
#[test]
fn startup_registers_surface_specific_shell_workdir_schemas() {
    // The model-visible schema must advertise the implemented working-directory
    // argument and reject negative timeouts before invocation. Directory update
    // coordination is handled inside ext-shell when dir_lock is enabled, not by
    // harness execution modes.
    let (mut reader, mut writer) = spawn_extension();

    let mut found_shell = false;
    let mut found_gpt_shell = false;
    for _ in 0..12 {
        let event = reader
            .read_event()
            .expect("read")
            .expect("startup event should arrive");
        let Event::ToolRegistrationDeclared(register) = event else {
            continue;
        };
        if register.tool.name == SHELL_TOOL_NAME || register.tool.name == GPT_SHELL_TOOL_NAME {
            let description = register.tool.description.as_deref().expect("description");
            if register.tool.name == SHELL_TOOL_NAME {
                assert!(description.contains("structured command results"));
            }
            assert!(!description.contains("tool errors"));
            let parameters = register.tool.parameters.as_ref().expect("parameters");
            let properties = &parameters["properties"];
            assert!(properties["mode"].is_null());
            if register.tool.name == SHELL_TOOL_NAME {
                assert_eq!(properties["cwd"]["type"], serde_json::json!("string"));
                assert!(properties["workdir"].is_null());
                assert_eq!(properties["timeout"]["minimum"], serde_json::json!(0));
            } else {
                assert_eq!(
                    register.tool.model_visible_name.as_deref(),
                    Some("shell_command")
                );
                assert_eq!(properties["workdir"]["type"], serde_json::json!("string"));
                assert!(properties["cwd"].is_null());
                assert!(
                    properties["workdir"]["description"]
                        .as_str()
                        .expect("workdir description")
                        .contains("top-level workdir(path) tool")
                );
            }
            assert_eq!(parameters["required"], serde_json::json!(["command"]));
            found_shell |= register.tool.name == SHELL_TOOL_NAME;
            found_gpt_shell |= register.tool.name == GPT_SHELL_TOOL_NAME;
        }
    }
    assert!(found_shell, "expected shell tool registration");
    assert!(found_gpt_shell, "expected gpt_shell tool registration");

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// The persistent workdir tool must expose one optional path, so an empty
/// object remains the provider-visible read operation.
#[test]
fn workdir_schema_keeps_path_optional() {
    let tool = registered_tool_specs(false)
        .into_iter()
        .find(|tool| tool.name.as_str() == WORKDIR_TOOL_NAME)
        .expect("workdir tool");
    let parameters = tool.parameters.expect("workdir parameters");
    assert!(parameters.get("required").is_none());
    assert_eq!(parameters["properties"]["path"]["type"], "string");
    assert_eq!(parameters["properties"]["path"]["minLength"], 1);
}

#[test]
fn startup_registers_shell_workdir_prompt_fragment() {
    // The cwd prompt prose is owned by the shell extension, not an individual
    // tool, so it remains available even when shell tools are disabled.
    let (mut reader, mut writer) = spawn_extension();

    let mut found_context_provider = false;
    let mut found_fragment = false;
    let mut saw_tool_fragment = false;
    for _ in 0..15 {
        let event = reader
            .read_event()
            .expect("read")
            .expect("startup event should arrive");
        match event {
            Event::ToolRegistrationDeclared(register) => {
                saw_tool_fragment |= register.prompt_fragment.is_some();
            }
            Event::ExtensionContextProviderRegister(_) => {
                found_context_provider = true;
            }
            Event::ExtPromptFragmentPublish(publish) => {
                assert_eq!(publish.fragment.name, "shell.workdir");
                assert_eq!(
                    publish.fragment.priority,
                    tau_proto::PromptPriority::new(900)
                );
                assert!(
                    publish
                        .fragment
                        .template
                        .as_str()
                        .contains("agent_context.workdir")
                );
                found_fragment = true;
            }
            _ => {}
        }
    }
    assert!(
        found_context_provider,
        "shell cwd context must gate first prompt dispatch"
    );
    assert!(found_fragment, "expected shell cwd prompt fragment publish");
    assert!(!saw_tool_fragment, "cwd must not be attached to any tool");

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn session_agent_loaded_publishes_current_directory_context_for_agent() {
    // Agent context is the structured source used by the shell cwd prompt
    // fragment; it must be keyed by durable agent, not by session.
    let cwd = std::env::current_dir().expect("current dir");
    let (tx, rx) = std::sync::mpsc::channel();
    let output = Output::channel(tx);
    let cwd_state = CwdState::new();

    dispatch_session_agent_loaded(
        tau_proto::SessionAgentLoaded {
            session_id: tau_proto::SessionId::new("session-1"),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            ephemeral: false,
        },
        &output,
        &cwd_state,
        false,
    );

    let HarnessInputMessage::Emit(emit) = rx.recv().expect("cwd metadata publish") else {
        panic!("expected cwd metadata publish");
    };
    let Event::AgentMetadataSet(metadata) = *emit.event else {
        panic!("expected cwd metadata publish");
    };
    assert_eq!(metadata.key.as_str(), "ext_core-shell_cwd");
    assert!(metadata.inheritable);
    assert!(
        rx.try_recv().is_err(),
        "context waits for committed metadata"
    );

    cwd_state.set(
        metadata.agent_id.clone(),
        PathBuf::from(cwd.display().to_string()),
    );
    let context = cwd_context_event(metadata.agent_id, &cwd, &cwd_state);
    let Event::ExtAgentContextPublish(publish) = context else {
        panic!("expected cwd agent context publish");
    };
    assert_eq!(publish.agent_id.as_ref(), "agent-1");
    assert_eq!(publish.key.as_ref(), "workdir");
    assert_eq!(publish.value.0["path"], cwd.display().to_string());
    assert_eq!(publish.value.0["label"], "default");
    assert_eq!(publish.value.0["status"], "available");
}

#[test]
fn discover_agents_files_walks_ancestor_chain_in_order() {
    let tempdir = TempDir::new().expect("tempdir");
    let root = tempdir.path();
    let nested = root.join("pkg/src");
    fs::create_dir_all(&nested).expect("mkdir");

    let root_agents = root.join("AGENTS.md");
    let root_extra_agents = root.join("AGENTS.extra.md");
    let ignored_agents = root.join("AGENTS.txt");
    let ignored_dir_agents = root.join("AGENTS.dir.md");
    let pkg_agents = root.join("pkg").join("AGENTS.md");
    let pkg_extra_agents = root.join("pkg").join("AGENTS.zeta.md");
    let empty_agents = root.join("pkg").join("src").join("AGENTS.md");

    fs::write(&root_agents, "# Root\n- rule one\n").expect("write root");
    fs::write(&root_extra_agents, "# Root extra\n- rule extra\n").expect("write root extra");
    fs::write(&ignored_agents, "# Ignored\n").expect("write ignored");
    fs::create_dir(&ignored_dir_agents).expect("create ignored agents dir");
    fs::write(&pkg_agents, "# Package\n- rule two\n").expect("write pkg");
    fs::write(&pkg_extra_agents, "# Package extra\n- rule zeta\n").expect("write pkg extra");
    fs::write(&empty_agents, "   \n").expect("write empty");

    let discovered = discover_agents_files_from(&nested);
    assert_eq!(discovered.len(), 4);
    assert_eq!(
        discovered[0].file_path,
        root_agents.canonicalize().expect("canonical root")
    );
    assert_eq!(
        discovered[1].file_path,
        root_extra_agents
            .canonicalize()
            .expect("canonical root extra")
    );
    assert_eq!(
        discovered[2].file_path,
        pkg_agents.canonicalize().expect("canonical pkg")
    );
    assert_eq!(
        discovered[3].file_path,
        pkg_extra_agents
            .canonicalize()
            .expect("canonical pkg extra")
    );
    assert!(discovered[0].content.contains("rule one"));
    assert!(discovered[1].content.contains("rule extra"));
    assert!(discovered[2].content.contains("rule two"));
    assert!(discovered[3].content.contains("rule zeta"));
}

#[test]
fn discover_agents_files_follows_symlinked_candidates() {
    // AGENTS.md files are trusted prompt input. Tau follows symlinks here so
    // project-local and dotfile-managed instruction layouts behave like ordinary
    // filesystem reads.
    let tempdir = TempDir::new().expect("tempdir");
    let root = tempdir.path().join("repo");
    fs::create_dir_all(&root).expect("mkdir");
    let shared = tempdir.path().join("shared.AGENTS.md");
    fs::write(&shared, "# Shared\n- linked rule\n").expect("write shared agents");
    fs::write(root.join("AGENTS.good.md"), "# Good\n").expect("write good agents");
    symlink(&shared, root.join("AGENTS.md")).expect("symlink agents");

    let discovered = discover_agents_files_from_roots(vec![root]);
    assert_eq!(discovered.len(), 2);
    assert_eq!(
        discovered[0].file_path,
        shared.canonicalize().expect("canonical shared")
    );
    assert!(discovered[0].content.contains("linked rule"));
    assert!(discovered[1].file_path.ends_with("AGENTS.good.md"));
}

/// Ensures symlinked project `.agents.local` roots are followed like normal
/// trusted instruction directories.
#[test]
fn discover_agents_files_follows_symlinked_agent_roots() {
    let tempdir = TempDir::new().expect("tempdir");
    let repo = tempdir.path().join("repo");
    let shared = tempdir.path().join("shared-agents");
    fs::create_dir_all(&repo).expect("mkdir repo");
    fs::create_dir_all(&shared).expect("mkdir shared");
    fs::write(shared.join("AGENTS.md"), "# Shared root\n- linked root\n")
        .expect("write shared agents");
    symlink(&shared, repo.join(".agents.local")).expect("symlink agents local");

    let discovered = discover_agents_files_from_roots(vec![repo.join(".agents.local")]);
    assert_eq!(discovered.len(), 1);
    assert!(discovered[0].content.contains("linked root"));
}

#[test]
fn discover_agents_files_skips_oversized_candidates() {
    // Session-start AGENTS loading must have its own input cap; output caps on
    // later tool calls do not protect the implicit instruction payload.
    let tempdir = TempDir::new().expect("tempdir");
    let root = tempdir.path();
    fs::write(root.join("AGENTS.md"), "x".repeat(1024 * 1024 + 1)).expect("write huge agents");
    fs::write(root.join("AGENTS.ok.md"), "# Ok\n").expect("write ok agents");

    let discovered = discover_agents_files_from_roots(vec![root.to_path_buf()]);
    assert_eq!(discovered.len(), 1);
    assert!(discovered[0].file_path.ends_with("AGENTS.ok.md"));
}

/// Ensures user instruction roots still load before project instructions, while
/// preferring the XDG user directories over legacy `~/.agents` roots.
#[test]
fn user_agents_roots_prefer_config_agents_before_legacy_home_agents() {
    let tempdir = TempDir::new().expect("tempdir");
    let home = tempdir.path().join("home");
    let repo = tempdir.path().join("repo");
    fs::create_dir_all(home.join(".agents")).expect("home agents dir");
    fs::create_dir_all(home.join(".agents.local")).expect("home local agents dir");
    fs::create_dir_all(home.join(".config").join("agents")).expect("config agents dir");
    fs::create_dir_all(home.join(".config").join("agents.local")).expect("config local agents dir");
    fs::create_dir_all(repo.join("pkg")).expect("repo pkg dir");

    let config_agents = home.join(".config").join("agents").join("AGENTS.md");
    let config_local_agents = home.join(".config").join("agents.local").join("AGENTS.md");
    let legacy_agents = home.join(".agents").join("AGENTS.md");
    let legacy_local_agents = home.join(".agents.local").join("AGENTS.md");
    let repo_agents = repo.join("AGENTS.md");
    let pkg_agents = repo.join("pkg").join("AGENTS.md");
    fs::write(&config_agents, "# Home config\n- preferred personal rule\n")
        .expect("write config home");
    fs::write(&config_local_agents, "# Home config local\n").expect("write config local home");
    fs::write(&legacy_agents, "# Home legacy\n- legacy personal rule\n")
        .expect("write legacy home");
    fs::write(&legacy_local_agents, "# Home legacy local\n").expect("write legacy local home");
    fs::write(&repo_agents, "# Repo\n- repo rule\n").expect("write repo");
    fs::write(&pkg_agents, "# Package\n- package rule\n").expect("write pkg");

    let mut roots = user_agents_roots(&home);
    roots.push(repo.clone());
    roots.push(repo.join("pkg"));
    let discovered = discover_agents_files_from_roots(roots);

    let paths: Vec<PathBuf> = discovered.iter().map(|f| f.file_path.clone()).collect();
    assert_eq!(
        paths,
        vec![
            config_agents.canonicalize().expect("canonical config home"),
            config_local_agents
                .canonicalize()
                .expect("canonical config local home"),
            legacy_agents.canonicalize().expect("canonical legacy home"),
            legacy_local_agents
                .canonicalize()
                .expect("canonical legacy local home"),
            repo_agents.canonicalize().expect("canonical repo"),
            pkg_agents.canonicalize().expect("canonical pkg"),
        ]
    );
}

#[test]
fn discover_agents_files_includes_local_agent_dirs_after_regular_paths() {
    let tempdir = TempDir::new().expect("tempdir");
    let repo = tempdir.path().join("repo");
    let nested = repo.join("pkg");
    fs::create_dir_all(nested.join(".agents.local")).expect("nested local agents dir");

    let repo_agents = repo.join("AGENTS.md");
    let repo_local_agents = repo.join(".agents.local").join("AGENTS.md");
    let nested_agents = nested.join("AGENTS.md");
    let nested_local_agents = nested.join(".agents.local").join("AGENTS.md");
    fs::create_dir_all(repo.join(".agents.local")).expect("repo local agents dir");
    fs::write(&repo_agents, "# Repo\n").expect("write repo");
    fs::write(&repo_local_agents, "# Repo local\n").expect("write repo local");
    fs::write(&nested_agents, "# Nested\n").expect("write nested");
    fs::write(&nested_local_agents, "# Nested local\n").expect("write nested local");

    let discovered = discover_agents_files_from(&nested);
    let paths: Vec<PathBuf> = discovered.iter().map(|f| f.file_path.clone()).collect();
    assert_eq!(
        paths,
        vec![
            repo_agents.canonicalize().expect("canonical repo"),
            repo_local_agents
                .canonicalize()
                .expect("canonical repo local"),
            nested_agents.canonicalize().expect("canonical nested"),
            nested_local_agents
                .canonicalize()
                .expect("canonical nested local"),
        ]
    );
}

/// Ensures user skill roots keep project roots first, then assign XDG user
/// roots higher collision precedence than legacy `~/.agents` roots.
#[test]
fn session_skill_dirs_include_config_agents() {
    let temp = TempDir::new().expect("tempdir");
    let cwd = temp.path().join("repo");
    let home = temp.path().join("home");
    fs::create_dir_all(cwd.join(".agents").join("skills")).expect("cwd agents skills");
    fs::create_dir_all(cwd.join(".agents.local").join("skills")).expect("cwd local agents skills");

    let dirs = session_skill_dirs(Some(cwd.clone()), Some(home.clone()));
    let paths: Vec<_> = dirs.iter().map(|dir| dir.path.clone()).collect();
    let prompt_defaults: Vec<_> = dirs
        .iter()
        .map(|dir| dir.add_to_prompt_by_default)
        .collect();
    let source_precedence: Vec<_> = dirs.iter().map(|dir| dir.source_precedence).collect();

    assert_eq!(
        paths,
        vec![
            cwd.join(".agents").join("skills"),
            cwd.join(".agents.local").join("skills"),
            home.join(".config").join("agents").join("skills"),
            home.join(".config").join("agents.local").join("skills"),
            home.join(".agents").join("skills"),
            home.join(".agents.local").join("skills"),
        ]
    );
    assert_eq!(
        prompt_defaults,
        vec![true, true, false, false, false, false]
    );
    assert_eq!(
        source_precedence,
        vec![None, None, Some(0), Some(0), Some(1), Some(1)]
    );
}

/// Ensures only existing project skill roots from the ancestor chain are
/// advertised by default, and user roots are appended afterward.
#[test]
fn session_skill_dirs_include_existing_project_ancestors() {
    let temp = TempDir::new().expect("tempdir");
    let repo = temp.path().join("repo");
    let pkg = repo.join("pkg");
    let cwd = pkg.join("src");
    let home = temp.path().join("home");
    let repo_skills = repo.join(".agents").join("skills");
    let pkg_local_skills = pkg.join(".agents.local").join("skills");
    fs::create_dir_all(&cwd).expect("cwd");
    fs::create_dir_all(&repo_skills).expect("repo skills");
    fs::create_dir_all(&pkg_local_skills).expect("pkg local skills");

    let dirs = session_skill_dirs(Some(cwd), Some(home.clone()));
    let paths: Vec<_> = dirs.iter().map(|dir| dir.path.clone()).collect();

    assert_eq!(
        paths,
        vec![
            repo_skills,
            pkg_local_skills,
            home.join(".config").join("agents").join("skills"),
            home.join(".config").join("agents.local").join("skills"),
            home.join(".agents").join("skills"),
            home.join(".agents.local").join("skills"),
        ]
    );
}

/// Ensures a home directory that contains the current working directory is not
/// treated as a project-skill root, including the preferred XDG user roots.
#[test]
fn session_skill_dirs_do_not_treat_home_agents_as_project_skills() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    let cwd = home.join("repo");
    let home_skills = home.join(".agents").join("skills");
    let repo_skills = cwd.join(".agents").join("skills");
    fs::create_dir_all(&home_skills).expect("home skills");
    fs::create_dir_all(&repo_skills).expect("repo skills");

    let dirs = session_skill_dirs(Some(cwd), Some(home.clone()));
    let project_defaults: Vec<_> = dirs
        .iter()
        .map(|dir| (dir.path.clone(), dir.add_to_prompt_by_default))
        .collect();

    assert_eq!(
        project_defaults,
        vec![
            (repo_skills, true),
            (home.join(".config").join("agents").join("skills"), false),
            (
                home.join(".config").join("agents.local").join("skills"),
                false,
            ),
            (home_skills, false),
            (home.join(".agents.local").join("skills"), false),
        ]
    );
}

#[test]
fn project_scoped_skills_are_advertised_by_default() {
    let temp = TempDir::new().expect("tempdir");
    let cwd = temp.path().join("repo");
    let home = temp.path().join("home");
    let project_skill_dir = cwd.join(".agents").join("skills").join("project-skill");
    let user_skill_dir = home.join(".agents").join("skills").join("user-skill");
    fs::create_dir_all(&project_skill_dir).expect("create project skill dir");
    fs::create_dir_all(&user_skill_dir).expect("create user skill dir");
    let project_hidden_dir = cwd
        .join(".agents")
        .join("skills")
        .join("project-hidden-skill");
    fs::create_dir_all(&project_hidden_dir).expect("create hidden project skill dir");
    fs::write(
        project_skill_dir.join("SKILL.md"),
        "---\nname: project-skill\ndescription: Project skill\n---\nbody\n",
    )
    .expect("write project skill");
    fs::write(
        project_hidden_dir.join("SKILL.md"),
        "---\nname: project-hidden-skill\ndescription: Hidden project skill\nadvertise: false\n---\nbody\n",
    )
    .expect("write hidden project skill");
    fs::write(
        user_skill_dir.join("SKILL.md"),
        "---\nname: user-skill\ndescription: User skill\n---\nbody\n",
    )
    .expect("write user skill");

    let result =
        tau_skills::load_skills_from_skill_dirs(&session_skill_dirs(Some(cwd), Some(home)));
    let project_skill = result
        .skills
        .iter()
        .find(|skill| skill.name == "project-skill")
        .expect("project skill");
    let user_skill = result
        .skills
        .iter()
        .find(|skill| skill.name == "user-skill")
        .expect("user skill");
    let project_hidden_skill = result
        .skills
        .iter()
        .find(|skill| skill.name == "project-hidden-skill")
        .expect("hidden project skill");

    assert!(project_skill.add_to_prompt);
    assert!(!project_hidden_skill.add_to_prompt);
    assert!(!user_skill.add_to_prompt);
}

#[test]
fn skill_diagnostics_are_emitted_as_harness_notice() {
    let temp = TempDir::new().expect("tempdir");
    let skills_dir = temp.path().join(".agents").join("skills");
    let skill_dir = skills_dir.join("bad-skill");
    fs::create_dir_all(&skill_dir).expect("create skill dir");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: bad skill\ndescription: bad skill\n---\n\n# Bad\n",
    )
    .expect("write skill");

    let result = tau_skills::load_skills_from_dirs(&[skills_dir]);
    assert!(result.skills.is_empty());

    let mut events = Vec::new();
    push_skill_diagnostic_events(&mut events, result.diagnostics);

    let skipped = events.iter().find_map(|event| match event {
        Event::HarnessNotice(info) if info.message.contains("skill skipped:") => Some(info),
        _ => None,
    });
    let Some(info) = skipped else {
        panic!("expected skipped skill harness notice event, got {events:?}");
    };
    assert_eq!(info.level, tau_proto::NoticeLevel::Warning);
    assert!(info.message.contains("bad-skill/SKILL.md"));
    assert!(info.message.contains("name contains invalid characters"));
}

/// Ensures ext-shell keeps the expected notice severity for each skill-loader
/// diagnostic kind: soft warnings are informational, expected collisions are
/// trace-only, and skipped skills stay visible warnings.
#[test]
fn skill_diagnostics_map_expected_notice_levels() {
    let mut events = Vec::new();
    push_skill_diagnostic_events(
        &mut events,
        vec![
            tau_skills::SkillDiagnostic {
                path: PathBuf::from("warn/SKILL.md"),
                kind: tau_skills::DiagnosticKind::Warning,
                message: "soft warning".to_owned(),
            },
            tau_skills::SkillDiagnostic {
                path: PathBuf::from("collision/SKILL.md"),
                kind: tau_skills::DiagnosticKind::Collision,
                message: "name collision".to_owned(),
            },
            tau_skills::SkillDiagnostic {
                path: PathBuf::from("skipped/SKILL.md"),
                kind: tau_skills::DiagnosticKind::Skipped,
                message: "fatal skip".to_owned(),
            },
        ],
    );

    let notices = events
        .iter()
        .map(|event| match event {
            Event::HarnessNotice(info) => info,
            other => panic!("expected harness notice event, got {other:?}"),
        })
        .collect::<Vec<_>>();
    let warning = notices
        .iter()
        .find(|info| info.message.contains("skill warning:"))
        .expect("warning diagnostic");
    assert_eq!(warning.level, tau_proto::NoticeLevel::Info);
    assert!(!warning.always_show);

    let collision = notices
        .iter()
        .find(|info| info.message.contains("skill collision:"))
        .expect("collision diagnostic");
    assert_eq!(collision.kind, tau_proto::notice_kind::SKILL_COLLISION);
    assert_eq!(collision.level, tau_proto::NoticeLevel::Trace);
    assert!(!collision.always_show);

    let skipped = notices
        .iter()
        .find(|info| info.message.contains("skill skipped:"))
        .expect("skipped diagnostic");
    assert_eq!(skipped.level, tau_proto::NoticeLevel::Warning);
    assert!(skipped.always_show);
}

#[test]
fn session_agent_loaded_emits_ready_after_agent_context_publish() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
            session_id: "s1".into(),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            ephemeral: false,
        }))
        .expect("request");
    writer
        .write_frame(&HarnessOutputMessage::deliver(Event::AgentReplayComplete(
            tau_proto::AgentReplayComplete {
                agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
                session_id: Some("s1".into()),
                error: None,
            },
        )))
        .expect("agent replay boundary");
    writer.flush().expect("flush");
    let metadata = loop {
        let event = reader.read_event().expect("read").expect("metadata event");
        if let Event::AgentMetadataSet(metadata) = event {
            break metadata;
        }
    };
    writer
        .write_event(&Event::AgentMetadataSet(metadata))
        .expect("commit metadata");
    writer.flush().expect("flush metadata");

    let mut saw_cwd_context = false;
    loop {
        let event = reader.read_event().expect("read").expect("context event");
        match event {
            Event::ExtAgentContextPublish(publish) if publish.key.as_ref() == "workdir" => {
                saw_cwd_context = true;
            }
            Event::ExtensionContextReady(ready) => {
                assert!(saw_cwd_context, "ready must follow cwd context publish");
                assert_eq!(ready.session_id, "s1");
                assert_eq!(ready.agent_id.as_str(), "agent-1");
                break;
            }
            _ => {}
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_reads_file() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("README.txt");
    fs::write(&file_path, "hello from file").expect("write fixture");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(READ_TOOL_NAME),
            arguments: CborValue::Map(vec![(
                CborValue::Text("path".to_owned()),
                CborValue::Text(file_path.display().to_string()),
            )]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    assert_eq!(result.tool_name, READ_TOOL_NAME);
    assert_eq!(
        optional_argument_text(&result.result, "line-numbered content"),
        Ok(Some("1(no_nl) hello from file".to_owned()))
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_read_missing_file_reports_error() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(READ_TOOL_NAME),
            arguments: CborValue::Map(vec![(
                CborValue::Text("path".to_owned()),
                CborValue::Text("/definitely/missing/file.txt".to_owned()),
            )]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert!(!error.message.contains("failed to read"));
    assert!(error.message.contains("No such file or directory"));
    assert!(
        error.details.is_none(),
        "read errors should not echo arguments"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn edit_result_reports_minimal_status_without_model_diff() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("output.txt");
    fs::write(&file_path, "alpha beta gamma\nsame\n").expect("write fixture");

    let output = edit_file(&edit_arguments(
        &file_path,
        vec![context_line_edit(
            1,
            1,
            "alpha BETA gamma\n",
            "alpha beta gamma",
        )],
    ))
    .expect("edit");

    assert!(cbor_map_field(&output.result, "path").is_none());
    assert_eq!(cbor_int_field(&output.result, "edits"), Some(1));
    assert_eq!(cbor_bool_field(&output.result, "changed"), Some(true));
    assert_eq!(
        cbor_int_field(&output.result, "new_max_valid_start_line"),
        Some(3)
    );
    assert!(cbor_map_field(&output.result, "available_lines").is_none());
    assert!(cbor_map_field(&output.result, "max_valid_start_line").is_none());
    assert_eq!(cbor_int_field(&output.result, "total_bytes"), Some(22));
    assert!(cbor_map_text(&output.result, "output").is_none());
    assert!(cbor_map_text(&output.result, "diff").is_none());
    assert!(matches!(
        output.display.payload,
        Some(ToolUsePayload::Diff(_))
    ));
}

#[test]
fn edit_self_replacement_counts_without_diff() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "same\n").expect("write fixture");

    let output = edit_file(&edit_arguments(
        &file_path,
        vec![context_line_edit(1, 1, "same\n", "same")],
    ))
    .expect("edit");

    assert!(cbor_map_field(&output.result, "path").is_none());
    assert_eq!(cbor_int_field(&output.result, "edits"), Some(1));
    assert_eq!(cbor_bool_field(&output.result, "changed"), Some(false));
    assert_eq!(
        cbor_int_field(&output.result, "new_max_valid_start_line"),
        Some(2)
    );
    assert_eq!(cbor_int_field(&output.result, "total_bytes"), Some(5));
    assert!(cbor_map_text(&output.result, "output").is_none());
    assert!(output.display.payload.is_none());
}

#[test]
fn edit_new_file_reports_created_as_changed() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("new.txt");

    let result = edit_file(&edit_arguments(
        &file_path,
        vec![context_half_open_edit(1, 1, "created\n", "")],
    ))
    .expect("edit")
    .result;

    assert_eq!(cbor_int_field(&result, "edits"), Some(1));
    assert_eq!(cbor_bool_field(&result, "changed"), Some(true));
    assert_eq!(cbor_int_field(&result, "new_max_valid_start_line"), Some(2));
    assert_eq!(cbor_int_field(&result, "total_bytes"), Some(8));
    assert!(cbor_map_text(&result, "output").is_none());
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "created\n"
    );
}

#[test]
fn edit_existing_symlink_updates_target() {
    let tempdir = TempDir::new().expect("tempdir");
    let target_path = tempdir.path().join("target.txt");
    let link_path = tempdir.path().join("link.txt");
    fs::write(&target_path, "old\n").expect("write fixture");
    symlink("target.txt", &link_path).expect("symlink");

    let result = edit_file(&edit_arguments(
        &link_path,
        vec![context_line_edit(1, 1, "new\n", "old")],
    ))
    .expect("edit")
    .result;

    assert_eq!(cbor_bool_field(&result, "changed"), Some(true));
    assert_eq!(
        fs::read_to_string(&target_path).expect("read target"),
        "new\n"
    );
}

#[test]
fn edit_dangling_symlink_creates_target() {
    let tempdir = TempDir::new().expect("tempdir");
    let target_path = tempdir.path().join("target.txt");
    let link_path = tempdir.path().join("link.txt");
    symlink("target.txt", &link_path).expect("symlink");

    let result = edit_file(&edit_arguments(
        &link_path,
        vec![context_half_open_edit(1, 1, "", "")],
    ))
    .expect("edit")
    .result;

    assert_eq!(cbor_bool_field(&result, "changed"), Some(true));
    assert_eq!(cbor_int_field(&result, "total_bytes"), Some(0));
    assert_eq!(fs::read_to_string(&target_path).expect("read target"), "");
}

#[test]
fn edit_context_line_rejects_invalid_utf8_original_line() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("invalid.bin");
    fs::write(&file_path, b"abc\xffdef\nsecond\n").expect("write fixture");

    let error = edit_file(&edit_arguments(
        &file_path,
        vec![context_line_edit(1, 1, "FIRST\n", "abc�def")],
    ))
    .expect_err("invalid UTF-8 context_line should fail");

    assert_eq!(
        error.message,
        "context_line wrong - current line 1 is not valid UTF-8, so no context_line string can match it; see current content in the response"
    );
    let details = error.details.as_deref().expect("details");
    assert_eq!(
        cbor_map_text(details, "line-numbered content"),
        Some("1(invalid-utf8) abc�def\n2 second")
    );
    assert_eq!(cbor_bool_field(details, "valid_utf8"), Some(false));
    assert_eq!(
        fs::read(&file_path).expect("read back"),
        b"abc\xffdef\nsecond\n"
    );
}
#[test]
fn edit_rejects_edit_request_over_cap() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    let edits = (0..=100)
        .map(|_| context_half_open_edit(1, 1, "x", ""))
        .collect::<Vec<_>>();

    let error = edit_file(&edit_arguments(&file_path, edits))
        .expect_err("edit should reject over-cap request");

    assert_eq!(error.message, "requested edit count exceeds limit of 100");
    assert!(!file_path.exists());
}

#[test]
fn edit_rejects_edit_request_over_cap_before_reading_file() {
    let edits = (0..=100)
        .map(|_| context_half_open_edit(1, 1, "x", ""))
        .collect::<Vec<_>>();
    let args = cbor_map(vec![
        (
            "path",
            CborValue::Text("/definitely/missing/edit-target.txt".to_owned()),
        ),
        ("edits", CborValue::Array(edits)),
    ]);

    let error = edit_file(&args).expect_err("edit should reject arguments first");

    assert_eq!(error.message, "requested edit count exceeds limit of 100");
}

#[test]
fn edit_rejects_overlapping_ranges_without_partial_write() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "aa\nbb\ncc\n").expect("write fixture");

    let error = edit_file(&edit_arguments(
        &file_path,
        vec![
            context_line_edit(1, 2, "x\n", "aa"),
            context_line_edit(2, 2, "y\n", "aa"),
        ],
    ))
    .expect_err("overlap should fail");

    assert_eq!(error.message, "overlapping edits");
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "aa\nbb\ncc\n"
    );
}

#[test]
fn extension_edit_creates_file() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("output.txt");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(EDIT_TOOL_NAME),
            arguments: edit_arguments(
                &file_path,
                vec![context_half_open_edit(1, 1, "written content", "")],
            ),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    assert_eq!(result.tool_name, EDIT_TOOL_NAME);
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "written content\n"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_edit_rejects_oversized_existing_file_before_mutation() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("large.txt");
    let file = fs::File::create(&file_path).expect("create large file");
    file.set_len(TEST_SAFE_FILE_READ_LIMIT + 1)
        .expect("make sparse large file");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-oversized-edit".into(),
            tool_name: tau_proto::ToolName::new(EDIT_TOOL_NAME),
            arguments: edit_arguments(&file_path, vec![context_half_open_edit(1, 1, "x", "")]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert_eq!(error.tool_name, EDIT_TOOL_NAME);
    assert!(error.message.contains("file is too large to read safely"));
    assert_eq!(
        fs::metadata(&file_path).expect("metadata").len(),
        TEST_SAFE_FILE_READ_LIMIT + 1
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_edit_missing_parent_reports_short_error() {
    let tempdir = TempDir::new().expect("tempdir");
    let missing_parent = tempdir.path().join("missing-parent");
    let file_path = missing_parent.join("child.txt");
    fs::write(&missing_parent, "not a dir").expect("write blocker");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(EDIT_TOOL_NAME),
            arguments: edit_arguments(&file_path, vec![context_half_open_edit(1, 1, "x", "")]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert_eq!(error.tool_name, EDIT_TOOL_NAME);
    assert!(!error.message.contains("failed to create directories"));
    assert!(!error.message.contains(file_path.to_string_lossy().as_ref()));
    assert!(error.message.contains("Not a directory"));

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_edit_directory_reports_short_error() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(EDIT_TOOL_NAME),
            arguments: edit_arguments(
                Path::new("/tmp"),
                vec![context_half_open_edit(1, 1, "x", "")],
            ),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert_eq!(error.tool_name, EDIT_TOOL_NAME);
    assert!(!error.message.contains("failed to write"));
    assert!(error.message.contains("Is a directory"));

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_edit_creates_directories() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("a/b/c/deep.txt");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(EDIT_TOOL_NAME),
            arguments: edit_arguments(
                &file_path,
                vec![context_half_open_edit(1, 1, "deep content", "")],
            ),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    assert!(matches!(result, Event::ToolResult(_)));
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "deep content\n"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_apply_patch_updates_file() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("patch.txt");
    fs::write(&file_path, "before\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n-before\n+after\n*** End Patch",
        file_path.display()
    );
    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-patch-1".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    assert_eq!(result.tool_name, APPLY_PATCH_TOOL_NAME);
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "after\n"
    );
    assert_eq!(
        result.result,
        CborValue::Text(format!(
            "Success. Updated the following files:\nM {}",
            file_path.display()
        ))
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_apply_patch_reports_context_mismatch_without_writing() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("patch.txt");
    fs::write(&file_path, "before\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n-missing\n+after\n*** End Patch",
        file_path.display()
    );
    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-patch-2".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert_eq!(error.tool_name, APPLY_PATCH_TOOL_NAME);
    assert!(error.message.contains("Failed to find expected lines"));
    assert!(
        error.details.is_none(),
        "apply_patch errors should not echo patch text"
    );
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "before\n"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// Ensures apply_patch escapes control characters in model-visible path fields
/// so malicious filenames cannot inject fake summary or error lines.
#[test]
fn extension_apply_patch_escapes_control_characters_in_paths() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("line\tbreak.txt");
    fs::write(&file_path, "before\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n-before\n+after\n*** End Patch",
        file_path.display()
    );
    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-patch-escaped-success".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    let CborValue::Text(output) = result.result else {
        panic!("expected text result");
    };
    assert!(
        output.contains("line\\tbreak.txt"),
        "escaped path missing: {output}"
    );
    assert!(
        !output.contains("line\tbreak.txt"),
        "path tab should be escaped in output: {output}"
    );

    let created_path = tempdir.path().join("created\tfile.txt");
    let missing_path = tempdir.path().join("missing\tfile.txt");
    let patch = format!(
        "*** Begin Patch\n*** Add File: {}\n+hello\n*** Update File: {}\n@@\n-old\n+new\n*** End Patch",
        created_path.display(),
        missing_path.display(),
    );
    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-patch-escaped-partial".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert!(
        error.message.contains("missing\\tfile.txt"),
        "escaped error path missing: {}",
        error.message
    );
    let details = error.details.expect("partial changes details");
    let CborValue::Map(entries) = details else {
        panic!("expected structured partial change details");
    };
    let partial_changes = entries
        .iter()
        .find_map(|(key, value)| match (key, value) {
            (CborValue::Text(key), CborValue::Array(changes)) if key == "partial_changes" => {
                Some(changes)
            }
            _ => None,
        })
        .expect("partial_changes detail");
    let CborValue::Map(change) = &partial_changes[0] else {
        panic!("expected partial change map");
    };
    assert!(change.iter().any(|(key, value)| matches!(
        (key, value),
        (CborValue::Text(key), CborValue::Text(value))
            if key == "path" && value.contains("created\\tfile.txt")
    )));

    let dir_path = tempdir.path().join("dir\tname");
    fs::create_dir(&dir_path).expect("create tab dir");
    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n-old\n+new\n*** End Patch",
        dir_path.display(),
    );
    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-patch-escaped-io-error".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert!(
        error.message.contains("dir\\tname"),
        "escaped diagnostic path missing: {}",
        error.message
    );
    assert!(
        !error.message.contains("dir\tname"),
        "embedded I/O diagnostic should not keep raw tabs: {}",
        error.message
    );

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-patch-escaped-invalid-op".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(
                "*** Begin Patch\n*** Bad\tOperation\n*** End Patch".to_owned(),
            ),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert!(
        error.message.contains("Bad\\tOperation"),
        "escaped invalid operation missing: {}",
        error.message
    );
    assert!(
        !error.message.contains("Bad\tOperation"),
        "invalid operation should not keep raw tabs: {}",
        error.message
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_apply_patch_move_renames_file() {
    let tempdir = TempDir::new().expect("tempdir");
    let src = tempdir.path().join("old.txt");
    let dst = tempdir.path().join("new.txt");
    fs::write(&src, "before\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n*** Move to: {}\n@@\n-before\n+after\n*** End Patch",
        src.display(),
        dst.display()
    );
    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-patch-3".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    assert!(matches!(result, Event::ToolResult(_)));
    assert!(!src.exists(), "source path should be removed after move");
    assert_eq!(fs::read_to_string(&dst).expect("read back"), "after\n");

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// Ensures apply_patch moves do not silently clobber an existing destination.
/// Move patches must fail before mutating either path so accidental data loss
/// is reported as a tool error instead of hidden in UI diff metadata.
#[test]
fn extension_apply_patch_move_rejects_existing_destination() {
    let tempdir = TempDir::new().expect("tempdir");
    let src = tempdir.path().join("old.txt");
    let dst = tempdir.path().join("new.txt");
    fs::write(&src, "before\n").expect("write src");
    fs::write(&dst, "existing\n").expect("write dst");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n*** Move to: {}\n@@\n-before\n+after\n*** End Patch",
        src.display(),
        dst.display()
    );
    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-patch-move-existing".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert_eq!(error.tool_name, APPLY_PATCH_TOOL_NAME);
    assert!(error.message.contains("Move destination already exists"));
    assert_eq!(fs::read_to_string(&src).expect("read src"), "before\n");
    assert_eq!(fs::read_to_string(&dst).expect("read dst"), "existing\n");

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_apply_patch_applies_multiple_operations() {
    let tempdir = TempDir::new().expect("tempdir");
    let add_path = tempdir.path().join("nested/new.txt");
    let modify_path = tempdir.path().join("modify.txt");
    let delete_path = tempdir.path().join("delete.txt");
    fs::write(&modify_path, "line1\nline2\n").expect("write modify");
    fs::write(&delete_path, "obsolete\n").expect("write delete");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let patch = format!(
        "*** Begin Patch\n*** Add File: {}\n+created\n*** Delete File: {}\n*** Update File: {}\n@@\n-line2\n+changed\n*** End Patch",
        add_path.display(),
        delete_path.display(),
        modify_path.display(),
    );
    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-patch-4".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    assert_eq!(result.tool_name, APPLY_PATCH_TOOL_NAME);
    assert_eq!(
        fs::read_to_string(&add_path).expect("read added"),
        "created\n"
    );
    assert_eq!(
        fs::read_to_string(&modify_path).expect("read modified"),
        "line1\nchanged\n"
    );
    assert!(!delete_path.exists(), "deleted path should be removed");
    assert_eq!(
        result.result,
        CborValue::Text(format!(
            "Success. Updated the following files:\nA {}\nM {}\nD {}",
            add_path.display(),
            modify_path.display(),
            delete_path.display(),
        ))
    );
    let display = result.display.expect("apply_patch display");
    let Some(ToolUsePayload::Diffs { files }) = display.payload else {
        panic!("expected multi-file structured diff payload");
    };
    assert_eq!(files.len(), 3);
    assert!(
        files
            .iter()
            .any(|file| file.path == add_path.display().to_string())
    );
    assert!(
        files
            .iter()
            .any(|file| file.path == delete_path.display().to_string())
    );
    let modify_diff = files
        .iter()
        .find(|file| file.path == modify_path.display().to_string())
        .expect("modify diff");
    assert!(
        modify_diff
            .diff
            .hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .any(|line| matches!(line, tau_proto::DiffLine::Modify { .. }))
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_apply_patch_applies_multiple_chunks() {
    let tempdir = TempDir::new().expect("tempdir");
    let target_path = tempdir.path().join("multi.txt");
    fs::write(&target_path, "line1\nline2\nline3\nline4\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n-line2\n+changed2\n@@\n-line4\n+changed4\n*** End Patch",
        target_path.display(),
    );
    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-patch-5".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    assert!(matches!(result, Event::ToolResult(_)));
    assert_eq!(
        fs::read_to_string(&target_path).expect("read back"),
        "line1\nchanged2\nline3\nchanged4\n"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_apply_patch_failure_after_partial_success_leaves_changes() {
    let tempdir = TempDir::new().expect("tempdir");
    let created_path = tempdir.path().join("created.txt");
    let missing_path = tempdir.path().join("missing.txt");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let patch = format!(
        "*** Begin Patch\n*** Add File: {}\n+hello\n*** Update File: {}\n@@\n-old\n+new\n*** End Patch",
        created_path.display(),
        missing_path.display(),
    );
    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-patch-5b".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert_eq!(error.tool_name, APPLY_PATCH_TOOL_NAME);
    assert!(error.message.contains("Failed to read file to update"));
    let details = error.details.expect("partial changes details");
    let CborValue::Map(entries) = details else {
        panic!("expected structured partial change details");
    };
    let partial_changes = entries
        .iter()
        .find_map(|(key, value)| match (key, value) {
            (CborValue::Text(key), CborValue::Array(changes)) if key == "partial_changes" => {
                Some(changes)
            }
            _ => None,
        })
        .expect("partial_changes detail");
    assert_eq!(partial_changes.len(), 1);
    let CborValue::Map(change) = &partial_changes[0] else {
        panic!("expected partial change map");
    };
    assert!(change.iter().any(|(key, value)| matches!(
        (key, value),
        (CborValue::Text(key), CborValue::Text(value)) if key == "status" && value == "A"
    )));
    assert!(change.iter().any(|(key, value)| matches!(
        (key, value),
        (CborValue::Text(key), CborValue::Text(value))
            if key == "path" && value == &created_path.display().to_string()
    )));
    let display = error.display.expect("error display");
    let Some(ToolUsePayload::Diff(diff)) = display.payload else {
        panic!("expected structured diff payload for partial apply_patch failure");
    };
    assert_eq!(diff.added, 1);
    assert_eq!(diff.removed, 0);
    assert!(
        diff.hunks
            .iter()
            .flat_map(|hunk| hunk.lines.iter())
            .any(|line| matches!(line, tau_proto::DiffLine::Add { text } if text == "hello")),
        "partial add should be visible in structured diff"
    );
    assert_eq!(
        fs::read_to_string(&created_path).expect("created file should remain"),
        "hello\n"
    );
    assert!(!missing_path.exists());

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_apply_patch_rejects_oversized_update_before_mutation() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("large.txt");
    let file = fs::File::create(&file_path).expect("create large file");
    file.set_len(TEST_SAFE_FILE_READ_LIMIT + 1)
        .expect("make sparse large file");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n-old\n+new\n*** End Patch",
        file_path.display(),
    );
    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-oversized-patch".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert_eq!(error.tool_name, APPLY_PATCH_TOOL_NAME);
    assert!(error.message.contains("file is too large to read safely"));
    assert_eq!(
        fs::metadata(&file_path).expect("metadata").len(),
        TEST_SAFE_FILE_READ_LIMIT + 1
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_apply_patch_requires_existing_file_for_update() {
    let tempdir = TempDir::new().expect("tempdir");
    let missing_path = tempdir.path().join("missing.txt");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n-old\n+new\n*** End Patch",
        missing_path.display(),
    );
    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-patch-6".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert_eq!(error.tool_name, APPLY_PATCH_TOOL_NAME);
    assert!(error.message.contains("Failed to read file to update"));
    assert!(
        error.details.is_none(),
        "apply_patch errors should not echo patch text"
    );
    assert!(
        error
            .message
            .contains(missing_path.to_string_lossy().as_ref())
    );
    assert!(!missing_path.exists());

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// Ensures apply_patch Add File rejects existing files instead of silently
/// overwriting content that required an explicit Update File hunk.
#[test]
fn extension_apply_patch_add_rejects_existing_file() {
    let tempdir = TempDir::new().expect("tempdir");
    let path = tempdir.path().join("duplicate.txt");
    fs::write(&path, "old content\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let patch = format!(
        "*** Begin Patch\n*** Add File: {}\n+new content\n*** End Patch",
        path.display(),
    );
    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-patch-7".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert!(
        error.message.contains("Add File target already exists"),
        "unexpected error: {}",
        error.message
    );
    assert_eq!(
        fs::read_to_string(&path).expect("read back"),
        "old content\n"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_apply_patch_update_appends_trailing_newline() {
    let tempdir = TempDir::new().expect("tempdir");
    let path = tempdir.path().join("no_newline.txt");
    fs::write(&path, "no newline at end").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n-no newline at end\n+first line\n+second line\n*** End Patch",
        path.display(),
    );
    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-patch-8".into(),
            tool_name: tau_proto::ToolName::new(APPLY_PATCH_TOOL_NAME),
            arguments: CborValue::Text(patch),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    assert!(matches!(result, Event::ToolResult(_)));
    let contents = fs::read_to_string(&path).expect("read back");
    assert!(contents.ends_with('\n'));
    assert_eq!(contents, "first line\nsecond line\n");

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn edit_rejects_missing_new_text() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "hello\nworld\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(EDIT_TOOL_NAME),
            arguments: edit_arguments(
                &file_path,
                vec![cbor_map(vec![
                    ("start_line", CborValue::Integer(1.into())),
                    ("end_line_exclusive", CborValue::Integer(2.into())),
                ])],
            ),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert_eq!(error.tool_name, EDIT_TOOL_NAME);
    assert_eq!(error.message, "each edit must have a string newText");

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn edit_rejects_negative_start_line_with_path_args() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "hello\nworld\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(EDIT_TOOL_NAME),
            arguments: edit_arguments(&file_path, vec![line_edit(-1, 1, "x")]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert_eq!(error.tool_name, EDIT_TOOL_NAME);
    assert_eq!(error.message, "start_line must be at least 1");
    assert!(
        error.details.is_none(),
        "edit errors should not echo arguments"
    );
    assert_eq!(
        error.display.expect("display").args,
        file_path.display().to_string()
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn edit_rejects_zero_end_line_exclusive() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "hello\n").expect("write");

    let error = edit_file(&edit_arguments(
        &file_path,
        vec![cbor_map(vec![
            ("start_line", CborValue::Integer(1.into())),
            ("end_line_exclusive", CborValue::Integer(0.into())),
            ("newText", CborValue::Text("x".to_owned())),
            ("context_line", CborValue::Text("hello".to_owned())),
        ])],
    ))
    .expect_err("end_line_exclusive=0 should fail");

    assert_eq!(error.message, "end_line_exclusive must be at least 1");
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "hello\n"
    );
}

#[test]
fn edit_rejects_end_line_exclusive_before_start_line() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "hello\nworld\n").expect("write");

    let error = edit_file(&edit_arguments(
        &file_path,
        vec![context_half_open_edit(3, 1, "x", "world")],
    ))
    .expect_err("end_line_exclusive before start_line should fail");

    assert_eq!(
        error.message,
        "end_line_exclusive must be at least start_line"
    );
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "hello\nworld\n"
    );
}

#[test]
fn edit_adds_missing_line_ending_before_following_content() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "before\ntarget\nafter\n").expect("write");

    let _result = edit_file(&edit_arguments(
        &file_path,
        vec![context_line_edit(2, 2, "replacement", "target")],
    ))
    .expect("replacement without line ending should be normalized")
    .result;
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "before\nreplacement\nafter\n"
    );
}

#[test]
fn edit_preserves_final_newline_at_end_of_file() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "before\ntarget\n").expect("write");

    let _result = edit_file(&edit_arguments(
        &file_path,
        vec![context_line_edit(2, 2, "replacement", "target")],
    ))
    .expect("last line replacement should preserve final newline")
    .result;

    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "before\nreplacement\n"
    );
}

#[test]
fn edit_preserves_original_crlf_when_adding_missing_line_ending() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, b"before\r\ntarget\r\nafter\r\n").expect("write");

    let _result = edit_file(&edit_arguments(
        &file_path,
        vec![context_line_edit(2, 2, "replacement", "target")],
    ))
    .expect("replacement should reuse original line ending")
    .result;
    assert_eq!(
        fs::read(&file_path).expect("read back"),
        b"before\r\nreplacement\r\nafter\r\n"
    );
}

#[test]
fn edit_noop_after_normalization_reports_unchanged() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "a\nb\n").expect("write");

    let result = edit_file(&edit_arguments(
        &file_path,
        vec![context_line_edit(1, 1, "a", "a")],
    ))
    .expect("normalization may make an edit a no-op")
    .result;

    assert_eq!(cbor_bool_field(&result, "changed"), Some(false));
    assert_eq!(fs::read_to_string(&file_path).expect("read back"), "a\nb\n");
}

#[test]
fn edit_deletion_before_following_content_does_not_add_newline_header() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "before\ntarget\nafter\n").expect("write");

    let _result = edit_file(&edit_arguments(
        &file_path,
        vec![context_line_edit(2, 2, "", "target")],
    ))
    .expect("deletion should not be normalized")
    .result;
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "before\nafter\n"
    );
}

#[test]
fn edit_preserves_original_cr_when_adding_missing_line_ending() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, b"before\rtarget\rafter\r").expect("write");

    let _result = edit_file(&edit_arguments(
        &file_path,
        vec![context_line_edit(2, 2, "replacement", "target")],
    ))
    .expect("replacement should reuse original CR line ending")
    .result;
    assert_eq!(
        fs::read(&file_path).expect("read back"),
        b"before\rreplacement\rafter\r"
    );
}
#[test]
fn edit_rejects_legacy_line_count() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "hello\n").expect("write");

    let error = edit_file(&edit_arguments(
        &file_path,
        vec![cbor_map(vec![
            ("start_line", CborValue::Integer(1.into())),
            ("end_line_exclusive", CborValue::Integer(2.into())),
            ("line_count", CborValue::Integer(1.into())),
            ("newText", CborValue::Text("x".to_owned())),
        ])],
    ))
    .expect_err("line_count should fail");

    assert_eq!(
        error.message,
        "line_count is no longer supported; use end_line_exclusive"
    );
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "hello\n"
    );
}

#[test]
fn edit_uses_original_line_numbers_for_multiple_replacements() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "a\nb\nc\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(EDIT_TOOL_NAME),
            arguments: edit_arguments(
                &file_path,
                vec![
                    context_line_edit(1, 1, "x\ny\n", "a"),
                    context_line_edit(3, 3, "z\n", "c"),
                ],
            ),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    assert_eq!(cbor_map_int(&result.result, "edits"), Some(2));
    assert_eq!(
        cbor_map_int(&result.result, "new_max_valid_start_line"),
        Some(5)
    );
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "x\ny\nb\nz\n"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn edit_replaces_exact_line_range() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "fish\nfish\nfish\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(EDIT_TOOL_NAME),
            arguments: edit_arguments(&file_path, vec![context_line_edit(2, 2, "cat\n", "fish")]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    let expected_args = format!("{} 2..<3", file_path.display());
    assert_eq!(
        result.display.as_ref().map(|display| display.args.as_str()),
        Some(expected_args.as_str())
    );
    assert_eq!(cbor_map_int(&result.result, "edits"), Some(1));
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "fish\ncat\nfish\n"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn edit_appends_to_line_after_trailing_newline() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "fish\n").expect("write");

    let result = edit_file(&edit_arguments(
        &file_path,
        vec![context_half_open_edit(2, 2, "cat\n", "")],
    ))
    .expect("edit")
    .result;

    assert_eq!(cbor_int_field(&result, "new_max_valid_start_line"), Some(3));
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "fish\ncat\n"
    );
}

#[test]
fn edit_half_open_replaces_line_range() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "one\ntwo\nthree\n").expect("write");

    let _result = edit_file(&edit_arguments(
        &file_path,
        vec![context_half_open_edit(2, 3, "TWO", "two")],
    ))
    .expect("boundary replacement should edit line 2")
    .result;
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "one\nTWO\nthree\n"
    );
}

#[test]
fn edit_half_open_inserts_at_top_and_middle() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "one\ntwo\n").expect("write");

    let output = edit_file(&edit_arguments(
        &file_path,
        vec![
            context_half_open_edit(1, 1, "zero", "one"),
            context_half_open_edit(2, 2, "middle", "two"),
        ],
    ))
    .expect("empty half-open ranges should insert");

    assert_eq!(
        output.display.args,
        format!("{} 1..<1,2..<2", file_path.display())
    );

    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "zero\none\nmiddle\ntwo\n"
    );
}

#[test]
fn edit_half_open_context_lines_empty_insertion_with_start_line() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "one\n").expect("write");

    edit_file(&edit_arguments(
        &file_path,
        vec![context_half_open_edit(1, 1, "zero\n", "one")],
    ))
    .expect("empty insertion at BOF should accept start-line context_line");

    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "zero\none\n"
    );
}

#[test]
fn edit_half_open_appends_after_file_with_trailing_newline() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "one\n").expect("write");

    edit_file(&edit_arguments(
        &file_path,
        vec![context_half_open_edit(2, 2, "two\n", "")],
    ))
    .expect("EOF insertion should not add blank line after existing line ending");

    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "one\ntwo\n"
    );
}

#[test]
fn edit_half_open_inserts_before_line_without_trailing_newline() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "one").expect("write");

    let _result = edit_file(&edit_arguments(
        &file_path,
        vec![context_half_open_edit(1, 1, "zero", "one")],
    ))
    .expect("insertion before unterminated content should stay line-oriented")
    .result;
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "zero\none"
    );
}

#[test]
fn edit_half_open_insertion_preserves_following_crlf() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, b"one\r\ntwo\r\n").expect("write");

    edit_file(&edit_arguments(
        &file_path,
        vec![context_half_open_edit(2, 2, "middle", "two")],
    ))
    .expect("boundary insertion should use following line ending style");

    assert_eq!(
        fs::read(&file_path).expect("read back"),
        b"one\r\nmiddle\r\ntwo\r\n"
    );
}

#[test]
fn edit_half_open_appends_after_file_without_trailing_newline() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "one").expect("write");

    let _result = edit_file(&edit_arguments(
        &file_path,
        vec![context_half_open_edit(2, 2, "two\n", "")],
    ))
    .expect("EOF insertion should keep line boundary")
    .result;
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "one\ntwo\n"
    );
}

#[test]
fn edit_half_open_creates_empty_file() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("missing.txt");

    edit_file(&edit_arguments(
        &file_path,
        vec![context_half_open_edit(1, 1, "hello\n", "")],
    ))
    .expect("half-open insertion should create missing file");

    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "hello\n"
    );
}

#[test]
fn edit_rejects_legacy_start_line_and_end_line() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "one\n").expect("write");

    let error = edit_file(&edit_arguments(
        &file_path,
        vec![cbor_map(vec![
            ("start_line", CborValue::Integer(1.into())),
            ("end_line", CborValue::Integer(1.into())),
            ("newText", CborValue::Text("x\n".to_owned())),
            ("context_line", CborValue::Text("one".to_owned())),
        ])],
    ))
    .expect_err("legacy edit ranges should fail");

    assert_eq!(
        error.message,
        "edit uses end_line_exclusive; to replace read output lines A through B, use start_line A and end_line_exclusive B+1"
    );
}

#[test]
fn edit_rejects_legacy_after_line_and_before_line() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "one\n").expect("write");

    let error = edit_file(&edit_arguments(
        &file_path,
        vec![cbor_map(vec![
            ("after_line", CborValue::Integer(0.into())),
            ("before_line", CborValue::Integer(2.into())),
            ("newText", CborValue::Text("x\n".to_owned())),
            ("context_line", CborValue::Text("one".to_owned())),
        ])],
    ))
    .expect_err("legacy boundary edit ranges should fail");

    assert_eq!(
        error.message,
        "after_line and before_line are no longer supported; use start_line and end_line_exclusive"
    );
}

#[test]
fn edit_replaces_empty_file_line_one() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "").expect("write");

    let result = edit_file(&edit_arguments(
        &file_path,
        vec![context_half_open_edit(1, 1, "hello\n", "")],
    ))
    .expect("edit")
    .result;

    assert_eq!(cbor_int_field(&result, "new_max_valid_start_line"), Some(2));
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "hello\n"
    );
}

#[test]
fn edit_rejects_end_line_exclusive_past_end() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "hello\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(EDIT_TOOL_NAME),
            arguments: edit_arguments(&file_path, vec![context_half_open_edit(2, 4, "x", "")]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let error = reader.read_event().expect("read").expect("error");
    let Event::ToolError(error) = error else {
        panic!("expected tool error");
    };
    assert_eq!(error.tool_name, EDIT_TOOL_NAME);
    assert!(
        error
            .message
            .contains("end_line_exclusive 4 is past end of file")
    );
    assert!(error.message.contains("max_valid_start_line: 2"));
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "hello\n"
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn edit_rejects_range_past_end_without_trailing_newline() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "hello").expect("write");

    let error = edit_file(&edit_arguments(
        &file_path,
        vec![context_line_edit(1, 2, "x", "hello")],
    ))
    .expect_err("range should fail");

    assert_eq!(
        error.message,
        "end_line_exclusive 3 is past end of file (max_valid_start_line: 2)"
    );
    assert_eq!(fs::read_to_string(&file_path).expect("read back"), "hello");
}

#[test]
fn edit_context_line_allows_matching_start_line() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "alpha\nbeta\ngamma\n").expect("write");

    // Regression coverage: context_line must match start_line's original content,
    // not the previous line.
    let result = edit_file(&edit_arguments(
        &file_path,
        vec![context_line_edit(2, 2, "BETA\n", "beta")],
    ))
    .expect("edit")
    .result;

    assert_eq!(cbor_int_field(&result, "new_max_valid_start_line"), Some(4));
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "alpha\nBETA\ngamma\n"
    );
}

#[test]
fn edit_context_line_rejects_previous_line_for_replacement() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "alpha\nbeta\ngamma\n").expect("write");

    // Prevents restoring the rejected behavior where line 2 replacements used
    // line 1 as their context_line anchor.
    let error = edit_file(&edit_arguments(
        &file_path,
        vec![context_line_edit(2, 2, "BETA\n", "alpha")],
    ))
    .expect_err("previous-line context should fail");

    assert_eq!(
        error.message,
        "context_line wrong - must equal current line 2, see current content in the response"
    );
    let details = error.details.as_deref().expect("details");
    assert_eq!(cbor_int_field(details, "context_line_number"), Some(2));
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "alpha\nbeta\ngamma\n"
    );
}

#[test]
fn edit_context_line_rejects_stale_line_number_and_returns_context_line_context() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    let original = (1..=100)
        .map(|line| format!("line {line:03}\n"))
        .collect::<String>();
    fs::write(&file_path, &original).expect("write");

    // On mismatch, the file stays untouched and the agent gets read-like
    // details around the line whose context_line failed. This gives enough context
    // to recover from stale line numbers without dumping context into the UI
    // payload.
    let error = edit_file(&edit_arguments(
        &file_path,
        vec![
            context_line_edit(1, 1, "LINE 001\n", "line 001"),
            context_line_edit(12, 40, "replacement\n", "wrong"),
        ],
    ))
    .expect_err("context_line mismatch should fail");

    let expected_context = (2..=22)
        .map(|line| format!("{line} line {line:03}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        error.message,
        "context_line wrong - must equal current line 12, see current content in the response"
    );
    let details = error.details.as_deref().expect("details");
    assert_eq!(
        cbor_map_text(details, "line-numbered content"),
        Some(expected_context.as_str())
    );
    assert_eq!(cbor_int_field(details, "context_line_number"), Some(12));
    assert_eq!(error.display.payload, None);
    assert_eq!(error.display.stats.lines, Some(21));
    assert_eq!(
        error.display.stats.bytes,
        Some(expected_context.len() as u64)
    );
    assert_eq!(error.display.stats.matches, None);
    assert_eq!(fs::read_to_string(&file_path).expect("read back"), original);
}

#[test]
fn edit_rejects_non_empty_context_line_for_missing_file_insertion() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("missing.txt");

    // Missing files have no start_line content, so creation must use an empty
    // context_line.
    let error = edit_file(&edit_arguments(
        &file_path,
        vec![context_half_open_edit(1, 1, "created\n", "not-empty")],
    ))
    .expect_err("non-empty context_line on missing-file insertion should fail");

    assert_eq!(
        error.message,
        "context_line wrong - must equal \"\" for missing line 1, see current content in the response"
    );
    assert!(!file_path.exists());
}

#[test]
fn edit_rejects_wrong_context_line_at_file_start_with_recovery_context() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "alpha\nbeta\n").expect("write");

    let error = edit_file(&edit_arguments(
        &file_path,
        vec![context_line_edit(1, 1, "ALPHA\n", "not-empty")],
    ))
    .expect_err("wrong context_line at file start should fail");

    assert_eq!(
        error.message,
        "context_line wrong - must equal current line 1, see current content in the response"
    );
    let details = error.details.as_deref().expect("details");
    assert_eq!(cbor_int_field(details, "context_line_number"), Some(1));
    assert_eq!(
        cbor_map_text(details, "line-numbered content"),
        Some("1 alpha\n2 beta")
    );
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "alpha\nbeta\n"
    );
}

#[test]
fn edit_empty_insertion_rejects_previous_line_as_context_line() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "one\ntwo\n").expect("write");

    let error = edit_file(&edit_arguments(
        &file_path,
        vec![context_half_open_edit(2, 2, "middle\n", "one")],
    ))
    .expect_err("empty insertion should reject previous-line context");

    assert_eq!(
        error.message,
        "context_line wrong - must equal current line 2, see current content in the response"
    );
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "one\ntwo\n"
    );
}

#[test]
fn edit_rejects_missing_context_line_without_writing() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "alpha\n").expect("write");

    let error = edit_file(&edit_arguments(
        &file_path,
        vec![line_edit(1, 1, "ALPHA\n")],
    ))
    .expect_err("missing context_line should fail");

    assert_eq!(error.message, "each edit must have a string context_line");
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "alpha\n"
    );
}

#[test]
fn edit_context_line_rejects_non_string_context_line_without_writing() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "alpha\n").expect("write");

    let error = edit_file(&edit_arguments(
        &file_path,
        vec![cbor_map(vec![
            ("start_line", CborValue::Integer(1.into())),
            ("end_line_exclusive", CborValue::Integer(2.into())),
            ("newText", CborValue::Text("ALPHA\n".to_owned())),
            ("context_line", CborValue::Integer(1.into())),
        ])],
    ))
    .expect_err("non-string context_line should fail");

    assert_eq!(error.message, "context_line must be a string");
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "alpha\n"
    );
}

#[test]
fn edit_context_line_trims_trailing_newline_characters() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");

    for context_line in ["beta\n", "beta\r", "beta\r\n", "beta\n\n"] {
        fs::write(&file_path, "alpha\nbeta\n").expect("write");
        edit_file(&edit_arguments(
            &file_path,
            vec![context_line_edit(2, 2, "BETA\n", context_line)],
        ))
        .expect("context_line with trailing newline should match");

        assert_eq!(
            fs::read_to_string(&file_path).expect("read back"),
            "alpha\nBETA\n"
        );
    }
}

#[test]
fn edit_context_line_rejects_embedded_newline_characters_without_writing() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "alpha\nbeta\n").expect("write");

    // Context lines describe one line's content only, so embedded line endings are
    // malformed instead of being treated as ordinary mismatching text.
    for context_line in ["al\npha", "al\rpha", "al\r\npha"] {
        let error = edit_file(&edit_arguments(
            &file_path,
            vec![context_line_edit(2, 2, "BETA\n", context_line)],
        ))
        .expect_err("context_line with embedded newline should fail");

        assert_eq!(
            error.message,
            "context_line must not include embedded newline characters"
        );
        assert_eq!(
            fs::read_to_string(&file_path).expect("read back"),
            "alpha\nbeta\n"
        );
    }
}
#[test]
fn edit_context_line_matches_crlf_line_without_ending() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "one\r\ntwo\r\n").expect("write");

    let result = edit_file(&edit_arguments(
        &file_path,
        vec![context_line_edit(2, 2, "TWO\r\n", "two")],
    ))
    .expect("edit")
    .result;

    assert_eq!(cbor_int_field(&result, "new_max_valid_start_line"), Some(3));
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "one\r\nTWO\r\n"
    );
}

#[test]
fn edit_context_line_allows_empty_append_line_after_trailing_newline() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "fish\n").expect("write");

    let result = edit_file(&edit_arguments(
        &file_path,
        vec![context_half_open_edit(2, 2, "cat\n", "")],
    ))
    .expect("edit")
    .result;

    assert_eq!(cbor_int_field(&result, "new_max_valid_start_line"), Some(3));
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "fish\ncat\n"
    );
}

#[test]
fn edit_handles_crlf_line_ranges() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.txt");
    fs::write(&file_path, "one\r\ntwo\r\n").expect("write");

    let result = edit_file(&edit_arguments(
        &file_path,
        vec![context_line_edit(2, 2, "TWO\r\n", "two")],
    ))
    .expect("edit")
    .result;

    assert_eq!(cbor_int_field(&result, "new_max_valid_start_line"), Some(3));
    assert_eq!(
        fs::read_to_string(&file_path).expect("read back"),
        "one\r\nTWO\r\n"
    );
}

#[test]
fn extension_finds_files() {
    let tempdir = TempDir::new().expect("tempdir");
    fs::create_dir_all(tempdir.path().join("src/nested")).expect("mkdir");
    fs::write(tempdir.path().join("src/lib.rs"), "pub fn one() {}\n").expect("write");
    fs::write(
        tempdir.path().join("src/nested/mod.rs"),
        "pub fn two() {}\n",
    )
    .expect("write");
    fs::write(tempdir.path().join("README.md"), "# hi\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(FIND_TOOL_NAME),
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("pattern".to_owned()),
                    CborValue::Text("**/*.rs".to_owned()),
                ),
                (
                    CborValue::Text("path".to_owned()),
                    CborValue::Text(tempdir.path().display().to_string()),
                ),
            ]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    assert_eq!(result.tool_name, FIND_TOOL_NAME);
    assert_eq!(cbor_int_field(&result.result, "matches"), Some(2));
    let output = cbor_map_text(&result.result, "output").expect("output");
    assert!(output.contains("src/lib.rs"));
    assert!(output.contains("src/nested/mod.rs"));
    assert!(!output.contains("README.md"));

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn extension_lists_directory_contents() {
    let tempdir = TempDir::new().expect("tempdir");
    fs::create_dir_all(tempdir.path().join("src")).expect("mkdir");
    fs::write(tempdir.path().join("README.md"), "# hi\n").expect("write");
    fs::write(tempdir.path().join(".env"), "SECRET=1\n").expect("write");

    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(LS_TOOL_NAME),
            arguments: CborValue::Map(vec![(
                CborValue::Text("path".to_owned()),
                CborValue::Text(tempdir.path().display().to_string()),
            )]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    assert_eq!(result.tool_name, LS_TOOL_NAME);
    assert_eq!(cbor_int_field(&result.result, "entries"), Some(3));
    let output = cbor_map_text(&result.result, "output").expect("output");
    assert!(output.contains(".env"));
    assert!(output.contains("README.md"));
    assert!(output.contains("src/"));

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn shell_tool_reports_progress_and_success() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(SHELL_TOOL_NAME),
            arguments: CborValue::Map(vec![(
                CborValue::Text("command".to_owned()),
                CborValue::Text("printf hello".to_owned()),
            )]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let progress = reader.read_event().expect("read").expect("progress");
    let Event::ToolProgressReported(progress) = progress else {
        panic!("expected tool progress");
    };
    assert_eq!(progress.display.expect("display").mode, "");

    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    assert_eq!(result.tool_name, SHELL_TOOL_NAME);
    assert_eq!(result.display.as_ref().expect("display").mode, "");
    assert_eq!(
        optional_argument_text(&result.result, "output"),
        Ok(Some("out(no_nl) hello".to_owned()))
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn gpt_shell_tool_reports_progress_and_success() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-gpt-shell".into(),
            tool_name: tau_proto::ToolName::new(GPT_SHELL_TOOL_NAME),
            arguments: CborValue::Map(vec![(
                CborValue::Text("command".to_owned()),
                CborValue::Text("printf hello".to_owned()),
            )]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let progress = reader.read_event().expect("read").expect("progress");
    let Event::ToolProgressReported(progress) = progress else {
        panic!("expected tool progress");
    };
    assert_eq!(progress.tool_name, GPT_SHELL_TOOL_NAME);
    assert_eq!(progress.display.expect("display").mode, "");

    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    assert_eq!(result.tool_name, GPT_SHELL_TOOL_NAME);
    assert_eq!(result.display.as_ref().expect("display").mode, "");
    assert_eq!(
        optional_argument_text(&result.result, "output"),
        Ok(Some("out(no_nl) hello".to_owned()))
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn shell_tool_applies_configured_prefix_and_command() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_frame(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::new("test-extension"),
            config: CborValue::Map(vec![(
                CborValue::Text("shell".to_owned()),
                CborValue::Map(vec![
                    (
                        CborValue::Text("prefix".to_owned()),
                        CborValue::Array(vec![
                            CborValue::Text("env".to_owned()),
                            CborValue::Text("TAU_SHELL_PREFIX_TEST=ok".to_owned()),
                        ]),
                    ),
                    (
                        CborValue::Text("command".to_owned()),
                        CborValue::Text("sh".to_owned()),
                    ),
                ]),
            )]),
            state_dir: None,
            secrets: std::collections::BTreeMap::new(),
        }))
        .expect("configure");
    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(SHELL_TOOL_NAME),
            arguments: CborValue::Map(vec![(
                CborValue::Text("command".to_owned()),
                CborValue::Text("printf %s \"$TAU_SHELL_PREFIX_TEST\"".to_owned()),
            )]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let _progress = reader.read_event().expect("read").expect("progress");
    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    assert_eq!(
        optional_argument_text(&result.result, "output"),
        Ok(Some("out(no_nl) ok".to_owned()))
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn command_isolation_preserves_explicit_environment() {
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c")
        .arg("printf %s \"${TAU_EXPLICIT_ENV_TEST-unset}\"")
        .env("TAU_EXPLICIT_ENV_TEST", "ok")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    crate::isolation::apply_command_isolation(&mut cmd);
    let output = cmd.output().expect("run env probe");
    assert!(output.status.success(), "env probe failed: {output:?}");
    assert_eq!(String::from_utf8(output.stdout).expect("utf8 stdout"), "ok");
}

#[test]
fn command_isolation_clears_cargo_build_environment() {
    let cargo_env_vars = [
        "CARGO",
        "CARGO_BIN_NAME",
        "CARGO_CRATE_NAME",
        "CARGO_MANIFEST_DIR",
        "CARGO_MANIFEST_LINKS",
        "CARGO_MANIFEST_PATH",
        "CARGO_PKG_AUTHORS",
        "CARGO_PKG_DESCRIPTION",
        "CARGO_PKG_HOMEPAGE",
        "CARGO_PKG_LICENSE",
        "CARGO_PKG_LICENSE_FILE",
        "CARGO_PKG_NAME",
        "CARGO_PKG_README",
        "CARGO_PKG_REPOSITORY",
        "CARGO_PKG_RUST_VERSION",
        "CARGO_PKG_VERSION",
        "CARGO_PKG_VERSION_MAJOR",
        "CARGO_PKG_VERSION_MINOR",
        "CARGO_PKG_VERSION_PATCH",
        "CARGO_PKG_VERSION_PRE",
        "CARGO_PRIMARY_PACKAGE",
        "OUT_DIR",
    ];
    let script = cargo_env_vars
        .iter()
        .map(|env_var| format!("printf '%s=%s\\n' {env_var} \"${{{env_var}-unset}}\""))
        .collect::<Vec<_>>()
        .join("; ");
    let expected = cargo_env_vars
        .iter()
        .map(|env_var| format!("{env_var}=unset\n"))
        .collect::<String>();

    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c")
        .arg(script)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for env_var in cargo_env_vars {
        cmd.env(env_var, "should-not-leak");
    }
    crate::isolation::apply_command_isolation(&mut cmd);
    let output = cmd.output().expect("run env probe");
    assert!(output.status.success(), "env probe failed: {output:?}");
    assert_eq!(
        String::from_utf8(output.stdout).expect("utf8 stdout"),
        expected
    );
}

#[test]
fn shell_extension_rejects_invalid_config() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_frame(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::new("test-extension"),
            config: CborValue::Map(vec![(
                CborValue::Text("shell".to_owned()),
                CborValue::Map(vec![(
                    CborValue::Text("prefix".to_owned()),
                    CborValue::Text("nope".to_owned()),
                )]),
            )]),
            state_dir: None,
            secrets: std::collections::BTreeMap::new(),
        }))
        .expect("configure");
    writer.flush().expect("flush");

    let error = loop {
        let message = reader.read_message().expect("read").expect("message");
        if let HarnessInputMessage::ConfigError(error) = message {
            break error;
        }
    };
    assert!(error.message.contains("invalid type"));

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[cfg(unix)]
#[test]
fn shell_extension_reports_config_error_for_insecure_dir_lock_state_dir() {
    use std::os::unix::fs::PermissionsExt;

    let tempdir = tempfile::TempDir::new().expect("tempdir");
    std::fs::set_permissions(tempdir.path(), std::fs::Permissions::from_mode(0o755))
        .expect("chmod tempdir");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_frame(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::new("test-extension"),
            config: CborValue::Map(vec![(
                CborValue::Text("dir_lock".to_owned()),
                CborValue::Map(vec![
                    (CborValue::Text("enable".to_owned()), CborValue::Bool(true)),
                    (
                        CborValue::Text("backend".to_owned()),
                        CborValue::Text("filesystem".to_owned()),
                    ),
                    (
                        CborValue::Text("state_dir".to_owned()),
                        CborValue::Text(tempdir.path().display().to_string()),
                    ),
                ]),
            )]),
            state_dir: None,
            secrets: std::collections::BTreeMap::new(),
        }))
        .expect("configure");
    writer.flush().expect("flush");

    let error = loop {
        let message = reader.read_message().expect("read").expect("message");
        if let HarnessInputMessage::ConfigError(error) = message {
            break error;
        }
    };
    assert!(error.message.contains("must be private"));

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// Ensures the directory-lock mechanism remains opt-in while read-only bind
/// mount enforcement defaults on once that mechanism is enabled.
#[test]
fn shell_enforce_ro_bind_defaults_true_under_dir_lock_config() {
    assert!(ExtConfig::default().dir_lock.enforce_ro_bind);

    let config = serde_json::from_value::<ExtConfig>(serde_json::json!({
        "dir_lock": {
            "enforce_ro_bind": false,
        },
    }))
    .expect("parse enforce_ro_bind config");
    assert!(!config.dir_lock.enforce_ro_bind);
}

#[test]
fn shell_working_directory_cannot_change_after_startup() {
    let current = ExtConfig {
        working_directory: Some(PathBuf::from("/srv/one")),
        ..Default::default()
    };
    let same = ExtConfig {
        working_directory: Some(PathBuf::from("/srv/one")),
        ..Default::default()
    };
    let changed = ExtConfig {
        working_directory: Some(PathBuf::from("/srv/two")),
        ..Default::default()
    };

    apply_working_directory(&current, &same, false).expect("same cwd is idempotent");
    let err = apply_working_directory(&current, &changed, false).expect_err("cwd change rejected");
    assert!(err.contains("cannot be changed after startup"));
}

#[test]
fn shell_working_directory_cannot_be_set_after_runtime_events() {
    // A late None -> Some transition would mutate process-global cwd while
    // workers may already be resolving relative paths, so it must be rejected.
    let current = ExtConfig::default();
    let next = ExtConfig {
        working_directory: Some(PathBuf::from("/srv/late")),
        ..Default::default()
    };

    let err = apply_working_directory(&current, &next, true).expect_err("late cwd set rejected");

    assert!(err.contains("cannot be set after runtime events"));
}

#[test]
fn shell_extension_reports_invalid_working_directory_config() {
    // `working_directory` is applied by ext-shell itself after Configure. A bad
    // path should surface as ConfigError instead of silently leaving relative
    // filesystem tools rooted at an unexpected directory.
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    let td = TempDir::new().expect("tempdir");
    let missing_dir = td.path().join("missing");

    writer
        .write_frame(&HarnessOutputMessage::Configure(tau_proto::Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::new("test-extension"),
            config: cbor_text_map(vec![(
                "working_directory",
                missing_dir.to_str().expect("utf8 temp path"),
            )]),
            state_dir: None,
            secrets: std::collections::BTreeMap::new(),
        }))
        .expect("configure");
    writer.flush().expect("flush");

    let error = loop {
        let message = reader.read_message().expect("read").expect("message");
        if let HarnessInputMessage::ConfigError(error) = message {
            break error;
        }
    };
    assert!(
        error
            .message
            .contains("failed to set ext-shell working_directory")
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

#[test]
fn shell_hidden_read_write_mode_is_unrestricted_by_default() {
    // With directory locking disabled, shell calls run read-write and publish no
    // access-mode chip.
    let td = TempDir::new().expect("tempdir");
    let args = CborValue::Map(vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text("printf ok > probe".to_owned()),
        ),
        (
            CborValue::Text("cwd".to_owned()),
            CborValue::Text(td.path().to_string_lossy().into_owned()),
        ),
    ]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &crate::config::ShellConfig::default(),
        crate::tools::shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    assert_eq!(output.display.mode, "");
    assert_eq!(
        fs::read_to_string(td.path().join("probe")).expect("probe"),
        "ok"
    );
}

#[test]
fn shell_tool_multiline_display_uses_short_args_and_text_payload() {
    let args = CborValue::Map(vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text("printf hello\nprintf world".to_owned()),
        ),
        (
            CborValue::Text("timeout".to_owned()),
            CborValue::Integer(5.into()),
        ),
    ]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &crate::config::ShellConfig::default(),
        crate::tools::shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    assert_eq!(output.display.mode, "");
    assert_eq!(output.display.args, "printf hello");
    assert_eq!(
        output.display.payload,
        Some(tau_proto::ToolUsePayload::Text {
            text: "printf hello\nprintf world".to_owned(),
        })
    );
}

/// A one-line shell command shortened in the display header remains available
/// as a full Unicode text payload in both progress and terminal descriptors so
/// `show-tools=full` can render the omitted middle.
#[test]
fn shell_tool_long_display_args_include_full_text_payload() {
    let command = "αβγδεζηθικλμνξοπρστυφχψω一二三四五六七八九十甲乙丙丁戊己庚辛壬癸";
    let args = CborValue::Map(vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text(command.to_owned()),
        ),
        (
            CborValue::Text("timeout".to_owned()),
            CborValue::Integer(5.into()),
        ),
    ]);
    let expected_args = "αβγδεζηθικλμνξοπρστυ┄一二三四五六七八九十甲乙丙丁戊己庚辛壬癸";
    let expected_payload = Some(tau_proto::ToolUsePayload::Text {
        text: command.to_owned(),
    });
    let initial = crate::tools::shell::initial_display(
        &args,
        crate::tools::shell::ShellCommandMode::READ_WRITE_HIDDEN,
    );
    assert_eq!(initial.args, expected_args);
    assert_eq!(initial.payload, expected_payload);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &crate::config::ShellConfig::default(),
        crate::tools::shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    assert_eq!(output.display.mode, "");
    assert_eq!(output.display.args, expected_args);
    assert_eq!(output.display.payload, expected_payload);
}

/// A one-line shell command that fits the display header does not gain a
/// redundant payload body.
#[test]
fn shell_tool_short_display_args_omit_redundant_text_payload() {
    let args = CborValue::Map(vec![(
        CborValue::Text("command".to_owned()),
        CborValue::Text("printf hello".to_owned()),
    )]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &crate::config::ShellConfig::default(),
        crate::tools::shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    assert_eq!(output.display.args, "printf hello");
    assert_eq!(output.display.payload, None);
}

#[test]
fn shell_tool_use_state_mode_can_show_inferred_access_mode() {
    // When directory locking is enabled, the CLI can render ext-shell's inferred
    // shell access mode separately from display args.
    let args = CborValue::Map(vec![(
        CborValue::Text("command".to_owned()),
        CborValue::Text("printf hello".to_owned()),
    )]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &crate::config::ShellConfig::default(),
        crate::tools::shell::ShellCommandMode::visible(
            crate::tools::shell::ShellAccessMode::ReadOnly,
        ),
        false,
        None,
    )
    .expect("run") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    assert_eq!(output.display.mode, "ro");
    assert_eq!(output.display.args, "printf hello");
}

#[test]
fn shell_tool_marks_invalid_utf8_stdout_line_and_marks_output_invalid() {
    // Regression coverage for agent-facing shell output collection: stdout
    // can contain arbitrary bytes, and read_to_string used to drop all output
    // after the first invalid UTF-8 sequence.
    let args = CborValue::Map(vec![(
        CborValue::Text("command".to_owned()),
        CborValue::Text("printf '\\377stdout'".to_owned()),
    )]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &crate::config::ShellConfig::default(),
        crate::tools::shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    assert_eq!(
        cbor_map_text(&output.result, "output"),
        Some("out(invalid-utf8,no_nl) �stdout")
    );
    assert_eq!(cbor_bool_field(&output.result, "valid_utf8"), Some(false));
}

#[test]
fn shell_tool_replaces_invalid_utf8_stderr_and_marks_output_invalid() {
    // Regression coverage for agent-facing shell output collection: stderr
    // must be decoded lossily too, with a warning that does not erase the
    // original stderr text.
    let args = CborValue::Map(vec![(
        CborValue::Text("command".to_owned()),
        CborValue::Text("printf '\\376stderr' >&2".to_owned()),
    )]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &crate::config::ShellConfig::default(),
        crate::tools::shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    assert_eq!(
        cbor_map_text(&output.result, "output"),
        Some("err(invalid-utf8,no_nl) �stderr")
    );
    assert_eq!(cbor_bool_field(&output.result, "valid_utf8"), Some(false));
}

#[test]
fn shell_tool_replaces_invalid_utf8_both_streams_in_combined_output() {
    // Regression coverage for commands that write invalid bytes to both pipes:
    // the agent should see both decoded streams and one concise warning.
    let args = CborValue::Map(vec![(
        CborValue::Text("command".to_owned()),
        CborValue::Text("printf '\\377stdout'; printf '\\376stderr' >&2".to_owned()),
    )]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &crate::config::ShellConfig::default(),
        crate::tools::shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    assert_eq!(
        cbor_map_text(&output.result, "output"),
        Some("out(invalid-utf8,no_nl) �stdout\nerr(invalid-utf8,no_nl) �stderr")
    );
    assert_eq!(cbor_bool_field(&output.result, "valid_utf8"), Some(false));
}

#[test]
fn shell_tool_marks_crlf_and_cr_line_endings() {
    // Keep shell output line markers aligned with `read`: raw carriage
    // returns should not leak into agent-visible output.
    let args = CborValue::Map(vec![(
        CborValue::Text("command".to_owned()),
        CborValue::Text("printf 'a\r\nb\rc\n'".to_owned()),
    )]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &crate::config::ShellConfig::default(),
        crate::tools::shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    assert_eq!(
        cbor_map_text(&output.result, "output"),
        Some("out(crlf) a\nout(cr) b\nout c")
    );
}

#[test]
fn shell_tool_omits_truncation_marker_without_truncation() {
    // Compatibility metadata should stay sparse: total/truncated fields are
    // only present when a stream was actually truncated.
    let args = CborValue::Map(vec![(
        CborValue::Text("command".to_owned()),
        CborValue::Text("printf 'ok\\n'".to_owned()),
    )]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &crate::config::ShellConfig::default(),
        crate::tools::shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    assert_eq!(cbor_map_text(&output.result, "output"), Some("out ok"));
    let field = "truncated";
    assert!(
        cbor_map_field(&output.result, field).is_none(),
        "{field} should be absent without truncation"
    );
}

#[test]
fn shell_tool_reports_truncation_marker_and_original_totals() {
    // Regression coverage for shell truncation: agents need an explicit stderr
    // warning plus original stream totals, while legacy line/byte counts remain
    // stats for the returned (truncated and warning-prefixed) content.
    let line_count = MAX_OUTPUT_LINES + 1;
    let command = format!(
        "i=0; while [ \"$i\" -lt {line_count} ]; do printf 'x\\n'; printf 'e\\n' >&2; i=$((i + 1)); done"
    );
    let args = CborValue::Map(vec![(
        CborValue::Text("command".to_owned()),
        CborValue::Text(command),
    )]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &crate::config::ShellConfig::default(),
        crate::tools::shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    let combined = cbor_map_text(&output.result, "output").expect("output");
    assert!(combined.starts_with("out x") || combined.starts_with("err e"));
    assert!(combined.contains("\n...\n"));
    assert!(combined.contains("\nout x") || combined.contains("\nerr e"));
    assert_eq!(
        cbor_int_field(&output.result, "total_lines"),
        Some((line_count * 2) as i128)
    );
    assert!(cbor_int_field(&output.result, "total_bytes").is_some());
    assert_eq!(cbor_bool_field(&output.result, "truncated"), Some(true));
}

#[test]
fn shell_tool_marks_invalid_utf8_and_truncation_together() {
    // When multiple shell-side warnings apply, keep them outside the stream
    // marker and in a deterministic order before stderr content.
    let line_count = MAX_OUTPUT_LINES + 1;
    let command = format!(
        "printf '\\377'; i=0; while [ \"$i\" -lt {line_count} ]; do printf 'x\\n'; i=$((i + 1)); done"
    );
    let args = CborValue::Map(vec![(
        CborValue::Text("command".to_owned()),
        CborValue::Text(command),
    )]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &crate::config::ShellConfig::default(),
        crate::tools::shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    assert_eq!(cbor_bool_field(&output.result, "valid_utf8"), Some(false));
    assert_eq!(cbor_bool_field(&output.result, "truncated"), Some(true));
}

#[test]
fn shell_tool_runs_in_requested_cwd() {
    // Regression coverage for the schema-exposed cwd argument: the execution
    // path already supports it, and the shell must actually start there.
    let tempdir = TempDir::new().expect("tempdir");
    let args = CborValue::Map(vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text("pwd".to_owned()),
        ),
        (
            CborValue::Text("cwd".to_owned()),
            CborValue::Text(tempdir.path().display().to_string()),
        ),
    ]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &crate::config::ShellConfig::default(),
        crate::tools::shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    let cwd = tempdir.path().canonicalize().expect("canonical cwd");
    let expected_stdout = format!("out {}", cwd.display());
    assert_eq!(
        cbor_map_text(&output.result, "output"),
        Some(expected_stdout.as_str())
    );
    assert!(cbor_map_text(&output.result, "cwd").is_none());
}

#[test]
fn shell_tool_timeout_preserves_partial_output() {
    let args = CborValue::Map(vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text("printf 'before\\n'; sleep 2; printf 'after\\n'".to_owned()),
        ),
        (
            CborValue::Text("timeout".to_owned()),
            CborValue::Integer(1.into()),
        ),
    ]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &crate::config::ShellConfig::default(),
        crate::tools::shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("timeout result") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    assert_eq!(output.display.status, ToolUseStatus::Error);
    assert_eq!(output.display.status_text, "timeout");
    assert_eq!(cbor_map_text(&output.result, "output"), Some("out before"));
    assert!(cbor_map_field(&output.result, "total_lines").is_none());
    assert_eq!(cbor_bool_field(&output.result, "timed_out"), Some(true));
    assert!(cbor_int_field(&output.result, "timeout_secs").is_none());
    assert_eq!(
        cbor_map_text(&output.result, "termination_reason"),
        Some("timeout")
    );
}

#[cfg(unix)]
#[test]
fn shell_tool_returns_after_foreground_exit_even_if_background_holds_pipe() {
    // Regression coverage for background pipe holders: once the foreground
    // shell exits, inherited stdout fds in background jobs must not make the
    // shell tool wait for pipe EOF or capture late output.
    let args = CborValue::Map(vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text("(sleep 5; printf late) & printf early".to_owned()),
        ),
        (
            CborValue::Text("timeout".to_owned()),
            CborValue::Integer(1.into()),
        ),
    ]);

    let started = std::time::Instant::now();
    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &crate::config::ShellConfig::default(),
        crate::tools::shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "background pipe holder delayed shell result for {elapsed:?}"
    );
    let output = cbor_map_text(&output.result, "output").expect("output");
    assert_eq!(output, "out(no_nl) early");
    assert!(!output.contains("late"));
}

#[cfg(unix)]
#[test]
fn user_shell_returns_after_foreground_exit_even_if_background_holds_pipe() {
    // Regression coverage for user `!` shell dispatch: detached descendants can
    // inherit stdout after the foreground shell exits, but UI command completion
    // must not wait for pipe EOF or capture late background output.
    if !std::process::Command::new("sh")
        .arg("-c")
        .arg("command -v setsid >/dev/null")
        .status()
        .is_ok_and(|status| status.success())
    {
        return;
    }

    let (tx, rx) = std::sync::mpsc::channel();
    let cmd = tau_proto::UiShellCommand {
        session_id: "s1".into(),
        command_id: "ui-sh-bg".into(),
        command: "setsid sh -c 'sleep 5; printf late' & printf early".to_owned(),
        include_in_context: true,
        target_agent_id: None,
    };

    let started = std::time::Instant::now();
    let output = Output::channel(tx);
    let (_cancel_tx, cancel_rx) = std::sync::mpsc::channel();
    crate::tools::shell::dispatch_user_shell_command(
        cmd,
        crate::config::ShellConfig::default(),
        &output,
        cancel_rx,
        std::env::current_dir().expect("current dir"),
    );
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "background pipe holder delayed user shell result for {elapsed:?}"
    );

    let mut finished = None;
    for message in rx.try_iter() {
        if let HarnessInputMessage::Emit(emit) = message
            && let Event::ShellCommandFinished(event) = *emit.event
        {
            finished = Some(event);
        }
    }
    let finished = finished.expect("finished event");
    assert_eq!(finished.output, "early");
    assert!(!finished.output.contains("late"));
    assert_eq!(finished.exit_code, Some(0));
    assert!(!finished.cancelled);
}

#[cfg(unix)]
#[test]
fn shell_tool_timeout_returns_without_waiting_for_escaped_pipe_holder() {
    // Regression coverage for timeout with an escaped pipe holder: process-group
    // kill does not reach a setsid child, but timeout return must still be
    // independent from that child's inherited stdout pipe closing.
    if !std::process::Command::new("sh")
        .arg("-c")
        .arg("command -v setsid >/dev/null")
        .status()
        .is_ok_and(|status| status.success())
    {
        return;
    }

    let args = CborValue::Map(vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text(
                "setsid sh -c 'sleep 5; printf late' & printf early; sleep 5".to_owned(),
            ),
        ),
        (
            CborValue::Text("timeout".to_owned()),
            CborValue::Integer(1.into()),
        ),
    ]);

    let started = std::time::Instant::now();
    let CommandOutcome::Finished(result) = run_command_live(
        &args,
        &crate::config::ShellConfig::default(),
        crate::tools::shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("timeout result") else {
        panic!("expected finished shell outcome");
    };
    let result = *result;
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "escaped pipe holder delayed timeout result for {elapsed:?}"
    );
    assert_eq!(result.display.status, ToolUseStatus::Error);
    assert_eq!(result.display.status_text, "timeout");
    let output = cbor_map_text(&result.result, "output").expect("output");
    assert_eq!(output, "out(no_nl) early");
    assert!(!output.contains("late"));
    assert_eq!(cbor_bool_field(&result.result, "timed_out"), Some(true));
    assert_eq!(
        cbor_map_text(&result.result, "termination_reason"),
        Some("timeout")
    );
}

#[test]
fn shell_tool_bounded_huge_output_reports_original_totals() {
    // The shell reader keeps only a bounded tail in memory while counting the
    // original stream, so huge stdout still reports total bytes and truncation.
    let byte_count = MAX_OUTPUT_BYTES * 4 + 123;
    let command = format!("yes x | head -c {byte_count}");
    let args = CborValue::Map(vec![(
        CborValue::Text("command".to_owned()),
        CborValue::Text(command),
    )]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &crate::config::ShellConfig::default(),
        crate::tools::shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("run") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    let combined = cbor_map_text(&output.result, "output").expect("output");
    assert!(combined.contains("..."));
    assert!(combined.len() < byte_count);
    assert_eq!(cbor_bool_field(&output.result, "truncated"), Some(true));
    assert!(cbor_int_field(&output.result, "total_bytes").is_some());
}

#[test]
fn shell_tool_timeout_zero_is_immediate_timeout() {
    // A zero timeout is valid and means the child should be killed as soon as
    // timeout accounting observes that it has not already exited.
    let args = CborValue::Map(vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text("sleep 1".to_owned()),
        ),
        (
            CborValue::Text("timeout".to_owned()),
            CborValue::Integer(0.into()),
        ),
    ]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &crate::config::ShellConfig::default(),
        crate::tools::shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("timeout result") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    assert_eq!(output.display.status, ToolUseStatus::Error);
    assert_eq!(output.display.status_text, "timeout");
    assert_eq!(cbor_bool_field(&output.result, "timed_out"), Some(true));
    assert!(cbor_int_field(&output.result, "timeout_secs").is_none());
    assert_eq!(
        cbor_map_text(&output.result, "termination_reason"),
        Some("timeout")
    );
}

#[test]
fn shell_tool_rejects_negative_timeout() {
    // Negative durations cannot be represented by the runner; reject them
    // explicitly instead of silently falling back to the default.
    let args = CborValue::Map(vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text("printf should-not-run".to_owned()),
        ),
        (
            CborValue::Text("timeout".to_owned()),
            CborValue::Integer((-1).into()),
        ),
    ]);

    let error = run_command_live(
        &args,
        &crate::config::ShellConfig::default(),
        crate::tools::shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect_err("timeout");
    assert_eq!(error.message, "argument `timeout` must be non-negative");
}
#[cfg(target_os = "linux")]
#[test]
fn read_only_mount_setattr_flags_are_recursive() {
    // Enforced read-only mode clones the cwd tree recursively, so the final
    // mount_setattr call must also apply recursively; otherwise nested mounts
    // could remain writable under a supposedly read-only cwd subtree.
    let flags = crate::isolation::read_only_mount_setattr_flags();
    assert_ne!(flags & (libc::AT_EMPTY_PATH as libc::c_uint), 0);
    assert_ne!(flags & (libc::AT_RECURSIVE as libc::c_uint), 0);
}

#[cfg(target_os = "linux")]
#[test]
fn shell_tool_enforced_read_only_mode_bind_mounts_cwd_read_only() {
    // Regression coverage for enforced inferred read-only shell mode: lock
    // elision is not enough. When `enforce_ro_bind` is true, the child must get a
    // read-only bind mount over its cwd so accidental writes fail before they
    // can alter the working tree.
    let dir = TempDir::new().expect("temp dir");
    fs::write(dir.path().join("input.txt"), "ok").expect("write fixture");
    let args = CborValue::Map(vec![
        (
            CborValue::Text("cwd".to_owned()),
            CborValue::Text(dir.path().display().to_string()),
        ),
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text("cat input.txt; touch created.txt".to_owned()),
        ),
        (
            CborValue::Text("timeout".to_owned()),
            CborValue::Integer(5.into()),
        ),
    ]);

    let mut world = crate::tools::world::ShellWorld::real();
    let output = match crate::tools::shell::run_command_cancellable(
        "enforced_ro_test",
        &args,
        &crate::config::ShellConfig::default(),
        crate::tools::shell::ShellCommandMode::visible(
            crate::tools::shell::ShellAccessMode::ReadOnly,
        ),
        true,
        None,
        &mut world,
    ) {
        Ok(crate::tools::shell::CommandOutcome::Finished(output)) => *output,
        Ok(crate::tools::shell::CommandOutcome::Cancelled) => panic!("unexpected cancellation"),
        Err(error) if error.message.contains("Operation not permitted") => return,
        Err(error) => panic!("unexpected shell start error: {error:?}"),
    };

    assert_ne!(cbor_int_field(&output.result, "status"), Some(0));
    assert!(
        !dir.path().join("created.txt").exists(),
        "read-only shell command must not create files in cwd"
    );
    let combined = cbor_map_text(&output.result, "output").expect("output");
    assert!(
        combined.contains(" ok"),
        "read should still work: {combined}"
    );
    assert!(
        combined.contains("Read-only file system") || combined.contains("Permission denied"),
        "write should fail due to mount permissions: {combined}"
    );
}

#[test]
fn shell_tool_ignores_legacy_wrong_type_mode_argument() {
    // `mode` is no longer part of the schema. If a stale caller sends it anyway,
    // shell execution is controlled by ext-shell's inferred mode argument.
    let args = CborValue::Map(vec![
        (
            CborValue::Text("mode".to_owned()),
            CborValue::Integer(1.into()),
        ),
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text("printf should-not-run".to_owned()),
        ),
    ]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &crate::config::ShellConfig::default(),
        crate::tools::shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("legacy mode ignored") else {
        panic!("expected finished shell outcome");
    };
    assert_eq!(output.display.mode, "");
}

#[test]
fn shell_tool_rejects_wrong_type_timeout() {
    // The old lenient integer helper ignored wrong-type values, causing the
    // default timeout to be used without telling the agent its request was bad.
    let args = CborValue::Map(vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text("printf should-not-run".to_owned()),
        ),
        (
            CborValue::Text("timeout".to_owned()),
            CborValue::Text("1".to_owned()),
        ),
    ]);

    let error = run_command_live(
        &args,
        &crate::config::ShellConfig::default(),
        crate::tools::shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect_err("timeout");
    assert_eq!(error.message, "argument `timeout` must be an integer");
}

#[test]
fn shell_tool_rejects_wrong_type_cwd() {
    let args = CborValue::Map(vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text("printf should-not-run".to_owned()),
        ),
        (
            CborValue::Text("cwd".to_owned()),
            CborValue::Integer(1.into()),
        ),
    ]);

    let error = run_command_live(
        &args,
        &crate::config::ShellConfig::default(),
        crate::tools::shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect_err("cwd");
    assert_eq!(error.message, "argument `cwd` must be a string");
}

#[cfg(unix)]
#[test]
fn shell_tool_reports_signal_termination_details() {
    // Regression coverage for signal deaths: shells killed by a signal do not
    // have an exit code, but Unix exposes the terminating signal separately.
    let args = CborValue::Map(vec![
        (
            CborValue::Text("command".to_owned()),
            CborValue::Text("kill -TERM $$".to_owned()),
        ),
        (
            CborValue::Text("timeout".to_owned()),
            CborValue::Integer(5.into()),
        ),
    ]);

    let CommandOutcome::Finished(output) = run_command_live(
        &args,
        &crate::config::ShellConfig::default(),
        crate::tools::shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect("signal result") else {
        panic!("expected finished shell outcome");
    };
    let output = *output;
    assert_eq!(output.display.status, ToolUseStatus::Error);
    assert_eq!(output.display.status_text, "signal 15");
    assert_eq!(cbor_int_field(&output.result, "signal"), Some(15));
    assert!(cbor_bool_field(&output.result, "timed_out").is_none());
    assert_eq!(
        cbor_map_text(&output.result, "termination_reason"),
        Some("signal")
    );
    assert!(cbor_map_field(&output.result, "status").is_none());
}

#[test]
fn shell_tool_reports_failures_with_details() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(SHELL_TOOL_NAME),
            arguments: CborValue::Map(vec![(
                CborValue::Text("command".to_owned()),
                CborValue::Text("exit 7".to_owned()),
            )]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("invoke");
    writer.flush().expect("flush");

    let _progress = reader.read_event().expect("read").expect("progress");

    let result = reader.read_event().expect("read").expect("result");
    let Event::ToolResult(result) = result else {
        panic!("expected tool result");
    };
    assert_eq!(result.tool_name, SHELL_TOOL_NAME);
    assert_eq!(cbor_int_field(&result.result, "status"), Some(7));
    assert_eq!(
        cbor_map_text(&result.result, "termination_reason"),
        Some("exit")
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

// -- Truncation ---------------------------------------------------------

#[test]
fn truncate_head_short_input_unchanged() {
    let input = "line 1\nline 2\nline 3";
    let result = truncate_head(input);
    assert!(!result.was_truncated);
    assert_eq!(result.content, input);
}

#[test]
fn truncate_head_limits_by_lines() {
    let lines: Vec<String> = (1..=MAX_OUTPUT_LINES + 500)
        .map(|i| format!("line {i}"))
        .collect();
    let input = lines.join("\n");
    let result = truncate_head(&input);
    assert!(result.was_truncated);
    assert!(result.content.contains("line 1\n"));
    assert!(result.content.contains("\n...\n"));
    assert!(
        result
            .content
            .contains(&format!("line {}", MAX_OUTPUT_LINES + 500))
    );
    assert_eq!(result.content.lines().count(), MAX_OUTPUT_LINES + 1);
}

#[test]
fn truncate_head_limits_by_bytes() {
    // Create input that's within line count but exceeds byte limit.
    let big_line = "x".repeat(MAX_OUTPUT_BYTES + 100);
    let input = format!("first\n{big_line}\nthird");
    let result = truncate_head(&input);
    assert!(result.was_truncated);
    assert!(result.content.starts_with("first"));
    assert!(result.content.contains("(truncated)"));
}

#[test]
fn mark_line_merges_existing_markers_when_truncating() {
    // Truncating an already marked rendered line should preserve the single
    // marker group grammar used by read/shell output.
    assert_eq!(
        mark_line("1(no_nl) hello", "truncated"),
        "1(no_nl,truncated)"
    );
    assert_eq!(mark_line("2(crlf) hello", "truncated"), "2(crlf,truncated)");
    assert_eq!(
        mark_line("out(no_nl) hello", "truncated"),
        "out(no_nl,truncated)"
    );
}

#[test]
fn grep_result_map_omits_request_context() {
    // The agent already knows the grep request arguments it sent. Do not echo
    // pattern/path/glob in the result headers; keep only execution outcome and
    // payload metadata.
    let result = grep_result_map(Some(0), 3, "src/a.rs:1:foo".to_owned());
    assert!(cbor_map_text(&result, "pattern").is_none());
    assert!(cbor_map_text(&result, "path").is_none());
    assert!(cbor_map_text(&result, "glob").is_none());
    assert_eq!(cbor_int_field(&result, "status"), Some(0));
    assert_eq!(cbor_int_field(&result, "matches"), Some(3));
    assert_eq!(cbor_map_text(&result, "output"), Some("src/a.rs:1:foo"));
    assert_eq!(cbor_int_field(&result, "output_lines"), Some(1));
    assert_eq!(cbor_int_field(&result, "output_bytes"), Some(14));

    let no_matches = grep_result_map(Some(1), 0, "no matches found".to_owned());
    assert_eq!(cbor_int_field(&no_matches, "status"), Some(1));
    assert_eq!(cbor_int_field(&no_matches, "matches"), Some(0));
    assert_eq!(cbor_int_field(&no_matches, "output_lines"), Some(1));
    assert_eq!(cbor_int_field(&no_matches, "output_bytes"), Some(16));
}

#[test]
fn classify_ripgrep_stderr_recognizes_stable_prefixes() {
    // Bad regex from the agent. The trailing `error: <diagnostic>`
    // line is the useful one — the header and caret lines aren't.
    let parsed = classify_ripgrep_stderr(
        "regex parse error:\n    (?:Result<(.*Address.*TweakIdx)\n    ^\nerror: unclosed group",
    );
    assert!(
        matches!(parsed, RipgrepError::Usage { .. }),
        "got: {parsed:?}"
    );
    assert_eq!(parsed.to_string(), "regex parse error: unclosed group");
    // Missing path / file.
    assert_eq!(
        classify_ripgrep_stderr("No such file or directory (os error 2)"),
        RipgrepError::NotFound,
    );
    assert_eq!(
        classify_ripgrep_stderr("No such file or directory (os error 2)").to_string(),
        "no such file or directory",
    );
    // Permission denied.
    assert_eq!(
        classify_ripgrep_stderr("Permission denied (os error 13)"),
        RipgrepError::Permission,
    );
    // Anything else (genuine runtime fault) keeps the first stderr
    // line so the chip still carries a useful signal.
    assert_eq!(
        classify_ripgrep_stderr("some unfamiliar ripgrep failure").to_string(),
        "ripgrep error: some unfamiliar ripgrep failure",
    );
}

#[test]
fn reported_lock_wait_duration_seconds_rounds_only_slow_waits() {
    assert_eq!(
        reported_lock_wait_duration_seconds(Duration::from_secs(5)),
        None
    );
    assert_eq!(
        reported_lock_wait_duration_seconds(Duration::from_millis(5001)),
        Some(6)
    );
    assert_eq!(
        reported_lock_wait_duration_seconds(Duration::from_secs(6)),
        Some(6)
    );
}

#[test]
fn lock_wait_duration_header_wraps_non_map_results() {
    let event = with_lock_wait_duration(
        Event::ToolResult(tau_proto::ToolResult {
            call_id: "call-lock-wait".into(),
            tool_name: tau_proto::ToolName::new(EDIT_TOOL_NAME),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("changed".to_owned()),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        }),
        Some(6),
    );

    let Event::ToolResult(result) = event else {
        panic!("expected tool result");
    };
    assert_eq!(
        cbor_int_field(&result.result, LOCK_WAIT_DURATION_SECONDS_HEADER),
        Some(6)
    );
    assert_eq!(cbor_map_text(&result.result, "output"), Some("changed"));
}

#[test]
fn lock_wait_duration_header_extends_tool_error_details() {
    let event = with_lock_wait_duration(
        Event::ToolError(tau_proto::ToolError {
            call_id: "call-lock-wait".into(),
            tool_name: tau_proto::ToolName::new(SHELL_TOOL_NAME),
            tool_type: tau_proto::ToolType::Function,
            message: "failed".to_owned(),
            details: Some(cbor_text_map(vec![("output", "start failed")])),
            display: None,
            originator: tau_proto::PromptOriginator::User,
        }),
        Some(7),
    );

    let Event::ToolError(error) = event else {
        panic!("expected tool error");
    };
    let details = error.details.expect("error details");
    assert_eq!(
        cbor_int_field(&details, LOCK_WAIT_DURATION_SECONDS_HEADER),
        Some(7)
    );
    assert_eq!(cbor_map_text(&details, "output"), Some("start failed"));
}

#[test]
fn command_details_value_records_combined_output_stats() {
    let details = command_details_value(CommandDetails {
        status: Some(0),
        signal: None,
        timed_out: false,
        duration_seconds: None,
        termination_reason: "exit",
        total_lines: None,
        total_bytes: None,
        output: "out hi\nerr oops".to_owned(),
        truncated: false,
        valid_utf8: true,
    });
    assert_eq!(cbor_map_text(&details, "output"), Some("out hi\nerr oops"));
    assert!(cbor_map_field(&details, "total_lines").is_none());
    assert!(cbor_map_field(&details, "total_bytes").is_none());
    assert!(cbor_map_field(&details, "valid_utf8").is_none());
    assert!(cbor_map_field(&details, "timed_out").is_none());
    assert!(cbor_map_field(&details, "termination_reason").is_none());
    assert!(cbor_map_field(&details, "truncated").is_none());
    assert!(cbor_map_field(&details, "duration_seconds").is_none());
}

#[test]
fn command_details_value_records_slow_command_exec_time() {
    let details = command_details_value(CommandDetails {
        status: Some(0),
        signal: None,
        timed_out: false,
        duration_seconds: Some(6),
        termination_reason: "exit",
        total_lines: None,
        total_bytes: None,
        output: String::new(),
        truncated: false,
        valid_utf8: true,
    });

    assert_eq!(cbor_int_field(&details, "duration_seconds"), Some(6));
}

#[test]
fn optional_argument_bool_rejects_present_non_bool_values() {
    let args = CborValue::Map(vec![(
        CborValue::Text("ignoreCase".to_owned()),
        CborValue::Text("True".to_owned()),
    )]);

    let err = optional_argument_bool(&args, "ignoreCase").expect_err("non-bool should fail");

    assert_eq!(err, "argument `ignoreCase` must be a boolean");
}

#[test]
fn optional_argument_text_rejects_present_non_string_values() {
    let args = CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Integer(123.into()),
    )]);

    let err = optional_argument_text(&args, "path").expect_err("non-string should fail");

    assert_eq!(err, "argument `path` must be a string");
}

/// Protects the scheduler admission path from allocating/debug-formatting huge
/// CBOR arguments before applying the queued-byte budget.
#[test]
fn approximate_tool_bytes_caps_large_cbor_without_debug_rendering() {
    let Event::ToolStarted(invoke) = tool_started(
        "large-args",
        SHELL_TOOL_NAME,
        CborValue::Bytes(vec![0; crate::scheduler::DEFAULT_QUEUED_BYTES_LIMIT + 1024]),
        "agent-a",
    ) else {
        panic!("expected tool started");
    };

    assert_eq!(
        approximate_tool_bytes(&invoke, crate::scheduler::DEFAULT_QUEUED_BYTES_LIMIT),
        crate::scheduler::DEFAULT_QUEUED_BYTES_LIMIT + 1
    );
    let raised_estimate =
        approximate_tool_bytes(&invoke, crate::scheduler::DEFAULT_QUEUED_BYTES_LIMIT * 2);
    assert!(raised_estimate > crate::scheduler::DEFAULT_QUEUED_BYTES_LIMIT + 1024);
    assert!(raised_estimate < crate::scheduler::DEFAULT_QUEUED_BYTES_LIMIT * 2);
}
fn grep_args(pattern: &str, path: &str, extra: Vec<(CborValue, CborValue)>) -> CborValue {
    let mut entries = vec![
        (
            CborValue::Text("pattern".to_owned()),
            CborValue::Text(pattern.to_owned()),
        ),
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(path.to_owned()),
        ),
    ];
    entries.extend(extra);
    CborValue::Map(entries)
}

#[test]
fn run_grep_rejects_string_bool_argument() {
    let tempdir = TempDir::new().expect("tempdir");
    fs::write(tempdir.path().join("a.txt"), "alpha\n").expect("write a");

    let args = grep_args(
        "alpha",
        &tempdir.path().display().to_string(),
        vec![(
            CborValue::Text("ignoreCase".to_owned()),
            CborValue::Text("True".to_owned()),
        )],
    );
    let err = run_grep(&args).expect_err("string bool should fail");

    assert_eq!(err.message, "argument `ignoreCase` must be a boolean");
}

#[test]
fn run_grep_counts_matches_across_directory() {
    let tempdir = TempDir::new().expect("tempdir");
    fs::write(tempdir.path().join("a.txt"), "alpha\nbeta\nalpha\n").expect("write a");
    fs::write(tempdir.path().join("b.txt"), "alpha\n").expect("write b");

    let args = grep_args("alpha", &tempdir.path().display().to_string(), vec![]);
    let result = run_grep(&args).expect("grep").result;

    assert_eq!(cbor_int_field(&result, "matches"), Some(3));
}

#[test]
fn run_grep_no_matches_uses_plain_ok_status() {
    let tempdir = TempDir::new().expect("tempdir");
    fs::write(tempdir.path().join("a.txt"), "alpha\n").expect("write a");

    let args = grep_args("beta", &tempdir.path().display().to_string(), vec![]);
    let output = run_grep(&args).expect("grep");

    assert_eq!(output.display.status_text, "ok");
    assert_eq!(output.display.stats.matches, Some(0));
}

#[test]
fn run_grep_counts_matches_in_single_file() {
    // Regression: when `path` is a single file, rg drops the
    // `PATH:` prefix from each line. Without `--with-filename` the
    // match-line classifier misses every match and `matches` falls
    // back to 0 even though output clearly contains hits.
    let tempdir = TempDir::new().expect("tempdir");
    let file = tempdir.path().join("single.txt");
    fs::write(&file, "alpha\nbeta\nalpha\ngamma\nalpha\n").expect("write");

    let args = grep_args("alpha", &file.display().to_string(), vec![]);
    let result = run_grep(&args).expect("grep").result;

    assert_eq!(cbor_int_field(&result, "matches"), Some(3));
    let output = cbor_map_text(&result, "output").expect("output");
    assert!(
        output.contains(&format!("{}:1:alpha", file.display())),
        "expected PATH:LINE:CONTENT shape, got: {output}"
    );
}

#[test]
fn run_grep_with_context_counts_only_match_lines() {
    // Context lines (`PATH-LINE-CONTENT`) must not be counted as
    // matches. Search a single file so we also exercise the
    // `--with-filename` path.
    let tempdir = TempDir::new().expect("tempdir");
    let file = tempdir.path().join("single.txt");
    fs::write(
        &file,
        "filler 1\nfiller 2\nalpha\nfiller 3\nfiller 4\nalpha\nfiller 5\n",
    )
    .expect("write");

    let args = grep_args(
        "alpha",
        &file.display().to_string(),
        vec![(
            CborValue::Text("context".to_owned()),
            CborValue::Integer(1.into()),
        )],
    );
    let result = run_grep(&args).expect("grep").result;

    // Two matches; surrounding context lines are present in output
    // but must not inflate the count.
    assert!(cbor_map_field(&result, "path").is_none());
    assert!(cbor_map_field(&result, "pattern").is_none());
    assert_eq!(cbor_int_field(&result, "matches"), Some(2));
    let output = cbor_map_text(&result, "output").expect("output");
    assert!(output.contains(":3:alpha"), "first match missing: {output}");
    assert!(
        output.contains(":6:alpha"),
        "second match missing: {output}"
    );
    assert!(
        output.contains("-2-filler 2"),
        "context line missing: {output}"
    );
}

#[test]
fn truncate_tail_short_input_unchanged() {
    let input = "line 1\nline 2\nline 3";
    let result = truncate_tail(input);
    assert!(!result.was_truncated);
    assert_eq!(result.content, input);
}

#[test]
fn truncate_tail_keeps_last_lines() {
    let lines: Vec<String> = (1..=MAX_OUTPUT_LINES + 500)
        .map(|i| format!("line {i}"))
        .collect();
    let input = lines.join("\n");
    let result = truncate_tail(&input);
    assert!(result.was_truncated);
    assert!(
        result
            .content
            .contains(&format!("line {}", MAX_OUTPUT_LINES + 500))
    );
    assert!(result.content.contains("\n...\n"));
    assert!(result.content.contains("line 1\n"));
}

#[test]
fn truncate_tail_limits_by_bytes() {
    let big_line = "x".repeat(MAX_OUTPUT_BYTES + 100);
    let input = format!("first\nsecond\n{big_line}\nlast");
    let result = truncate_tail(&input);
    assert!(result.was_truncated);
    assert!(result.content.contains("last"));
    assert!(result.content.contains("(truncated)"));
}

#[test]
fn truncate_tail_keeps_suffix_for_one_huge_line() {
    // Regression coverage for an oversized single-line stream: tail truncation
    // used to keep zero lines and report an impossible `lines 2-1 of 1` range.
    let input = "x".repeat(MAX_OUTPUT_BYTES + 100);
    let result = truncate_tail(&input);

    assert!(result.was_truncated);
    assert_eq!(result.content, "(truncated)");
}

#[test]
fn truncate_tail_keeps_suffix_for_huge_final_line() {
    // When the final line alone exceeds the byte cap, the useful tail is a
    // suffix of that line rather than an empty line range.
    let final_line = format!("{}TAIL", "x".repeat(MAX_OUTPUT_BYTES + 100));
    let input = format!("first\n{final_line}");
    let result = truncate_tail(&input);

    assert!(result.was_truncated);
    assert!(result.content.starts_with("first\n"));
    assert!(result.content.contains("(truncated)"));
    assert!(!result.content.contains("TAIL"));
}

#[test]
fn truncate_tail_preserves_utf8_boundary_for_huge_line_suffix() {
    // Byte fallback must never slice through a multibyte codepoint; otherwise
    // shell output truncation can panic or manufacture invalid UTF-8.
    let input = "€".repeat(MAX_OUTPUT_BYTES / "€".len() + 100);
    let result = truncate_tail(&input);
    assert!(result.was_truncated);
    assert_eq!(result.content, "(truncated)");
}

#[test]
fn slice_lines_returns_requested_window() {
    let sliced = slice_lines("a\nb\nc\nd", 2, Some(3));
    assert_eq!(sliced.content, "2 b\n3 c");
    assert_eq!(sliced.line_count, 2);
}

#[test]
fn slice_lines_clamps_past_end() {
    let sliced = slice_lines("a\nb\nc", 10, Some(14));
    assert_eq!(sliced.content, "");
    assert_eq!(sliced.line_count, 0);
}

#[test]
fn read_file_honors_start_line_and_end_line() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("small.txt");
    std::fs::write(&path, "line 1\nline 2\nline 3\nline 4\nline 5\n").expect("write");

    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(path.display().to_string()),
        ),
        (
            CborValue::Text("start_line".to_owned()),
            CborValue::Integer(2.into()),
        ),
        (
            CborValue::Text("end_line".to_owned()),
            CborValue::Integer(4.into()),
        ),
    ]);
    let output = read_file(&args).expect("read");
    let result = output.result;
    assert_eq!(output.display.args, format!("{} 2..4", path.display()));
    assert_eq!(
        cbor_map_text(&result, "line-numbered content"),
        Some("2 line 2\n3 line 3\n4 line 4")
    );
    assert!(cbor_map_field(&result, "path").is_none());
    assert!(cbor_map_field(&result, "start_line").is_none());
    assert!(cbor_map_field(&result, "end_line").is_none());
    assert!(cbor_map_field(&result, "total_lines").is_none());
    assert!(cbor_map_field(&result, "ends_with_newline").is_none());
    assert!(cbor_map_field(&result, "line_ending").is_none());
}

#[test]
fn read_file_clips_end_line_past_eof() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("small.txt");
    std::fs::write(&path, "one\ntwo\nthree\n").expect("write");

    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(path.display().to_string()),
        ),
        (
            CborValue::Text("start_line".to_owned()),
            CborValue::Integer(2.into()),
        ),
        (
            CborValue::Text("end_line".to_owned()),
            CborValue::Integer(99.into()),
        ),
    ]);

    let output = read_file(&args).expect("read");
    assert_eq!(output.display.args, format!("{} 2..99", path.display()));
    assert_eq!(
        cbor_map_text(&output.result, "line-numbered content"),
        Some("2 two\n3 three")
    );
}

#[test]
fn read_file_reads_multiple_disjoint_ranges_with_blank_separator() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("small.txt");
    std::fs::write(&path, "one\ntwo\nthree\nfour\nfive\n").expect("write");

    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(path.display().to_string()),
        ),
        (
            CborValue::Text("ranges".to_owned()),
            CborValue::Array(vec![read_range(2, 3), read_range(5, 5)]),
        ),
    ]);
    let output = read_file(&args).expect("read");
    let result = output.result;

    assert_eq!(output.display.args, format!("{} 2..3,5..5", path.display()));
    assert_eq!(
        cbor_map_text(&result, "line-numbered content"),
        Some("2 two\n3 three\n\n5 five")
    );
    assert!(cbor_map_field(&result, "total_lines").is_none());
}

#[test]
fn read_file_allows_overlapping_ranges_with_redundant_chunks() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("small.txt");
    std::fs::write(&path, "one\ntwo\nthree\n").expect("write");

    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(path.display().to_string()),
        ),
        (
            CborValue::Text("ranges".to_owned()),
            CborValue::Array(vec![read_range(1, 2), read_range(2, 3)]),
        ),
    ]);

    let output = read_file(&args).expect("overlap should be returned redundantly");
    assert_eq!(output.display.args, format!("{} 1..2,2..3", path.display()));
    assert_eq!(
        cbor_map_text(&output.result, "line-numbered content"),
        Some("1 one\n2 two\n\n2 two\n3 three")
    );
}

#[test]
fn read_file_rejects_range_request_over_cap_before_reading_file() {
    let ranges = (0..=100).map(|_| read_range(1, 1)).collect::<Vec<_>>();
    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text("/definitely/missing/read-target.txt".to_owned()),
        ),
        (
            CborValue::Text("ranges".to_owned()),
            CborValue::Array(ranges),
        ),
    ]);

    let error = read_file(&args).expect_err("read should reject arguments first");
    assert_eq!(error.message, "requested range count exceeds limit of 100");
}

#[test]
fn read_file_rejects_ranges_combined_with_top_level_range() {
    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text("x".to_owned()),
        ),
        (
            CborValue::Text("start_line".to_owned()),
            CborValue::Integer(1.into()),
        ),
        (
            CborValue::Text("ranges".to_owned()),
            CborValue::Array(vec![read_range(1, 1)]),
        ),
    ]);

    let error = read_file(&args).expect_err("mixed range styles should fail");
    assert_eq!(
        error.message,
        "ranges cannot be combined with start_line or end_line"
    );
}

#[test]
fn format_read_range_reports_requested_ranges() {
    assert_eq!(format_read_range(None, None), "..");
    assert_eq!(format_read_range(Some(11), None), "11..");
    assert_eq!(format_read_range(None, Some(100)), "1..100");
    assert_eq!(format_read_range(Some(11), Some(11)), "11..11");
    assert_eq!(format_read_range(Some(11), Some(100)), "11..100");
}

#[test]
fn read_file_errors_when_start_line_is_past_eof() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("small.txt");
    std::fs::write(&path, "one\ntwo\n").expect("write");

    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(path.display().to_string()),
        ),
        (
            CborValue::Text("start_line".to_owned()),
            CborValue::Integer(3.into()),
        ),
    ]);

    let error = read_file(&args).expect_err("start_line past EOF should fail");
    assert_eq!(
        error.message,
        "start_line 3 is past end of file (total_lines: 2)"
    );
}

#[test]
fn read_file_reports_empty_file_as_zero_lines() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("empty.txt");
    std::fs::write(&path, "").expect("write");

    let args = CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Text(path.display().to_string()),
    )]);
    let result = read_file(&args).expect("read").result;

    assert_eq!(cbor_map_text(&result, "line-numbered content"), Some(""));
    assert!(cbor_map_field(&result, "start_line").is_none());
    assert!(cbor_map_field(&result, "end_line").is_none());
    assert_eq!(cbor_int_field(&result, "total_lines"), Some(0));
    assert_eq!(cbor_int_field(&result, "total_bytes"), Some(0));
    assert!(cbor_map_field(&result, "ends_with_newline").is_none());
    assert!(cbor_map_field(&result, "line_ending").is_none());
}

#[test]
fn read_file_rejects_start_line_after_empty_file() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("empty.txt");
    std::fs::write(&path, "").expect("write");

    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(path.display().to_string()),
        ),
        (
            CborValue::Text("start_line".to_owned()),
            CborValue::Integer(2.into()),
        ),
    ]);

    let error = read_file(&args).expect_err("start_line after empty file should fail");
    assert_eq!(
        error.message,
        "start_line 2 is past end of file (total_lines: 0)"
    );
}

#[test]
fn read_file_reports_no_trailing_newline_as_one_line() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("no-newline.txt");
    std::fs::write(&path, "text").expect("write");

    let args = CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Text(path.display().to_string()),
    )]);
    let result = read_file(&args).expect("read").result;

    assert_eq!(
        cbor_map_text(&result, "line-numbered content"),
        Some("1(no_nl) text")
    );
    assert!(cbor_map_field(&result, "start_line").is_none());
    assert!(cbor_map_field(&result, "end_line").is_none());
    assert!(cbor_map_field(&result, "total_lines").is_none());
    assert!(cbor_map_field(&result, "ends_with_newline").is_none());
    assert!(cbor_map_field(&result, "line_ending").is_none());
}

#[test]
fn read_file_rejects_invalid_line_arguments() {
    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text("x".to_owned()),
        ),
        (
            CborValue::Text("start_line".to_owned()),
            CborValue::Integer(0.into()),
        ),
    ]);
    assert_eq!(
        read_file(&args)
            .expect_err("start_line=0 should fail")
            .message,
        "start_line must be >= 1"
    );

    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text("x".to_owned()),
        ),
        (
            CborValue::Text("end_line".to_owned()),
            CborValue::Integer(0.into()),
        ),
    ]);
    assert_eq!(
        read_file(&args)
            .expect_err("end_line=0 should fail")
            .message,
        "end_line must be >= 1"
    );

    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text("x".to_owned()),
        ),
        (
            CborValue::Text("start_line".to_owned()),
            CborValue::Integer(3.into()),
        ),
        (
            CborValue::Text("end_line".to_owned()),
            CborValue::Integer(2.into()),
        ),
    ]);
    assert_eq!(
        read_file(&args)
            .expect_err("end_line before start_line should fail")
            .message,
        "end_line must be >= start_line"
    );

    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text("x".to_owned()),
        ),
        (
            CborValue::Text("line_count".to_owned()),
            CborValue::Integer(1.into()),
        ),
    ]);
    assert_eq!(
        read_file(&args)
            .expect_err("line_count should be rejected")
            .message,
        "line_count is no longer supported; use end_line"
    );
}

#[test]
fn read_file_truncates_large_output() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("big.txt");
    let lines: Vec<String> = (1..=3000).map(|i| format!("line {i}")).collect();
    std::fs::write(&path, lines.join("\n")).expect("write");

    let args = CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Text(path.display().to_string()),
    )]);
    let result = read_file(&args).expect("read").result;
    let content = cbor_map_text(&result, "line-numbered content").expect("content field");
    assert!(content.contains("line 1\n"));
    assert!(content.contains("\n...\n"));
    assert!(content.contains("line 3000"));
    assert!(cbor_map_field(&result, "start_line").is_none());
    assert!(cbor_map_field(&result, "end_line").is_none());
    assert_eq!(cbor_int_field(&result, "total_lines"), Some(3000));
}

#[test]
fn read_file_truncation_notice_uses_source_line_numbers() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("big-slice.txt");
    let lines: Vec<String> = (1..=2105).map(|i| format!("line {i}")).collect();
    std::fs::write(&path, lines.join("\n")).expect("write");

    let args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(path.display().to_string()),
        ),
        (
            CborValue::Text("start_line".to_owned()),
            CborValue::Integer(100.into()),
        ),
    ]);
    let result = read_file(&args).expect("read").result;
    let content = cbor_map_text(&result, "line-numbered content").expect("content field");

    assert!(content.contains("100 line 100"));
    assert!(content.contains("\n...\n"));
    assert!(content.contains("line 2105"));
    assert!(cbor_map_field(&result, "start_line").is_none());
    assert!(cbor_map_field(&result, "end_line").is_none());
    assert_eq!(cbor_int_field(&result, "total_lines"), Some(2105));
}

#[test]
fn read_file_reports_crlf_line_endings() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("crlf.txt");
    std::fs::write(&path, "one\r\ntwo\r\n").expect("write");

    let args = CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Text(path.display().to_string()),
    )]);
    let result = read_file(&args).expect("read").result;

    assert_eq!(
        cbor_map_text(&result, "line-numbered content"),
        Some("1(crlf) one\n2(crlf) two")
    );
    assert!(cbor_map_field(&result, "ends_with_newline").is_none());
    assert!(cbor_map_field(&result, "line_ending").is_none());
}

#[test]
fn read_file_reports_cr_only_line_endings() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("cr.txt");
    std::fs::write(&path, b"one\rtwo\r").expect("write");

    let args = CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Text(path.display().to_string()),
    )]);
    let result = read_file(&args).expect("read").result;

    assert_eq!(
        cbor_map_text(&result, "line-numbered content"),
        Some("1(cr) one\n2(cr) two")
    );
    assert!(cbor_map_field(&result, "total_lines").is_none());
    assert!(cbor_map_field(&result, "ends_with_newline").is_none());
    assert!(cbor_map_field(&result, "line_ending").is_none());
}

#[test]
fn read_file_does_not_mark_lf_when_line_endings_are_evenly_mixed() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("mixed-even.txt");
    std::fs::write(&path, b"one\ntwo\r\nthree").expect("write");

    let args = CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Text(path.display().to_string()),
    )]);
    let result = read_file(&args).expect("read").result;

    assert_eq!(
        cbor_map_text(&result, "line-numbered content"),
        Some("1 one\n2(crlf) two\n3(no_nl) three")
    );
}

#[test]
fn read_file_marks_line_ending_outliers() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("mixed.txt");
    std::fs::write(&path, b"one\ntwo\nthree\r\nfour\rfive").expect("write");

    let args = CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Text(path.display().to_string()),
    )]);
    let result = read_file(&args).expect("read").result;

    assert_eq!(
        cbor_map_text(&result, "line-numbered content"),
        Some("1 one\n2 two\n3(crlf) three\n4(cr) four\n5(no_nl) five")
    );
    assert!(cbor_map_field(&result, "ends_with_newline").is_none());
}

#[test]
fn read_file_handles_invalid_utf8_per_line() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("invalid.bin");
    std::fs::write(&path, b"abc\xffdef\nsecond\n").expect("write");

    let args = CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Text(path.display().to_string()),
    )]);
    let result = read_file(&args).expect("read").result;

    assert_eq!(
        cbor_map_text(&result, "line-numbered content"),
        Some("1(invalid-utf8) abc�def\n2 second")
    );
    assert!(cbor_map_field(&result, "end_line").is_none());
    assert_eq!(cbor_bool_field(&result, "valid_utf8"), Some(false));
    assert!(cbor_map_field(&result, "total_bytes").is_none());
}

#[test]
fn read_file_truncates_single_long_line() {
    let td = TempDir::new().expect("tempdir");
    let path = td.path().join("longline.txt");
    std::fs::write(&path, format!("{}\nsecond\n", "x".repeat(60 * 1024))).expect("write");

    let args = CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Text(path.display().to_string()),
    )]);
    let result = read_file(&args).expect("read").result;
    let content = cbor_map_text(&result, "line-numbered content").expect("content");

    assert_eq!(content, "1(truncated)\n2 second");
    assert!(cbor_map_field(&result, "end_line").is_none());
    assert!(cbor_int_field(&result, "total_bytes").is_some());
}

#[test]
fn edit_context_line_rejects_invalid_utf8_bytes_without_writing() {
    let tempdir = TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("edit.bin");
    fs::write(&file_path, b"abc\xffdef\nsecond\n").expect("write fixture");

    let error = edit_file(&edit_arguments(
        &file_path,
        vec![context_line_edit(1, 1, "FIRST\n", "abc�def")],
    ))
    .expect_err("invalid UTF-8 context_line should fail");

    assert_eq!(
        error.message,
        "context_line wrong - current line 1 is not valid UTF-8, so no context_line string can match it; see current content in the response"
    );
    assert_eq!(
        fs::read(&file_path).expect("read back"),
        b"abc\xffdef\nsecond\n"
    );
}
#[test]
fn run_find_double_star_matches_top_level_files() {
    // Regression: `**/*.rs` should match both nested AND
    // top-level Rust files. `globset`'s native `**` requires one
    // path separator; we work around that in `compile_find_glob`.
    let tempdir = TempDir::new().expect("tempdir");
    fs::create_dir_all(tempdir.path().join("src")).expect("mkdir");
    fs::write(tempdir.path().join("top.rs"), "fn top() {}\n").expect("write top");
    fs::write(tempdir.path().join("src/lib.rs"), "fn nested() {}\n").expect("write nested");
    fs::write(tempdir.path().join("README.md"), "# hi\n").expect("write readme");

    let args = CborValue::Map(vec![
        (
            CborValue::Text("pattern".to_owned()),
            CborValue::Text("**/*.rs".to_owned()),
        ),
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(tempdir.path().display().to_string()),
        ),
    ]);
    let result = run_find(&args).expect("find").result;

    assert_eq!(cbor_int_field(&result, "matches"), Some(2));
    let output = cbor_map_text(&result, "output").expect("output");
    assert!(
        output.contains("top.rs"),
        "top-level match missing: {output}"
    );
    assert!(
        output.contains("src/lib.rs"),
        "nested match missing: {output}"
    );
    assert!(!output.contains("README.md"));
}

#[test]
fn run_find_returns_matching_files() {
    let tempdir = TempDir::new().expect("tempdir");
    fs::create_dir_all(tempdir.path().join("src/nested")).expect("mkdir");
    fs::write(tempdir.path().join("src/lib.rs"), "pub fn one() {}\n").expect("write");
    fs::write(
        tempdir.path().join("src/nested/mod.rs"),
        "pub fn two() {}\n",
    )
    .expect("write");
    fs::write(tempdir.path().join("README.md"), "# hi\n").expect("write");

    let args = CborValue::Map(vec![
        (
            CborValue::Text("pattern".to_owned()),
            CborValue::Text("**/*.rs".to_owned()),
        ),
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(tempdir.path().display().to_string()),
        ),
    ]);
    let result = run_find(&args).expect("find").result;

    assert!(cbor_map_field(&result, "path").is_none());
    assert!(cbor_map_field(&result, "pattern").is_none());
    assert_eq!(cbor_int_field(&result, "matches"), Some(2));
    let output = cbor_map_text(&result, "output").expect("output");
    assert!(output.contains("src/lib.rs"));
    assert!(output.contains("src/nested/mod.rs"));
    assert!(!output.contains("README.md"));
}

#[test]
fn run_find_no_matches_uses_plain_ok_status() {
    // Regression: the UI already renders the zero-match count, so the
    // success chip should stay the generic `ok` instead of repeating
    // `no matches` in the status text.
    let tempdir = TempDir::new().expect("tempdir");
    fs::write(tempdir.path().join("README.md"), "# hi\n").expect("write");

    let args = CborValue::Map(vec![
        (
            CborValue::Text("pattern".to_owned()),
            CborValue::Text("**/*.rs".to_owned()),
        ),
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(tempdir.path().display().to_string()),
        ),
    ]);
    let output = run_find(&args).expect("find");

    assert_eq!(output.display.status_text, "ok");
    assert_eq!(output.display.stats.matches, Some(0));
    assert!(cbor_map_field(&output.result, "path").is_none());
    assert!(cbor_map_field(&output.result, "pattern").is_none());
    assert_eq!(cbor_int_field(&output.result, "matches"), Some(0));
}

#[test]
fn run_ls_lists_directory_contents() {
    let tempdir = TempDir::new().expect("tempdir");
    fs::create_dir_all(tempdir.path().join("src")).expect("mkdir");
    fs::write(tempdir.path().join("README.md"), "# hi\n").expect("write");
    fs::write(tempdir.path().join(".env"), "SECRET=1\n").expect("write");

    let args = CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Text(tempdir.path().display().to_string()),
    )]);
    let mut world = crate::tools::world::ShellWorld::real();
    let result = run_ls(&args, &mut world).expect("ls").result;

    assert!(cbor_map_field(&result, "path").is_none());
    assert_eq!(cbor_int_field(&result, "entries"), Some(3));
    let output = cbor_map_text(&result, "output").expect("output");
    assert!(output.contains(".env"));
    assert!(output.contains("README.md"));
    assert!(output.contains("src/"));
}

/// Configured instance identity, not a process id or prefix, namespaces
/// metadata.
#[test]
fn configure_instance_name_changes_workdir_metadata_key() {
    let cwd_state = CwdState::new();
    assert_eq!(cwd_state.key().as_str(), "ext_core-shell_cwd");
    cwd_state.set_instance_name("project-shell".to_owned());
    assert_eq!(cwd_state.key().as_str(), "ext_project-shell_cwd");
}

/// Two configured shell instances keep independent metadata namespaces and
/// publish distinct prefix-associated context contributions for the same agent.
#[test]
fn two_shell_instance_workdirs_are_independent_and_prefix_associated() {
    let agent_id = tau_proto::AgentId::parse("agent-two-shells").expect("agent id");
    let first_dir = TempDir::new().expect("first");
    let second_dir = TempDir::new().expect("second");
    let first = CwdState::new();
    first.set_instance_name("core-shell".to_owned());
    first.set_context_label(None);
    first.set(
        agent_id.clone(),
        first_dir.path().canonicalize().expect("first canonical"),
    );
    let second = CwdState::new();
    second.set_instance_name("prod-shell".to_owned());
    let prod = tau_proto::ToolNamePrefix::parse("prod").expect("prefix");
    second.set_context_label(Some(&prod));
    second.set(
        agent_id.clone(),
        second_dir.path().canonicalize().expect("second canonical"),
    );
    assert_ne!(first.key(), second.key());
    let Event::ExtAgentContextPublish(first_context) = cwd_context_event(
        agent_id.clone(),
        &first.get(&agent_id).expect("first path"),
        &first,
    ) else {
        unreachable!()
    };
    let Event::ExtAgentContextPublish(second_context) = cwd_context_event(
        agent_id,
        &second
            .get(&tau_proto::AgentId::parse("agent-two-shells").expect("agent id"))
            .expect("second path"),
        &second,
    ) else {
        unreachable!()
    };
    assert_eq!(first_context.value.0["label"], "default");
    assert_eq!(second_context.value.0["label"], "prod");
    assert_ne!(
        first_context.value.0["path"],
        second_context.value.0["path"]
    );
}

/// Regression guard: a call-local shell cwd must never mutate durable workdir
/// metadata.
#[test]
fn explicit_shell_cwd_is_canonicalized_without_emitting_metadata() {
    let temp = TempDir::new().expect("tempdir");
    let original = TempDir::new().expect("original cwd");
    let agent_id = tau_proto::AgentId::parse("agent-cwd-explicit").expect("agent id");
    let cwd_state = CwdState::new();
    cwd_state.set(
        agent_id.clone(),
        original.path().canonicalize().expect("original"),
    );
    let Event::ToolStarted(invoke) = tool_started(
        "call-cwd",
        SHELL_TOOL_NAME,
        cbor_text_map(vec![
            ("command", "pwd"),
            ("cwd", &temp.path().display().to_string()),
        ]),
        agent_id.as_str(),
    ) else {
        unreachable!();
    };

    let base = cwd_state.get_or_default(&agent_id).expect("remembered cwd");
    let rewritten = rewrite_invoke_for_cwd(invoke, &base);
    let canonical = temp.path().canonicalize().expect("canonical cwd");
    assert_eq!(
        cwd_state.get_or_default(&agent_id).expect("remembered cwd"),
        original.path().canonicalize().expect("original")
    );
    assert_eq!(
        optional_argument_text(&rewritten.arguments, "cwd").expect("cwd arg"),
        Some(canonical.display().to_string())
    );
}

/// Regression guard: generic `shell.cwd` and GPT `shell_command.workdir` remain
/// call-local across full dispatch, including relative GPT resolution.
#[test]
fn shell_surface_directory_overrides_remain_call_local_across_full_dispatch() {
    let remembered = TempDir::new().expect("remembered");
    let override_dir = TempDir::new().expect("override");
    let relative_override = remembered.path().join("relative-override");
    fs::create_dir(&relative_override).expect("relative override");
    fs::write(remembered.path().join("remembered.txt"), "from-a\n").expect("remembered file");
    let agent_id = tau_proto::AgentId::parse("agent-call-local-cwd").expect("agent id");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    writer
        .write_event(&Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
            agent_id: agent_id.clone(),
            key: tau_proto::AgentMetadataKey::new("ext_test-extension_cwd"),
            value: CborValue::Text(remembered.path().display().to_string()),
            mutation_id: None,
            inheritable: true,
        }))
        .expect("seed workdir");
    writer.flush().expect("flush seed");
    let _ = reader.read_event().expect("context").expect("context");

    for tool_name in [SHELL_TOOL_NAME, GPT_SHELL_TOOL_NAME] {
        let surface =
            crate::tools::ShellSurface::for_tool_name(tool_name).expect("known shell surface");
        let mut cases = vec![
            (
                "override",
                Some(override_dir.path().display().to_string()),
                override_dir.path(),
            ),
            ("remembered-after", None, remembered.path()),
        ];
        if surface == crate::tools::ShellSurface::ChatGpt {
            cases.push((
                "relative-override",
                Some("relative-override".to_owned()),
                relative_override.as_path(),
            ));
        }
        for (suffix, directory_argument, expected) in cases {
            let call_id = format!("call-{tool_name}-{suffix}");
            let mut args = vec![("command", "pwd")];
            if let Some(directory_argument) = directory_argument.as_deref() {
                args.push((surface.directory_argument(), directory_argument));
            }
            writer
                .write_event(&tool_started(
                    &call_id,
                    tool_name,
                    cbor_text_map(args),
                    agent_id.as_str(),
                ))
                .expect("shell call");
            writer.flush().expect("flush shell call");
            loop {
                match reader.read_event().expect("read").expect("event") {
                    Event::AgentMetadataSet(metadata) => {
                        panic!("call-local shell directory emitted metadata: {metadata:?}")
                    }
                    Event::AgentUserMessageInjected(notice) => {
                        panic!("call-local shell directory emitted notice: {notice:?}")
                    }
                    Event::ToolResult(result) if result.call_id.as_str() == call_id => {
                        let rendered = format!("{:?}", result.result);
                        assert!(
                            rendered.contains(expected.to_str().expect("UTF-8 path")),
                            "shell result did not use expected call-local directory: {rendered}"
                        );
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
    writer
        .write_event(&tool_started(
            "call-read-remembered-after",
            READ_TOOL_NAME,
            cbor_text_map(vec![("path", "remembered.txt")]),
            agent_id.as_str(),
        ))
        .expect("read remembered file");
    writer.flush().expect("flush read");
    loop {
        match reader.read_event().expect("read").expect("event") {
            Event::AgentMetadataSet(metadata) => {
                panic!("call-local shell directory emitted metadata: {metadata:?}")
            }
            Event::ToolResult(result)
                if result.call_id.as_str() == "call-read-remembered-after" =>
            {
                assert!(format!("{:?}", result.result).contains("from-a"));
                break;
            }
            _ => {}
        }
    }
}

/// Regression guard: the removed GPT `cwd` spelling is not accepted as an
/// unadvertised runtime compatibility alias.
#[test]
fn gpt_shell_does_not_accept_legacy_cwd_as_call_local_workdir() {
    let legacy_override = TempDir::new().expect("legacy override");
    let args = cbor_text_map(vec![
        ("command", "pwd"),
        ("cwd", &legacy_override.path().display().to_string()),
    ]);

    let error = crate::tools::shell::run_command_live_for_surface(
        crate::tools::ShellSurface::ChatGpt,
        &args,
        &crate::config::ShellConfig::default(),
        crate::tools::shell::ShellCommandMode::READ_WRITE_HIDDEN,
        false,
        None,
    )
    .expect_err("legacy cwd must be rejected");
    assert_eq!(
        error.message,
        "argument `cwd` is not supported by `shell_command`; use call-local `workdir`"
    );
}

/// Regression guard: a present non-string GPT `workdir` survives admission
/// rewriting for parser rejection and cannot mutate persistent state.
#[test]
fn malformed_gpt_workdir_fails_without_side_effects_through_full_dispatch() {
    let remembered = TempDir::new().expect("remembered");
    let agent_id = tau_proto::AgentId::parse("agent-malformed-gpt-workdir").expect("agent id");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    writer
        .write_event(&Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
            agent_id: agent_id.clone(),
            key: tau_proto::AgentMetadataKey::new("ext_test-extension_cwd"),
            value: CborValue::Text(remembered.path().display().to_string()),
            mutation_id: None,
            inheritable: true,
        }))
        .expect("seed workdir");
    writer.flush().expect("flush seed");
    let _ = reader.read_event().expect("context").expect("context");

    writer
        .write_event(&tool_started(
            "call-malformed-gpt-workdir",
            GPT_SHELL_TOOL_NAME,
            CborValue::Map(vec![
                (
                    CborValue::Text("command".to_owned()),
                    CborValue::Text("pwd".to_owned()),
                ),
                (
                    CborValue::Text("workdir".to_owned()),
                    CborValue::Integer(1.into()),
                ),
            ]),
            agent_id.as_str(),
        ))
        .expect("malformed GPT shell call");
    writer.flush().expect("flush call");

    loop {
        match reader.read_event().expect("read").expect("event") {
            Event::AgentMetadataSet(metadata) => {
                panic!("malformed call-local workdir emitted metadata: {metadata:?}")
            }
            Event::AgentUserMessageInjected(notice) => {
                panic!("malformed call-local workdir emitted notice: {notice:?}")
            }
            Event::ToolError(error) if error.call_id.as_str() == "call-malformed-gpt-workdir" => {
                assert_eq!(error.message, "argument `workdir` must be a string");
                break;
            }
            Event::ToolResult(result)
                if result.call_id.as_str() == "call-malformed-gpt-workdir" =>
            {
                panic!("malformed workdir unexpectedly executed: {result:?}")
            }
            _ => {}
        }
    }
}

/// Regression guard: GPT lock inference follows the advertised `workdir`
/// spelling so the frozen lock path and child process directory stay aligned.
#[test]
fn gpt_shell_workdir_selects_automatic_lock_directory() {
    let workdir = TempDir::new().expect("workdir");
    let arguments = cbor_text_map(vec![
        ("command", "pwd"),
        ("workdir", &workdir.path().display().to_string()),
    ]);

    let dirs = crate::dir_lock::automatic_lock_dirs_for_tool_in_dir(
        GPT_SHELL_TOOL_NAME,
        &arguments,
        Path::new("/"),
    )
    .expect("GPT shell lock directory");
    assert_eq!(
        dirs,
        vec![workdir.path().canonicalize().expect("canonical workdir")]
    );
}

/// Relative GPT `workdir` admission must freeze one canonical path used by
/// both later lock inference and command execution.
#[test]
fn relative_gpt_workdir_freezes_canonical_lock_path_at_admission() {
    let remembered = TempDir::new().expect("remembered");
    let child = remembered.path().join("child");
    fs::create_dir(&child).expect("child");
    let Event::ToolStarted(invoke) = tool_started(
        "call-relative-gpt-workdir",
        GPT_SHELL_TOOL_NAME,
        cbor_text_map(vec![("command", "pwd"), ("workdir", "child")]),
        "agent-relative-gpt-workdir",
    ) else {
        unreachable!();
    };

    let rewritten = rewrite_invoke_for_cwd(invoke, remembered.path());
    let canonical = child.canonicalize().expect("canonical child");
    assert_eq!(
        optional_argument_text(&rewritten.arguments, "workdir").expect("workdir argument"),
        Some(canonical.display().to_string())
    );
    assert_eq!(
        crate::dir_lock::automatic_lock_dirs_for_tool_in_dir(
            GPT_SHELL_TOOL_NAME,
            &rewritten.arguments,
            remembered.path(),
        )
        .expect("lock directory"),
        vec![canonical]
    );
}

/// Ensures omitted workdir path reads state without emitting a metadata
/// transaction.
#[test]
fn workdir_without_path_reports_current_status_without_mutation() {
    let remembered = TempDir::new().expect("remembered");
    let agent_id = tau_proto::AgentId::parse("agent-workdir-read").expect("agent id");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    writer
        .write_event(&Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
            agent_id: agent_id.clone(),
            key: tau_proto::AgentMetadataKey::new("ext_test-extension_cwd"),
            value: CborValue::Text(remembered.path().display().to_string()),
            mutation_id: None,
            inheritable: true,
        }))
        .expect("seed workdir");
    writer.flush().expect("flush seed");
    let _ = reader.read_event().expect("context").expect("context");
    writer
        .write_event(&tool_started(
            "call-workdir-read",
            WORKDIR_TOOL_NAME,
            CborValue::Map(Vec::new()),
            agent_id.as_str(),
        ))
        .expect("workdir read");
    writer.flush().expect("flush read");
    let result = loop {
        let event = reader.read_event().expect("read").expect("result");
        match event {
            Event::ToolResult(result) => break result,
            Event::AgentMetadataSet(metadata) => {
                panic!("workdir read emitted metadata: {metadata:?}")
            }
            Event::AgentUserMessageInjected(notice) => {
                panic!("workdir read emitted persistent-change notice: {notice:?}")
            }
            _ => {}
        }
    };
    assert_eq!(
        tau_proto::cbor_text_field(&result.result, "path"),
        Some(remembered.path().display().to_string())
    );
    assert_eq!(
        tau_proto::cbor_text_field(&result.result, "status"),
        Some("available".to_owned())
    );
}

/// Malformed present metadata must fail closed while an absolute setter remains
/// a repair path.
#[test]
fn absolute_workdir_setter_repairs_malformed_metadata() {
    let repaired = TempDir::new().expect("repair directory");
    let agent_id = tau_proto::AgentId::parse("agent-workdir-repair").expect("agent id");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    writer
        .write_event(&Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
            agent_id: agent_id.clone(),
            key: tau_proto::AgentMetadataKey::new("ext_test-extension_cwd"),
            value: CborValue::Text(".".to_owned()),
            mutation_id: None,
            inheritable: true,
        }))
        .expect("malformed metadata");
    writer.flush().expect("flush malformed");
    let context = reader.read_event().expect("read").expect("invalid context");
    assert!(matches!(
        context,
        Event::ExtAgentContextPublish(publish) if publish.value.0["status"] == "invalid"
    ));

    writer
        .write_event(&tool_started(
            "call-workdir-repair",
            WORKDIR_TOOL_NAME,
            cbor_text_map(vec![(
                "path",
                repaired.path().to_str().expect("UTF-8 path"),
            )]),
            agent_id.as_str(),
        ))
        .expect("absolute repair");
    writer.flush().expect("flush repair");
    let metadata = loop {
        match reader.read_event().expect("read").expect("event") {
            Event::AgentMetadataSet(metadata) => break metadata,
            Event::ToolResult(result) => panic!("repair completed before commit: {result:?}"),
            _ => {}
        }
    };
    writer
        .write_event(&Event::AgentMetadataSet(metadata))
        .expect("commit repair");
    writer.flush().expect("flush commit");
    loop {
        if matches!(
            reader.read_event().expect("read").expect("event"),
            Event::ToolResult(result) if result.call_id.as_str() == "call-workdir-repair"
        ) {
            break;
        }
    }
}

/// Relative filesystem paths resolve from the immutable admitted workdir.
#[test]
fn relative_path_tools_use_admission_workdir() {
    let temp = TempDir::new().expect("tempdir");
    let subdir = temp.path().join("src");
    std::fs::create_dir(&subdir).expect("create src");
    let agent_id = tau_proto::AgentId::parse("agent-cwd-relative").expect("agent id");
    let cwd_state = CwdState::new();
    cwd_state.set(
        agent_id.clone(),
        temp.path().canonicalize().expect("canonical temp"),
    );
    let Event::ToolStarted(invoke) = tool_started(
        "call-find",
        FIND_TOOL_NAME,
        cbor_text_map(vec![("path", "src")]),
        agent_id.as_str(),
    ) else {
        unreachable!();
    };

    let base = cwd_state.get_or_default(&agent_id).expect("remembered cwd");
    let rewritten = rewrite_invoke_for_cwd(invoke, &base);
    assert_eq!(
        optional_argument_text(&rewritten.arguments, "path").expect("path arg"),
        Some(
            subdir
                .canonicalize()
                .expect("canonical src")
                .display()
                .to_string()
        )
    );
}

/// A setter remains pending until the requested canonical value is committed.
#[test]
fn workdir_setter_waits_for_committed_metadata_before_notice_and_result() {
    let temp = TempDir::new().expect("tempdir");
    let start = temp.path().join("start");
    let next = temp.path().join("next");
    fs::create_dir_all(&start).expect("start dir");
    fs::create_dir_all(&next).expect("next dir");
    let agent_id = tau_proto::AgentId::parse("agent-cd-order").expect("agent id");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
            agent_id: agent_id.clone(),
            key: tau_proto::AgentMetadataKey::new("ext_test-extension_cwd"),
            value: CborValue::Text(start.display().to_string()),
            mutation_id: None,
            inheritable: true,
        }))
        .expect("seed cwd");
    writer.flush().expect("flush seed");
    let _ = reader.read_event().expect("read context").expect("context");

    writer
        .write_event(&tool_started(
            "call-cd-order",
            WORKDIR_TOOL_NAME,
            cbor_text_map(vec![("path", next.to_str().expect("utf8"))]),
            agent_id.as_str(),
        ))
        .expect("cd invoke");
    writer.flush().expect("flush cd");

    let metadata = loop {
        let event = reader
            .read_event()
            .expect("read metadata")
            .expect("metadata");
        assert!(
            !matches!(event, Event::ToolResult(_)),
            "cd result before metadata commit"
        );
        assert!(
            !matches!(event, Event::AgentUserMessageInjected(_)),
            "cd notice before metadata commit"
        );
        if let Event::AgentMetadataSet(metadata) = event {
            break metadata;
        }
    };
    writer
        .write_event(&Event::AgentMetadataSet(metadata))
        .expect("commit cwd");
    writer.flush().expect("flush commit");

    let context = reader.read_event().expect("read context").expect("context");
    assert!(matches!(context, Event::ExtAgentContextPublish(p) if p.key.as_ref() == "workdir"));
    let notice = reader.read_event().expect("read notice").expect("notice");
    let Event::AgentUserMessageInjected(notice) = notice else {
        panic!("expected cwd notice");
    };
    assert_eq!(notice.agent_id, agent_id);
    assert!(notice.text.contains(next.to_str().expect("utf8 next")));
    let result = reader.read_event().expect("read result").expect("result");
    assert!(
        matches!(result, Event::ToolResult(result) if result.call_id.as_str() == "call-cd-order")
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// One agent and instance may have only one outstanding persistent setter.
#[test]
fn overlapping_same_agent_workdir_setter_is_rejected_until_first_commit() {
    let temp = TempDir::new().expect("tempdir");
    let start = temp.path().join("start");
    let one = temp.path().join("one");
    let two = temp.path().join("two");
    fs::create_dir_all(&start).expect("start dir");
    fs::create_dir_all(&one).expect("one dir");
    fs::create_dir_all(&two).expect("two dir");
    let agent_id = tau_proto::AgentId::parse("agent-cd-overlap").expect("agent id");
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);

    writer
        .write_event(&Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
            agent_id: agent_id.clone(),
            key: tau_proto::AgentMetadataKey::new("ext_test-extension_cwd"),
            value: CborValue::Text(start.display().to_string()),
            mutation_id: None,
            inheritable: true,
        }))
        .expect("seed cwd");
    writer.flush().expect("flush seed");
    let _ = reader.read_event().expect("read context").expect("context");

    writer
        .write_event(&tool_started(
            "call-cd-one",
            WORKDIR_TOOL_NAME,
            cbor_text_map(vec![("path", one.to_str().expect("utf8"))]),
            agent_id.as_str(),
        ))
        .expect("first cd");
    writer.flush().expect("flush first cd");
    let first_metadata = loop {
        match reader.read_event().expect("read").expect("event") {
            Event::AgentMetadataSet(metadata) => break metadata,
            Event::ToolResult(result) => panic!("first cd completed before commit: {result:?}"),
            _ => {}
        }
    };

    writer
        .write_event(&tool_started(
            "call-cd-two",
            WORKDIR_TOOL_NAME,
            cbor_text_map(vec![("path", two.to_str().expect("utf8"))]),
            agent_id.as_str(),
        ))
        .expect("second cd");
    writer.flush().expect("flush second cd");
    loop {
        match reader.read_event().expect("read").expect("event") {
            Event::ToolError(error) if error.call_id.as_str() == "call-cd-two" => {
                assert!(error.message.contains("workdir change is already pending"));
                break;
            }
            Event::AgentMetadataSet(metadata) => {
                panic!("second cd emitted metadata while first was pending: {metadata:?}");
            }
            Event::ToolResult(result) if result.call_id.as_str() == "call-cd-one" => {
                panic!("first cd completed before commit: {result:?}");
            }
            _ => {}
        }
    }

    writer
        .write_event(&Event::AgentMetadataSet(first_metadata))
        .expect("commit first cwd");
    writer.flush().expect("flush commit");
    let _ = reader.read_event().expect("read context").expect("context");
    let _ = reader.read_event().expect("read notice").expect("notice");
    let result = reader.read_event().expect("read result").expect("result");
    assert!(
        matches!(result, Event::ToolResult(result) if result.call_id.as_str() == "call-cd-one")
    );

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// Replayed `session.agent_loaded` catch-up snapshots must rebuild cwd context
/// and emit readiness for already-loaded agents without running live tools.
#[test]
fn replayed_session_agent_loaded_restores_workdir_context_and_ready() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    let agent_id = tau_proto::AgentId::parse("agent-replay-cwd").expect("agent id");
    let cwd = PathBuf::from("/tmp/replayed-cwd");

    writer
        .write_frame(&HarnessOutputMessage::deliver_replay(
            tau_proto::UnixMicros::new(1),
            Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
                agent_id: agent_id.clone(),
                key: tau_proto::AgentMetadataKey::new("ext_test-extension_cwd"),
                value: CborValue::Text(cwd.display().to_string()),
                mutation_id: None,
                inheritable: true,
            }),
        ))
        .expect("replay cwd metadata");
    writer
        .write_frame(&HarnessOutputMessage::deliver_replay(
            tau_proto::UnixMicros::new(2),
            Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                session_id: "s1".into(),
                agent_id: agent_id.clone(),
                ephemeral: false,
            }),
        ))
        .expect("replay loaded snapshot");
    writer
        .write_frame(&HarnessOutputMessage::deliver(Event::AgentReplayComplete(
            tau_proto::AgentReplayComplete {
                agent_id: agent_id.clone(),
                session_id: Some("s1".into()),
                error: None,
            },
        )))
        .expect("replay boundary");
    writer.flush().expect("flush replay");

    let mut saw_context = false;
    loop {
        let event = reader.read_event().expect("read").expect("event");
        match event {
            Event::ExtAgentContextPublish(publish)
                if publish.agent_id == agent_id && publish.key.as_ref() == "workdir" =>
            {
                assert_eq!(
                    publish.value.0["path"],
                    serde_json::json!(cwd.display().to_string())
                );
                saw_context = true;
            }
            Event::ExtensionContextReady(ready) if ready.agent_id == agent_id => {
                assert!(saw_context, "cwd context should precede ready");
                break;
            }
            _ => {}
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// A live existing-agent load must wait for replayed cwd metadata before
/// publishing cwd context or falling back to process-default cwd metadata.
#[test]
fn live_loaded_existing_agent_uses_replayed_workdir_before_ready() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    let agent_id = tau_proto::AgentId::parse("agent-live-replay-cwd").expect("agent id");
    let stored_cwd = PathBuf::from("/tmp/stored-live-cwd");

    writer
        .write_event(&Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
            session_id: "s1".into(),
            agent_id: agent_id.clone(),
            ephemeral: false,
        }))
        .expect("live load");
    writer
        .write_frame(&HarnessOutputMessage::deliver_replay(
            tau_proto::UnixMicros::new(1),
            Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
                agent_id: agent_id.clone(),
                key: tau_proto::AgentMetadataKey::new("ext_test-extension_cwd"),
                value: CborValue::Text(stored_cwd.display().to_string()),
                mutation_id: None,
                inheritable: true,
            }),
        ))
        .expect("replay cwd");
    writer
        .write_frame(&HarnessOutputMessage::deliver(Event::AgentReplayComplete(
            tau_proto::AgentReplayComplete {
                agent_id: agent_id.clone(),
                session_id: Some("s1".into()),
                error: None,
            },
        )))
        .expect("replay boundary");
    writer.flush().expect("flush");

    let mut saw_context = false;
    loop {
        let event = reader.read_event().expect("read").expect("event");
        match event {
            Event::AgentMetadataSet(metadata) if metadata.agent_id == agent_id => {
                panic!("default cwd metadata emitted before stored cwd replay: {metadata:?}");
            }
            Event::ExtAgentContextPublish(publish)
                if publish.agent_id == agent_id && publish.key.as_ref() == "workdir" =>
            {
                assert_eq!(
                    publish.value.0["path"],
                    serde_json::json!(stored_cwd.display().to_string())
                );
                saw_context = true;
            }
            Event::ExtensionContextReady(ready) if ready.agent_id == agent_id => {
                assert!(saw_context, "stored cwd context should precede ready");
                break;
            }
            _ => {}
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// A live load with no replayed cwd still falls back to process-default
/// metadata after the per-agent replay boundary, preserving new-agent
/// initialization.
#[test]
fn live_loaded_agent_defaults_workdir_after_replay_boundary_without_metadata() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    let agent_id = tau_proto::AgentId::parse("agent-live-default-cwd").expect("agent id");

    writer
        .write_event(&Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
            session_id: "s1".into(),
            agent_id: agent_id.clone(),
            ephemeral: false,
        }))
        .expect("live load");
    writer
        .write_frame(&HarnessOutputMessage::deliver(Event::AgentReplayComplete(
            tau_proto::AgentReplayComplete {
                agent_id: agent_id.clone(),
                session_id: Some("s1".into()),
                error: None,
            },
        )))
        .expect("replay boundary");
    writer.flush().expect("flush");

    loop {
        let event = reader.read_event().expect("read").expect("event");
        if let Event::AgentMetadataSet(metadata) = event
            && metadata.agent_id == agent_id
        {
            assert_eq!(metadata.key.as_str(), "ext_test-extension_cwd");
            break;
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}

/// An errored per-agent replay boundary must fail closed: shell must not invent
/// default cwd metadata or publish readiness when restore failed.
#[test]
fn live_loaded_agent_does_not_default_workdir_after_replay_error() {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut runtime = ShellRuntime::new(Output::channel(tx), ExtConfig::default());
    let agent_id = tau_proto::AgentId::parse("agent-live-error-cwd").expect("agent id");

    runtime
        .handle_event(
            Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                session_id: "s1".into(),
                agent_id: agent_id.clone(),
                ephemeral: false,
            }),
            false,
        )
        .expect("live load");
    runtime
        .handle_event(
            Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
                agent_id: agent_id.clone(),
                key: tau_proto::AgentMetadataKey::new("ext_core-shell_cwd"),
                value: CborValue::Text("/tmp/restored-before-error".to_owned()),
                mutation_id: None,
                inheritable: true,
            }),
            true,
        )
        .expect("replay cwd metadata");
    runtime
        .handle_event(
            Event::AgentReplayComplete(tau_proto::AgentReplayComplete {
                agent_id: agent_id.clone(),
                session_id: Some("s1".into()),
                error: Some("corrupt agent log".to_owned()),
            }),
            false,
        )
        .expect("errored replay boundary");

    while let Ok(message) = rx.try_recv() {
        let HarnessInputMessage::Emit(emit) = message else {
            continue;
        };
        match *emit.event {
            Event::AgentMetadataSet(metadata) if metadata.agent_id == agent_id => {
                panic!("errored replay must not synthesize cwd metadata: {metadata:?}");
            }
            Event::ExtAgentContextPublish(publish) if publish.agent_id == agent_id => {
                panic!("errored replay must not publish cwd context: {publish:?}");
            }
            Event::ExtensionContextReady(ready) if ready.agent_id == agent_id => {
                panic!("errored replay must not mark cwd context ready: {ready:?}");
            }
            _ => {}
        }
    }
    runtime
        .handle_event(
            Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
                agent_id: agent_id.clone(),
                key: tau_proto::AgentMetadataKey::new("ext_core-shell_cwd"),
                value: CborValue::Text("/tmp/late".to_owned()),
                mutation_id: None,
                inheritable: true,
            }),
            false,
        )
        .expect("late metadata");
    runtime
        .handle_event(
            Event::AgentMetadataUnset(tau_proto::AgentMetadataUnset {
                agent_id: agent_id.clone(),
                key: tau_proto::AgentMetadataKey::new("ext_core-shell_cwd"),
            }),
            false,
        )
        .expect("late unset");
    runtime
        .handle_event(
            Event::UiShellCommand(tau_proto::UiShellCommand {
                session_id: "s1".into(),
                command_id: "replay-failed-shell".into(),
                command: "pwd".to_owned(),
                include_in_context: false,
                target_agent_id: Some(agent_id),
            }),
            false,
        )
        .expect("replay-failed command is a local failure");
    let failure = rx.recv().expect("replay-failed terminal result");
    assert!(matches!(
        failure,
        HarnessInputMessage::Emit(emit)
            if matches!(emit.event.as_ref(), Event::ShellCommandFinished(finished)
                if finished.output.contains("replay failed"))
    ));
    runtime.final_shutdown();
}

/// Malformed live metadata publishes invalid status before context readiness.
#[test]
fn malformed_workdir_metadata_does_not_wedge_context_ready() {
    let (mut reader, mut writer) = spawn_extension();
    drain_startup(&mut reader);
    let agent_id = tau_proto::AgentId::parse("agent-bad-cwd").expect("agent id");

    writer
        .write_event(&Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
            session_id: "s1".into(),
            agent_id: agent_id.clone(),
            ephemeral: false,
        }))
        .expect("load");
    writer
        .write_frame(&HarnessOutputMessage::deliver(Event::AgentReplayComplete(
            tau_proto::AgentReplayComplete {
                agent_id: agent_id.clone(),
                session_id: Some("s1".into()),
                error: None,
            },
        )))
        .expect("agent replay boundary");
    writer.flush().expect("flush load");
    let _ = reader
        .read_event()
        .expect("read initial metadata")
        .expect("metadata");

    writer
        .write_event(&Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
            agent_id: agent_id.clone(),
            key: tau_proto::AgentMetadataKey::new("ext_test-extension_cwd"),
            value: CborValue::Bool(true),
            mutation_id: None,
            inheritable: true,
        }))
        .expect("bad metadata");
    writer.flush().expect("flush bad metadata");

    let mut saw_context = false;
    loop {
        let event = reader.read_event().expect("read").expect("event");
        match event {
            Event::ExtAgentContextPublish(publish) if publish.key.as_ref() == "workdir" => {
                saw_context = true;
            }
            Event::ExtensionContextReady(ready) => {
                assert!(saw_context);
                assert_eq!(ready.agent_id, agent_id);
                break;
            }
            _ => {}
        }
    }

    writer
        .write_frame(&disconnect_frame(None))
        .expect("disconnect");
    writer.flush().expect("flush");
}
