use std::io::{BufReader, BufWriter, Error, ErrorKind, Read};
use std::net::Shutdown;
use std::os::unix::fs::symlink;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Condvar;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use std::{
    collections as path_std_collections, fs as path_std_fs, fs, io as path_std_io_error,
    io as path_std_io, path as path_std_path, process as path_std_process, sync as path_std_sync,
    thread, time as path_std_time,
};

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
use crate::tool_lifecycle::{ToolCancellationState, ToolLifecycleRegistry};
use crate::tools::edit::edit_file as edit_file_with_world;
use crate::tools::find::run_find;
use crate::tools::grep::{grep_result_map, run_grep};
use crate::tools::ls::run_ls;
use crate::tools::read::{format_read_range, read_file as read_file_with_world, slice_lines};
use crate::tools::shell::{
    CommandDetails, CommandOutcome, command_details_value, run_command_live,
};
use crate::tools::{
    APPLY_PATCH_TOOL_NAME, EDIT_TOOL_NAME, FIND_TOOL_NAME, GPT_SHELL_TOOL_NAME, LS_TOOL_NAME,
    READ_TOOL_NAME, REPLACE_TOOL_NAME, SHELL_TOOL_NAME, shell as path_crate_tools_shell,
    world as path_crate_tools_world,
};
use crate::truncate::{
    MAX_OUTPUT_BYTES, MAX_OUTPUT_LINES, mark_line, truncate_head, truncate_tail,
};
use crate::{config as path_crate_config, tools as path_crate_tools};

const TEST_SAFE_FILE_READ_LIMIT: u64 = 10 * 1024 * 1024;
static SATURATION_FIXTURE_LOCK: Mutex<()> = Mutex::new(());

/// Panic-safe ownership of the process-global detached-overload test observer.
struct SaturationHookGuard {
    /// Serial guard preventing cross-fixture notification.
    _fixture: std::sync::MutexGuard<'static, ()>,
}

impl SaturationHookGuard {
    /// Serializes the fixture and installs its unique overload observer.
    fn install(notify: mpsc::Sender<()>) -> Self {
        let fixture = SATURATION_FIXTURE_LOCK
            .lock()
            .expect("saturation fixture lock");
        *DETACHED_OUTPUT_OVERLOAD_NOTIFY
            .lock()
            .expect("overload notification") = Some(notify);
        Self { _fixture: fixture }
    }
}

impl Drop for SaturationHookGuard {
    fn drop(&mut self) {
        DETACHED_OUTPUT_OVERLOAD_NOTIFY
            .lock()
            .expect("overload notification")
            .take();
    }
}

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

/// Writer that rejects one selected mandatory frame after startup.
struct MandatoryFrameFailureWriter {
    /// Protocol event-name bytes that identify the mandatory frame.
    needle: &'static [u8],
    /// Whether startup reached `Ready`.
    ready: bool,
    /// Whether the selected frame reached the writer.
    failed: Arc<AtomicBool>,
}

/// Production writer blocked on the first optional action error.
struct SaturationWriter {
    /// Serialized extension output.
    bytes: Arc<Mutex<Vec<u8>>>,
    /// Gate held while detached output reaches actual overload.
    gate: Arc<(Mutex<bool>, Condvar)>,
    /// Notification that the writer reached optional output.
    entered: mpsc::Sender<()>,
    /// Prevents repeated blocking.
    blocked: bool,
    /// Notification that a mandatory frame reached the writer.
    mandatory: mpsc::Sender<()>,
    /// Mandatory bytes observed since the previous flush.
    mandatory_pending: bool,
}

impl std::io::Write for SaturationWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if !self.blocked
            && bytes
                .windows(21)
                .any(|window| window == b"action.error_reported")
        {
            self.blocked = true;
            self.entered.send(()).expect("writer entry notification");
            let (lock, condvar) = &*self.gate;
            let mut closed = lock.lock().expect("writer gate");
            while *closed {
                closed = condvar.wait(closed).expect("writer gate wait");
            }
        }
        let mandatory = [
            b"tool.result_reported".as_slice(),
            b"tool.error_reported".as_slice(),
            b"tool.cancelled_reported".as_slice(),
            b"shell.command_finished_reported".as_slice(),
            b"agent.metadata_set_request".as_slice(),
        ]
        .iter()
        .any(|needle| bytes.windows(needle.len()).any(|window| window == *needle));
        self.bytes.lock().expect("writer bytes").extend(bytes);
        self.mandatory_pending |= mandatory;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if std::mem::take(&mut self.mandatory_pending) {
            let _ = self.mandatory.send(());
        }
        Ok(())
    }
}

impl std::io::Write for MandatoryFrameFailureWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.windows(5).any(|window| window == b"ready") {
            self.ready = true;
        }
        if self.ready
            && bytes
                .windows(self.needle.len())
                .any(|window| window == self.needle)
        {
            self.failed.store(true, Ordering::Release);
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.failed.load(Ordering::Acquire) {
            return Err(path_std_io_error::Error::other(
                "forced mandatory frame failure",
            ));
        }
        Ok(())
    }
}

fn read_file(
    arguments: &CborValue,
) -> Result<crate::display::ToolOutput, crate::display::ToolFailure> {
    let mut world = path_crate_tools_world::ShellWorld::real();
    read_file_with_world(arguments, &mut world)
}

fn edit_file(
    arguments: &CborValue,
) -> Result<crate::display::ToolOutput, crate::display::ToolFailure> {
    let mut world = path_crate_tools_world::ShellWorld::real();
    edit_file_with_world(arguments, &mut world)
}

type TestExtensionReader = EventReader<BufReader<DeadlineSocket>>;
type TestExtensionWriter = EventWriter<BufWriter<UnixStream>>;
type TestExtensionDone = path_std_sync::mpsc::Receiver<Result<(), String>>;

/// Shared deadline state for one test-side extension socket.
struct DeadlineControl {
    /// Socket handle used to restore timeout state or retire a failed
    /// connection.
    stream: UnixStream,
    /// Absolute deadline armed for the current scoped wait.
    deadline: Mutex<Option<Instant>>,
}

impl DeadlineControl {
    /// Arms one absolute deadline until the returned guard is dropped.
    fn arm(self: &Arc<Self>, deadline: Instant) -> DeadlineGuard {
        let previous_timeout = self.stream.read_timeout().expect("read socket timeout");
        let mut current = self.deadline.lock().expect("test read deadline");
        assert!(current.is_none(), "test read deadline already armed");
        *current = Some(deadline);
        drop(current);
        DeadlineGuard {
            control: Arc::clone(self),
            previous_timeout,
            successful: false,
        }
    }

    /// Returns the positive remaining duration or retires an expired
    /// connection.
    fn remaining(&self) -> std::io::Result<Option<Duration>> {
        let deadline = *self.deadline.lock().expect("test read deadline");
        let Some(deadline) = deadline else {
            return Ok(None);
        };
        match deadline.checked_duration_since(Instant::now()) {
            Some(remaining) if !remaining.is_zero() => Ok(Some(remaining)),
            _ => {
                self.retire();
                Err(Error::new(
                    ErrorKind::TimedOut,
                    "test extension read deadline expired",
                ))
            }
        }
    }

    /// Shuts down a connection whose decoder can no longer be reused safely.
    fn retire(&self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

/// Socket reader that reapplies an absolute deadline before every underlying
/// read.
struct DeadlineSocket {
    /// Socket carrying extension output.
    stream: UnixStream,
    /// Deadline shared with scoped wait guards.
    control: Arc<DeadlineControl>,
}

impl DeadlineSocket {
    /// Wraps a socket and returns its independently owned deadline control.
    fn new(stream: UnixStream) -> (Self, Arc<DeadlineControl>) {
        let control = Arc::new(DeadlineControl {
            stream: stream
                .try_clone()
                .expect("deadline control socket clone should succeed"),
            deadline: Mutex::new(None),
        });
        (
            Self {
                stream,
                control: Arc::clone(&control),
            },
            control,
        )
    }
}

impl Read for DeadlineSocket {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let Some(remaining) = self.control.remaining()? else {
                return self.stream.read(buffer);
            };
            self.stream.set_read_timeout(Some(remaining))?;
            match self.stream.read(buffer) {
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::Interrupted
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    self.control.remaining()?;
                    continue;
                }
                result => return result,
            }
        }
    }
}

/// Panic-safe restoration of the socket state used before a scoped wait.
struct DeadlineGuard {
    /// Deadline and socket state owned by this scope.
    control: Arc<DeadlineControl>,
    /// Socket timeout that was active before the wait.
    previous_timeout: Option<Duration>,
    /// Whether the scoped wait reached its expected event.
    successful: bool,
}

impl Drop for DeadlineGuard {
    fn drop(&mut self) {
        if !self.successful {
            self.control.retire();
        }
        *self.control.deadline.lock().expect("test read deadline") = None;
        let _ = self.stream().set_read_timeout(self.previous_timeout);
    }
}

impl DeadlineGuard {
    /// Marks the scoped wait successful before restoring its prior socket
    /// state.
    fn finish(mut self) {
        self.successful = true;
    }

    /// Returns the socket used to restore the prior timeout.
    fn stream(&self) -> &UnixStream {
        &self.control.stream
    }
}

/// Test-side wrapper around [`HarnessInputReader`] that exposes an
/// `Event`-flavoured API so the existing tests can stay mechanical. Non-event
/// messages are skipped by `read_event`.
struct EventReader<R> {
    inner: HarnessInputReader<R>,
    /// Optional deadline control for socket-backed integration tests.
    deadline: Option<Arc<DeadlineControl>>,
}

impl<R: std::io::Read> EventReader<R> {
    fn read_event(&mut self) -> Result<Option<Event>, tau_proto::DecodeError> {
        loop {
            self.check_deadline()?;
            match self.inner.read_message()? {
                None => return Ok(None),
                Some(HarnessInputMessage::Emit(emit)) => {
                    self.check_deadline()?;
                    match *emit.event {
                        Event::ToolProgressReported(progress)
                            if progress.message.is_none()
                                && progress.display.is_some()
                                && progress.tool_name != SHELL_TOOL_NAME
                                && progress.tool_name != GPT_SHELL_TOOL_NAME =>
                        {
                            continue;
                        }
                        // Most ext-shell tests exercise tool payload semantics rather
                        // than the peer/canonical wire split. Normalize reports back
                        // to their canonical payload shape here; focused protocol
                        // tests inspect `read_raw_message` instead.
                        Event::ToolResultReported(result) => {
                            return Ok(Some(Event::ToolResult(result)));
                        }
                        Event::ToolErrorReported(error) => {
                            return Ok(Some(Event::ToolError(error)));
                        }
                        Event::ToolCancelledReported(cancelled) => {
                            return Ok(Some(Event::ToolCancelled(cancelled)));
                        }
                        event => return Ok(Some(event)),
                    }
                }
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

    /// Checks an armed deadline before reading, skipping, or accepting a
    /// message.
    fn check_deadline(&self) -> Result<(), tau_proto::DecodeError> {
        if let Some(control) = &self.deadline {
            control
                .remaining()
                .map(|_| ())
                .map_err(tau_proto::DecodeError::Io)
        } else {
            Ok(())
        }
    }
}

impl EventReader<BufReader<DeadlineSocket>> {
    /// Builds a test reader whose socket reads can be bounded by scoped
    /// deadlines.
    fn deadline_aware(stream: UnixStream) -> Self {
        let (stream, deadline) = DeadlineSocket::new(stream);
        Self {
            inner: HarnessInputReader::new(BufReader::new(stream)),
            deadline: Some(deadline),
        }
    }

    /// Arms an absolute read deadline and restores prior state when the guard
    /// drops.
    fn arm_deadline(&self, deadline: Instant) -> DeadlineGuard {
        self.deadline
            .as_ref()
            .expect("deadline-aware event reader")
            .arm(deadline)
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
    let (done_tx, done_rx) = path_std_sync::mpsc::channel();
    thread::spawn(move || {
        let result = run_impl(
            reader_stream,
            runtime_stream,
            DiscoverySourcePolicy::Environment,
            RuntimeCwdSource::Process,
        )
        .map_err(|error| format!("extension should run: {error}"));
        let _ = done_tx.send(result);
    });
    let reader = EventReader::deadline_aware(
        harness_stream
            .try_clone()
            .expect("harness reader clone should succeed"),
    );
    let mut writer = EventWriter::new(BufWriter::new(harness_stream));
    writer
        .write_frame(&HarnessOutputMessage::Configure(tau_proto::Configure {
            purpose: tau_proto::ConfigurePurpose::Runtime,
            harness_protocol_version: None,
            tool_prefix,
            instance_name: tau_proto::ExtensionName::parse("test-extension")
                .expect("test extension name must satisfy the identifier grammar"),
            config: CborValue::Map(Vec::new()),
            state_dir: None,
            secrets: Default::default(),
            settings_files: Default::default(),
        }))
        .expect("write initial configure");
    writer.flush().expect("flush initial configure");
    (reader, writer, done_rx)
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

fn replace_arguments(path: &Path, old_text: &str, new_text: &str) -> CborValue {
    cbor_map(vec![
        ("path", CborValue::Text(path.display().to_string())),
        (
            "edits",
            CborValue::Array(vec![cbor_map(vec![
                ("oldText", CborValue::Text(old_text.to_owned())),
                ("newText", CborValue::Text(new_text.to_owned())),
            ])]),
        ),
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
            purpose: tau_proto::ConfigurePurpose::Runtime,
            harness_protocol_version: None,
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::parse("test-extension")
                .expect("test extension name must satisfy the identifier grammar"),
            config: cbor_map(vec![(
                "dir_lock",
                cbor_map(vec![("enable", CborValue::Bool(enable))]),
            )]),
            state_dir: None,
            secrets: Default::default(),
            settings_files: Default::default(),
        }))
        .expect("configure dir_lock");
    writer.flush().expect("flush config");
}

/// Configure explicit command-regex shell allowlist rules for integration
/// tests.
fn send_shell_regex_allowlist_config(
    writer: &mut EventWriter<BufWriter<UnixStream>>,
    rules: Vec<(&str, &str, Option<&str>)>,
) {
    let rules = rules
        .into_iter()
        .map(|(workdir, command_regex, description)| {
            let mut fields = vec![
                ("workdir", CborValue::Text(workdir.to_owned())),
                ("command_regex", CborValue::Text(command_regex.to_owned())),
            ];
            if let Some(description) = description {
                fields.push(("description", CborValue::Text(description.to_owned())));
            }
            cbor_map(fields)
        })
        .collect();
    writer
        .write_frame(&HarnessOutputMessage::Configure(tau_proto::Configure {
            purpose: tau_proto::ConfigurePurpose::Runtime,
            harness_protocol_version: None,
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::parse("test-extension")
                .expect("test extension name"),
            config: cbor_map(vec![(
                "shell",
                cbor_map(vec![("allowlist", CborValue::Array(rules))]),
            )]),
            state_dir: None,
            secrets: Default::default(),
            settings_files: Default::default(),
        }))
        .expect("configure shell regex allowlist");
    writer.flush().expect("flush shell regex allowlist");
}

fn tool_started(call_id: &str, tool_name: &str, arguments: CborValue, agent_id: &str) -> Event {
    Event::ToolStarted(ToolStarted {
        invocation_policy: tau_proto::ToolInvocationPolicy::default(),
        call_id: tau_proto::ToolCallId::new(call_id),
        tool_name: tau_proto::ToolName::new(tool_name),
        arguments,
        agent_id: tau_proto::AgentId::parse(agent_id).expect("agent id"),
        originator: tau_proto::PromptOriginator::User,
    })
}

fn action_invoke(invocation_id: &str, action_id: &str, directory: &str) -> Event {
    Event::ActionInvoke(tau_proto::ActionInvoke {
        invocation_id: test_action_invocation_id(invocation_id),
        session_id: "session-1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        extension_name: test_extension_name("tau-ext-shell"),
        instance_id: 0.into(),
        action_id: action_id.to_owned(),
        raw_line: format!(":shell-dir-force-unlock {directory}"),
        argv: vec![directory.to_owned()],
        arguments: cbor_text_map(vec![("directory", directory)]),
    })
}

fn ui_shell_command(command_id: &str, command: &str) -> Event {
    Event::UiShellCommand(tau_proto::UiShellCommand {
        session_id: "session-1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        command_id: test_shell_command_id(command_id),
        command: command.to_owned(),
        include_in_context: true,
        target_agent_id: None,
    })
}

/// Consumes startup events (tool registration declarations). The
/// hello/subscribe/ready messages are filtered out by the test-side
/// `EventReader` wrapper.
fn drain_startup(reader: &mut TestExtensionReader) {
    for expected in [
        EventName::TOOL_REGISTRATION_DECLARED,                  // echo
        EventName::TOOL_REGISTRATION_DECLARED,                  // read
        EventName::TOOL_REGISTRATION_DECLARED,                  // export
        EventName::TOOL_REGISTRATION_DECLARED,                  // import
        EventName::TOOL_REGISTRATION_DECLARED,                  // edit
        EventName::TOOL_REGISTRATION_DECLARED,                  // replace
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
        EventName::ACTION_SCHEMA_DECLARED,                      // shell-dir-force-unlock
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
    let deadline_guard = reader.arm_deadline(deadline);
    loop {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for user shell progress"
        );
        match reader.read_event().unwrap_or_else(|error| {
            if matches!(&error, tau_proto::DecodeError::Io(error) if error.kind() == std::io::ErrorKind::TimedOut)
            {
                panic!("timed out waiting for user shell progress");
            }
            panic!("read progress: {error}");
        }) {
            Some(Event::ShellCommandProgressReported(progress))
                if progress.command_id == command_id && progress.chunk.contains(text) =>
            {
                deadline_guard.finish();
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
    let deadline_guard = reader.arm_deadline(deadline);
    loop {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for user shell finish"
        );
        match reader.read_event().unwrap_or_else(|error| {
            if matches!(&error, tau_proto::DecodeError::Io(error) if error.kind() == std::io::ErrorKind::TimedOut)
            {
                panic!("timed out waiting for user shell finish");
            }
            panic!("read finish: {error}");
        }) {
            Some(Event::ShellCommandFinishedReported(finished))
                if finished.command_id == command_id =>
            {
                deadline_guard.finish();
                return finished;
            }
            Some(_) => {}
            None => panic!("extension closed before user shell finish"),
        }
    }
}

/// Blocks the production writer, observes actual detached FIFO exhaustion, then
/// admits one live mandatory boundary and returns every successfully flushed
/// frame.
fn run_after_production_fifo_saturation(
    events: Vec<Event>,
    echo_workdir: bool,
) -> Vec<HarnessInputMessage> {
    let (runtime_stream, harness_stream) = UnixStream::pair().expect("stream pair");
    let runtime_reader = runtime_stream.try_clone().expect("runtime reader");
    drop(runtime_stream);
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let gate = Arc::new((Mutex::new(true), Condvar::new()));
    let (entered_tx, entered_rx) = mpsc::channel();
    let (overloaded_tx, overloaded_rx) = mpsc::channel();
    let (mandatory_tx, mandatory_rx) = mpsc::channel();
    let _hook = SaturationHookGuard::install(overloaded_tx);
    let writer = SaturationWriter {
        bytes: Arc::clone(&bytes),
        gate: Arc::clone(&gate),
        entered: entered_tx,
        blocked: false,
        mandatory: mandatory_tx,
        mandatory_pending: false,
    };
    let runner = thread::spawn(move || {
        run_impl(
            runtime_reader,
            writer,
            DiscoverySourcePolicy::EmptyFixture,
            RuntimeCwdSource::Fixture(PathBuf::from("/tmp")),
        )
        .map_err(|error| error.to_string())
    });
    let mut input = EventWriter::new(harness_stream);
    input
        .write_frame(&HarnessOutputMessage::Configure(tau_proto::Configure {
            purpose: tau_proto::ConfigurePurpose::Runtime,
            harness_protocol_version: None,
            tool_prefix: None,
            instance_name: test_extension_name("test-extension"),
            config: CborValue::Map(Vec::new()),
            state_dir: None,
            secrets: Default::default(),
            settings_files: Default::default(),
        }))
        .expect("configure");
    input.flush().expect("flush configure");
    input
        .write_event(&action_invoke(
            "optional-saturation",
            "test.saturate-optional-output",
            "/tmp",
        ))
        .expect("optional saturation action");
    input.flush().expect("flush optional actions");
    entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("optional output reached writer");
    overloaded_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("detached FIFO reached overload");
    let (lock, condvar) = &*gate;
    *lock.lock().expect("writer gate") = false;
    condvar.notify_all();
    while mandatory_rx.try_recv().is_ok() {}
    for event in events {
        input.write_event(&event).expect("mandatory event");
    }
    input.flush().expect("flush mandatory events");
    mandatory_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("mandatory frame reached writer");
    if echo_workdir {
        let output = bytes.lock().expect("output bytes").clone();
        let mut reader = HarnessInputReader::new(path_std_io::Cursor::new(output));
        let parsed = std::iter::from_fn(|| reader.read_message().transpose())
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        let request = parsed
            .into_iter()
            .find_map(|frame| match frame {
                HarnessInputMessage::Emit(emit) => match *emit.event {
                    Event::AgentMetadataSetRequest(request) => Some(request),
                    _ => None,
                },
                _ => None,
            })
            .expect("correlated workdir prerequisite");
        input
            .write_event(&Event::AgentMetadataSet(request))
            .expect("canonical metadata echo");
        input.flush().expect("flush canonical echo");
        mandatory_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("echo-correlated terminal reached writer");
    }
    drop(input);
    runner
        .join()
        .expect("extension runner")
        .expect("mandatory output flush");
    let output = bytes.lock().expect("output bytes").clone();
    let mut reader = HarnessInputReader::new(path_std_io::Cursor::new(output));
    std::iter::from_fn(|| reader.read_message().transpose())
        .filter_map(Result::ok)
        .collect()
}

/// Counts exact reported terminal frames for one call and event name.
fn count_reported_terminal(
    frames: &[HarnessInputMessage],
    call_id: &str,
    event_name: EventName,
) -> usize {
    frames
        .iter()
        .filter(|frame| {
            matches!(
                frame,
                HarnessInputMessage::Emit(emit)
                    if emit.event.name() == event_name
                        && match emit.event.as_ref() {
                            Event::ToolResultReported(event) => event.call_id.as_str() == call_id,
                            Event::ToolErrorReported(event) => event.call_id.as_str() == call_id,
                            Event::ToolCancelledReported(event) => event.call_id.as_str() == call_id,
                            _ => false,
                        }
            )
        })
        .count()
}

/// Drives one live production adapter frame into a writer that fails the
/// selected mandatory flush and requires prompt loop teardown.
fn assert_mandatory_frame_failure_exits(event: Event, needle: &'static [u8], label: &str) {
    let (runtime_stream, harness_stream) = UnixStream::pair().expect("stream pair");
    let runtime_reader = runtime_stream.try_clone().expect("runtime reader");
    let failed = Arc::new(AtomicBool::new(false));
    let writer = MandatoryFrameFailureWriter {
        needle,
        ready: false,
        failed: Arc::clone(&failed),
    };
    let (done_tx, done_rx) = path_std_sync::mpsc::channel();
    thread::spawn(move || {
        let result = run_impl(
            runtime_reader,
            writer,
            DiscoverySourcePolicy::EmptyFixture,
            RuntimeCwdSource::Fixture(PathBuf::from("/tmp")),
        )
        .map_err(|error| error.to_string());
        let _ = done_tx.send(result);
    });
    let mut input = EventWriter::new(harness_stream);
    input
        .write_frame(&HarnessOutputMessage::Configure(tau_proto::Configure {
            purpose: tau_proto::ConfigurePurpose::Runtime,
            harness_protocol_version: None,
            tool_prefix: None,
            instance_name: test_extension_name("test-extension"),
            config: CborValue::Map(Vec::new()),
            state_dir: None,
            secrets: Default::default(),
            settings_files: Default::default(),
        }))
        .expect("configure");
    input.write_event(&event).expect("mandatory input");
    input.flush().expect("flush mandatory input");
    let result = done_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap_or_else(|_| panic!("{label} failure must exit the extension loop"));
    assert!(
        failed.load(Ordering::Acquire),
        "{label} did not reach the production writer"
    );
    assert!(result.is_err(), "{label} failure unexpectedly succeeded");
}

/// Assert that one cancellation report and no second terminal were emitted.
fn assert_cancelled_terminal_once(
    rx: &path_std_sync::mpsc::Receiver<HarnessInputMessage>,
    call_id: &tau_proto::ToolCallId,
) {
    let HarnessInputMessage::Emit(emit) = rx
        .recv_timeout(Duration::from_secs(1))
        .expect("cancelled terminal")
    else {
        panic!("expected emit");
    };
    let Event::ToolCancelledReported(cancelled) = *emit.event else {
        panic!("expected ToolCancelledReported");
    };
    assert_eq!(&cancelled.call_id, call_id);
    assert!(
        rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "cancellation must emit exactly one terminal"
    );
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

/// Builds a validated extension name used by this test module.
fn test_extension_name(value: impl AsRef<str>) -> tau_proto::ExtensionName {
    tau_proto::ExtensionName::parse(value.as_ref())
        .expect("test extension name must satisfy the identifier grammar")
}

/// Builds a validated shell command id used by this test module.
fn test_shell_command_id(value: impl AsRef<str>) -> tau_proto::ShellCommandId {
    tau_proto::ShellCommandId::parse(value.as_ref())
        .expect("test identifier must satisfy its grammar")
}

/// Builds a validated action invocation id used by this test module.
fn test_action_invocation_id(value: impl AsRef<str>) -> tau_proto::ActionInvocationId {
    tau_proto::ActionInvocationId::parse(value.as_ref())
        .expect("test identifier must satisfy its grammar")
}

mod agent_discovery;
mod argument_and_output_helpers;
mod deadline_reader;
mod directory_locking;
mod extension_lifecycle;
mod filesystem_tools;
mod process_lifecycle;
mod scheduler_queue;
