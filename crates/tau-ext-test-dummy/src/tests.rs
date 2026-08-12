use std::collections as path_std_collections;
use std::fs::DirBuilder;
use std::io::{Cursor, Error, Read};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

use tau_proto::{
    AgentPromptSubmitted, CborValue, Configure, Event, HarnessInputMessage, HarnessInputReader,
    HarnessOutputMessage, HarnessOutputWriter, InterceptReply, InterceptRequest,
    PromptMessageClass, PromptOriginator, ToolStarted, UnixMicros,
};

use super::*;
static SATURATION_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Panic-safe installation of one correlated production saturation hook.
struct SaturationHookGuard;

impl Drop for SaturationHookGuard {
    fn drop(&mut self) {
        SATURATION_HOOK.lock().expect("saturation hook").take();
    }
}

/// Cloneable output sink used while a release client runs concurrently.
#[derive(Clone, Default)]
struct SharedWriter {
    /// Bytes emitted by all cloned writers.
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl std::io::Write for SharedWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.bytes
            .lock()
            .expect("output lock")
            .extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl SharedWriter {
    /// Returns a snapshot of all bytes emitted so far.
    fn snapshot(&self) -> Vec<u8> {
        self.bytes.lock().expect("output lock").clone()
    }
}

/// Production writer blocked by the first saturation filler frame.
struct SaturationWriter {
    /// Serialized output bytes.
    bytes: Arc<Mutex<Vec<u8>>>,
    /// Writer release gate.
    gate: Arc<(Mutex<bool>, Condvar)>,
    /// Announces that production output is blocked.
    entered: mpsc::Sender<()>,
    /// Prevents repeated blocking.
    blocked: bool,
}

impl std::io::Write for SaturationWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if !self.blocked && bytes.windows(9).any(|window| window == b"term.bell") {
            self.blocked = true;
            let _ = self.entered.send(());
            let (lock, wake) = &*self.gate;
            let mut closed = lock.lock().expect("writer gate");
            while *closed {
                closed = wake.wait(closed).expect("writer gate wait");
            }
        }
        self.bytes.lock().expect("output bytes").extend(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Writer that rejects the first mandatory hold terminal.
struct TerminalFailureWriter {
    /// Bytes preceding the rejected terminal.
    bytes: Arc<Mutex<Vec<u8>>>,
    /// Whether terminal output reached the writer.
    failed: bool,
}

impl std::io::Write for TerminalFailureWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.failed |= [
            b"tool.result_reported".as_slice(),
            b"tool.error_reported",
            b"tool.cancelled_reported",
        ]
        .iter()
        .any(|needle| bytes.windows(needle.len()).any(|window| window == *needle));
        if !self.failed {
            self.bytes.lock().expect("output bytes").extend(bytes);
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.failed {
            Err(Error::other("forced mandatory dummy output failure"))
        } else {
            Ok(())
        }
    }
}

fn invoke_restart() -> HarnessOutputMessage {
    invoke_restart_with_id("call-1")
}

fn invoke_restart_with_id(call_id: &str) -> HarnessOutputMessage {
    HarnessOutputMessage::deliver(Event::ToolStarted(ToolStarted {
        call_id: call_id.into(),
        tool_name: tau_proto::ToolName::new(RESTART_TEST_DUMMY_TOOL_NAME),
        arguments: tau_proto::CborValue::Map(Vec::new()),
        agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
        originator: tau_proto::PromptOriginator::User,
    }))
}

fn extension_originated_restart() -> HarnessOutputMessage {
    HarnessOutputMessage::deliver(Event::ToolStarted(ToolStarted {
        call_id: "extension-call".into(),
        tool_name: tau_proto::ToolName::new(RESTART_TEST_DUMMY_TOOL_NAME),
        arguments: tau_proto::CborValue::Map(Vec::new()),
        agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
        originator: PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("fixture")
                .expect("test extension name must satisfy the identifier grammar"),
            query_id: "query-1".to_owned(),
        },
    }))
}

fn replayed_restart() -> HarnessOutputMessage {
    HarnessOutputMessage::deliver_replay(
        UnixMicros::new(1_700_000_000_000_000),
        Event::ToolStarted(ToolStarted {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(RESTART_TEST_DUMMY_TOOL_NAME),
            arguments: tau_proto::CborValue::Map(Vec::new()),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: PromptOriginator::User,
        }),
    )
}

fn restart_config(mode: &str) -> HarnessOutputMessage {
    HarnessOutputMessage::Configure(Configure {
        tool_prefix: None,
        instance_name: tau_proto::ExtensionName::parse("test-extension")
            .expect("test extension name must satisfy the identifier grammar"),
        config: CborValue::Map(vec![(
            CborValue::Text("restart_mode".to_owned()),
            CborValue::Text(mode.to_owned()),
        )]),
        state_dir: None,
        secrets: path_std_collections::BTreeMap::new(),
        settings_files: Default::default(),
    })
}

fn release_config(socket_path: &std::path::Path, nonce: &str) -> HarnessOutputMessage {
    HarnessOutputMessage::Configure(Configure {
        tool_prefix: None,
        instance_name: tau_proto::ExtensionName::parse("test-extension")
            .expect("test extension name must satisfy the identifier grammar"),
        config: CborValue::Map(vec![
            (
                CborValue::Text("restart_mode".to_owned()),
                CborValue::Text("hold_until_success_release".to_owned()),
            ),
            (
                CborValue::Text("release_socket_path".to_owned()),
                CborValue::Text(socket_path.to_string_lossy().into_owned()),
            ),
            (
                CborValue::Text("release_nonce".to_owned()),
                CborValue::Text(nonce.to_owned()),
            ),
        ]),
        state_dir: None,
        secrets: path_std_collections::BTreeMap::new(),
        settings_files: Default::default(),
    })
}

fn unique_socket_path(label: &str) -> PathBuf {
    use std::os::unix::fs::DirBuilderExt;

    let root = std::env::temp_dir().join(format!(
        "tau-dummy-{label}-{}-{}.sock",
        std::process::id(),
        rand::random::<u64>()
    ));
    DirBuilder::new()
        .mode(0o700)
        .create(&root)
        .expect("create private fixture root");
    root.join("release.sock")
}

fn send_release_frame(socket_path: &std::path::Path, frame: &[u8]) {
    let mut stream = UnixStream::connect(socket_path).expect("connect release socket");
    stream.write_all(frame).expect("write release frame");
}

fn cancel_restart(call_id: &str) -> HarnessOutputMessage {
    HarnessOutputMessage::deliver(Event::ToolCancelRequest(tau_proto::ToolCancelRequest {
        target_call_id: call_id.into(),
    }))
}

fn disconnect() -> HarnessOutputMessage {
    HarnessOutputMessage::Disconnect(tau_proto::Disconnect {
        reason: Some("fixture complete".to_owned()),
    })
}

fn run_restart_frames(
    input_frames: &[HarnessOutputMessage],
    seed: u64,
) -> Vec<HarnessInputMessage> {
    let input = restart_input(input_frames);
    let output = SharedWriter::default();
    let mut rng = StdRng::seed_from_u64(seed);
    run_with_rng(Cursor::new(input), output.clone(), &mut rng).expect("run");
    decode_output(output.snapshot())
}

fn restart_input(input_frames: &[HarnessOutputMessage]) -> Vec<u8> {
    let mut input = Vec::new();
    let mut writer = HarnessOutputWriter::new(&mut input);
    if !matches!(
        input_frames.first(),
        Some(HarnessOutputMessage::Configure(_))
    ) {
        writer
            .write_message(&HarnessOutputMessage::Configure(Configure {
                tool_prefix: None,
                instance_name: tau_proto::ExtensionName::parse("test-extension")
                    .expect("test extension name must satisfy the identifier grammar"),
                config: CborValue::Map(Vec::new()),
                state_dir: None,
                secrets: path_std_collections::BTreeMap::new(),
                settings_files: Default::default(),
            }))
            .expect("write initial configure");
    }
    for frame in input_frames {
        writer.write_message(frame).expect("write input frame");
    }
    writer.flush().expect("flush input");
    input
}

fn decode_output(output: Vec<u8>) -> Vec<HarnessInputMessage> {
    let mut reader = HarnessInputReader::new(Cursor::new(output));
    let mut frames = Vec::new();
    while let Some(frame) = reader.read_message().expect("read") {
        frames.push(frame);
    }
    frames
}

/// Reader that keeps protocol input open beyond one injected hold deadline.
struct DelayedReader {
    /// Complete prefix available immediately.
    prefix: Cursor<Vec<u8>>,
    /// Terminal protocol suffix released after the delay.
    suffix: Cursor<Vec<u8>>,
    /// Whether the one delay has elapsed.
    delayed: bool,
}

impl Read for DelayedReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let count = self.prefix.read(buffer)?;
        if count != 0 {
            return Ok(count);
        }
        if !self.delayed {
            std::thread::sleep(Duration::from_millis(100));
            self.delayed = true;
        }
        self.suffix.read(buffer)
    }
}

fn emitted_event(message: &HarnessInputMessage) -> Option<&Event> {
    match message {
        HarnessInputMessage::Emit(emit) => Some(emit.event.as_ref()),
        _ => None,
    }
}

fn fixture_extension_originator() -> PromptOriginator {
    PromptOriginator::Extension {
        name: tau_proto::ExtensionName::parse("fixture")
            .expect("test extension name must satisfy the identifier grammar"),
        query_id: "query-1".to_owned(),
    }
}

/// Verifies the historical random restart fixture can reply with a tool error.
#[test]
fn restart_tool_can_return_error() {
    let frames = run_restart_frames(&[invoke_restart()], 1);

    assert!(matches!(frames[0], HarnessInputMessage::Hello(_)));
    assert!(matches!(frames[1], HarnessInputMessage::Subscribe(_)));
    assert!(matches!(frames[2], HarnessInputMessage::Intercept(_)));
    let Some(Event::ToolRegistrationDeclared(declaration)) = emitted_event(&frames[3]) else {
        panic!("expected tool registration declaration");
    };
    assert_eq!(
        declaration
            .tool_group
            .as_ref()
            .map(|group| group.name.as_str()),
        Some("test")
    );
    assert!(matches!(frames[4], HarnessInputMessage::Ready(_)));
    let Some(Event::ToolErrorReported(error)) = frames.get(5).and_then(emitted_event) else {
        panic!("expected tool error");
    };
    assert_eq!(error.message, "restarting failed");
    assert_eq!(frames.len(), 6);
}

/// Verifies the historical random restart fixture can exit without replying.
#[test]
fn restart_tool_can_exit_without_reply() {
    let frames = run_restart_frames(&[invoke_restart()], 2);
    assert_eq!(frames.len(), 5);
    assert!(matches!(frames[0], HarnessInputMessage::Hello(_)));
    assert!(matches!(frames[1], HarnessInputMessage::Subscribe(_)));
    assert!(matches!(frames[2], HarnessInputMessage::Intercept(_)));
    let Some(Event::ToolRegistrationDeclared(declaration)) = emitted_event(&frames[3]) else {
        panic!("expected tool registration declaration");
    };
    assert_eq!(
        declaration
            .tool_group
            .as_ref()
            .map(|group| group.name.as_str()),
        Some("test")
    );
    // The random-exit branch must exit without emitting any
    // reply frame for the invoke — guard against a future bug that
    // re-introduces a stray ToolResult/ToolError before exit.
    assert!(
        frames.iter().all(|frame| !matches!(
            emitted_event(frame),
            Some(Event::ToolErrorReported(_)) | Some(Event::ToolResultReported(_))
        )),
        "no tool reply frame should appear in the random-exit branch"
    );
}

/// Verifies deterministic `success` configuration returns a final tool result.
#[test]
fn restart_tool_config_success_returns_tool_result() {
    // Harness restart tests need a deterministic happy path, not the
    // historical random exit-or-error behavior.
    let frames = run_restart_frames(&[restart_config("success"), invoke_restart()], 1);

    let result = frames
        .iter()
        .find_map(|frame| match emitted_event(frame) {
            Some(Event::ToolResultReported(result)) => Some(result),
            _ => None,
        })
        .expect("configured success should return a tool result");
    assert_eq!(result.call_id.as_str(), "call-1");
    assert_eq!(
        result.result,
        CborValue::Text("restart succeeded".to_owned())
    );
    assert_eq!(result.kind, tau_proto::ToolResultKind::Final);
    assert!(
        frames
            .iter()
            .all(|frame| !matches!(emitted_event(frame), Some(Event::ToolErrorReported(_))))
    );
}

/// Verifies deterministic `error` configuration overrides random exit.
#[test]
fn restart_tool_config_error_overrides_random_exit() {
    // Seed 2 hits the random exit branch; config must force a reply so
    // harness tests can exercise tool-error handling deterministically.
    let frames = run_restart_frames(&[restart_config("error"), invoke_restart()], 2);

    let error = frames
        .iter()
        .find_map(|frame| match emitted_event(frame) {
            Some(Event::ToolErrorReported(error)) => Some(error),
            _ => None,
        })
        .expect("configured error should return a tool error");
    assert_eq!(error.call_id.as_str(), "call-1");
    assert_eq!(error.message, "restarting failed");
    assert!(
        frames
            .iter()
            .all(|frame| !matches!(emitted_event(frame), Some(Event::ToolResultReported(_))))
    );
}

/// Verifies deterministic `exit` configuration overrides random error.
#[test]
fn restart_tool_config_exit_overrides_random_error() {
    // Seed 1 hits the random error branch; config must force the
    // extension-disconnect shape with no tool reply frame.
    let frames = run_restart_frames(&[restart_config("exit"), invoke_restart()], 1);

    assert_eq!(frames.len(), 5);
    assert!(frames.iter().all(|frame| !matches!(
        emitted_event(frame),
        Some(Event::ToolErrorReported(_)) | Some(Event::ToolResultReported(_))
    )));
}

/// Verifies invalid restart configuration is reported to the harness.
#[test]
fn invalid_restart_mode_emits_config_error() {
    let frames = run_restart_frames(&[restart_config("bogus")], 1);

    let error = frames
        .iter()
        .find_map(|frame| match frame {
            HarnessInputMessage::ConfigError(error) => Some(error),
            _ => None,
        })
        .expect("invalid config should emit ConfigError");
    assert!(
        error.message.contains("bogus") && error.message.contains("expected one of"),
        "error should describe invalid restart mode: {}",
        error.message
    );
}

/// Verifies release mode cannot become ready or bind a socket unless both
/// fixture-private configuration values are present and the nonce is nonempty.
#[test]
fn release_mode_requires_complete_configuration() {
    for config in [
        CborValue::Map(vec![(
            CborValue::Text("restart_mode".to_owned()),
            CborValue::Text("hold_until_success_release".to_owned()),
        )]),
        CborValue::Map(vec![
            (
                CborValue::Text("restart_mode".to_owned()),
                CborValue::Text("hold_until_success_release".to_owned()),
            ),
            (
                CborValue::Text("release_nonce".to_owned()),
                CborValue::Text("fixture-nonce".to_owned()),
            ),
        ]),
        CborValue::Map(vec![
            (
                CborValue::Text("restart_mode".to_owned()),
                CborValue::Text("hold_until_success_release".to_owned()),
            ),
            (
                CborValue::Text("release_socket_path".to_owned()),
                CborValue::Text("/unused/release.sock".to_owned()),
            ),
        ]),
        CborValue::Map(vec![
            (
                CborValue::Text("restart_mode".to_owned()),
                CborValue::Text("hold_until_success_release".to_owned()),
            ),
            (
                CborValue::Text("release_socket_path".to_owned()),
                CborValue::Text("/unused/release.sock".to_owned()),
            ),
            (
                CborValue::Text("release_nonce".to_owned()),
                CborValue::Text(String::new()),
            ),
        ]),
    ] {
        let frames = run_restart_frames(
            &[HarnessOutputMessage::Configure(Configure {
                tool_prefix: None,
                instance_name: tau_proto::ExtensionName::parse("test-extension")
                    .expect("extension name"),
                config,
                state_dir: None,
                secrets: path_std_collections::BTreeMap::new(),
                settings_files: Default::default(),
            })],
            1,
        );
        assert!(
            frames
                .iter()
                .any(|frame| matches!(frame, HarnessInputMessage::ConfigError(_)))
        );
        assert!(
            frames
                .iter()
                .all(|frame| !matches!(frame, HarnessInputMessage::Ready(_)))
        );
        assert!(!std::path::Path::new("/unused/release.sock").exists());
    }
}

/// Verifies the closed hold mode acknowledges readiness before a correlated
/// cancellation and joins its worker before protocol disconnect completes.
#[test]
fn hold_no_side_effect_reports_ready_then_cancels() {
    let frames = run_restart_frames(
        &[
            restart_config("hold_no_side_effect"),
            invoke_restart(),
            cancel_restart("call-1"),
            disconnect(),
        ],
        1,
    );

    let events = frames.iter().filter_map(emitted_event).collect::<Vec<_>>();
    assert!(matches!(
        events.as_slice(),
        [
            Event::ToolRegistrationDeclared(_),
            Event::ToolProgressReported(progress),
            Event::ToolCancelledReported(cancelled),
        ] if progress.call_id.as_str() == "call-1"
            && progress.tool_name.as_str() == RESTART_TEST_DUMMY_TOOL_NAME
            && progress.message.as_deref() == Some("hold_no_side_effect ready")
            && cancelled.call_id.as_str() == "call-1"
            && cancelled.tool_name.as_str() == RESTART_TEST_DUMMY_TOOL_NAME
    ));
}

/// Verifies disconnect wakes and joins the closed hold without fabricating a
/// tool terminal.
#[test]
fn hold_no_side_effect_disconnect_has_no_terminal_output() {
    let frames = run_restart_frames(
        &[
            restart_config("hold_no_side_effect"),
            invoke_restart(),
            disconnect(),
        ],
        1,
    );

    assert!(frames.iter().any(|frame| {
        matches!(
            emitted_event(frame),
            Some(Event::ToolProgressReported(progress))
                if progress.call_id.as_str() == "call-1"
                    && progress.message.as_deref() == Some("hold_no_side_effect ready")
        )
    }));
    assert!(frames.iter().all(|frame| !matches!(
        emitted_event(frame),
        Some(Event::ToolResultReported(_))
            | Some(Event::ToolErrorReported(_))
            | Some(Event::ToolCancelledReported(_))
    )));
}

/// Verifies a wrong-id cancellation cannot wake the sole hold and a concurrent
/// invocation is rejected before exact cancellation joins the original.
#[test]
fn hold_no_side_effect_rejects_wrong_cancel_and_concurrent_call() {
    let frames = run_restart_frames(
        &[
            restart_config("hold_no_side_effect"),
            invoke_restart(),
            cancel_restart("wrong-call"),
            invoke_restart_with_id("call-2"),
            cancel_restart("call-1"),
            disconnect(),
        ],
        1,
    );

    let errors = frames
        .iter()
        .filter_map(|frame| match emitted_event(frame) {
            Some(Event::ToolErrorReported(error)) => Some(error),
            _ => None,
        })
        .collect::<Vec<_>>();
    let cancellations = frames
        .iter()
        .filter_map(|frame| match emitted_event(frame) {
            Some(Event::ToolCancelledReported(cancelled)) => Some(cancelled),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].call_id.as_str(), "call-2");
    assert_eq!(
        errors[0].message,
        "hold_no_side_effect already has an active invocation"
    );
    assert_eq!(cancellations.len(), 1);
    assert_eq!(cancellations[0].call_id.as_str(), "call-1");
}

/// Verifies the bounded hold deadline emits one exact terminal error before
/// later disconnect joins the already completed worker.
#[test]
fn hold_no_side_effect_deadline_is_terminal() {
    let prefix = restart_input(&[restart_config("hold_no_side_effect"), invoke_restart()]);
    let suffix = restart_input(&[disconnect()]);
    let reader = DelayedReader {
        prefix: Cursor::new(prefix),
        suffix: Cursor::new(suffix),
        delayed: false,
    };
    let output = SharedWriter::default();
    let mut rng = StdRng::seed_from_u64(1);
    run_with_rng_and_hold_timeout(reader, output.clone(), &mut rng, Duration::from_millis(20))
        .expect("run");
    let frames = decode_output(output.snapshot());
    let errors = frames
        .iter()
        .filter_map(|frame| match emitted_event(frame) {
            Some(Event::ToolErrorReported(error)) => Some(error),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].call_id.as_str(), "call-1");
    assert_eq!(
        errors[0].message,
        "hold_no_side_effect reached its 10 second deadline"
    );
}

/// Runs one owned hold terminal after exhausting tau-client's production
/// detached FIFO, then returns all emitted protocol frames.
fn saturated_hold_terminal(
    configure: HarnessOutputMessage,
    timeout: Duration,
    action: impl FnOnce(&mut HarnessOutputWriter<UnixStream>) + Send,
) -> Vec<HarnessInputMessage> {
    let _serial = SATURATION_TEST_LOCK.lock().expect("saturation test lock");
    let (extension_input, harness_input) = UnixStream::pair().expect("input pair");
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let gate = Arc::new((Mutex::new(true), Condvar::new()));
    let (entered_tx, entered_rx) = mpsc::channel();
    let (overloaded_tx, overloaded_rx) = mpsc::channel();
    *SATURATION_HOOK.lock().expect("saturation hook") =
        Some(("saturated-call".into(), overloaded_tx));
    let hook = SaturationHookGuard;
    let output_bytes = Arc::clone(&bytes);
    let output_gate = Arc::clone(&gate);
    let runner = thread::spawn(move || {
        let mut rng = StdRng::seed_from_u64(1);
        run_with_rng_and_hold_timeout(
            extension_input,
            SaturationWriter {
                bytes: output_bytes,
                gate: output_gate,
                entered: entered_tx,
                blocked: false,
            },
            &mut rng,
            timeout,
        )
        .map_err(|error| error.to_string())
    });
    let mut input = HarnessOutputWriter::new(harness_input);
    input.write_message(&configure).expect("configure");
    input
        .write_message(&invoke_restart_with_id("saturated-call"))
        .expect("start deterministic hold");
    input.flush().expect("flush hold start");
    action(&mut input);
    entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("production writer blocked");
    let ownership_retained = overloaded_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("detached FIFO exhausted");
    assert!(ownership_retained, "hold ownership released before flush");
    drop(hook);
    let (closed, wake) = &*gate;
    *closed.lock().expect("writer gate") = false;
    wake.notify_all();
    input.write_message(&disconnect()).expect("disconnect");
    input.flush().expect("flush disconnect");
    runner.join().expect("runner").expect("clean disconnect");
    decode_output(bytes.lock().expect("output bytes").clone())
}

/// Proves cancellation and deadline terminals retain hold ownership and survive
/// saturation of the real detached output FIFO exactly once.
#[test]
fn no_side_effect_terminals_survive_production_fifo_saturation() {
    let cancelled = saturated_hold_terminal(
        restart_config("hold_no_side_effect"),
        HOLD_TERMINAL_TIMEOUT,
        |input| {
            input
                .write_message(&cancel_restart("saturated-call"))
                .expect("cancel hold");
            input.flush().expect("flush cancellation");
        },
    );
    let deadline = saturated_hold_terminal(
        restart_config("hold_no_side_effect"),
        Duration::from_millis(20),
        |_| {},
    );
    for (frames, expect_cancelled) in [(cancelled, true), (deadline, false)] {
        let terminals = frames
            .iter()
            .filter_map(emitted_event)
            .filter(|event| {
                matches!(event, Event::ToolCancelledReported(cancelled)
                    if cancelled.call_id.as_str() == "saturated-call")
                    || matches!(event, Event::ToolErrorReported(error)
                        if error.call_id.as_str() == "saturated-call")
                    || matches!(event, Event::ToolResultReported(result)
                        if result.call_id.as_str() == "saturated-call")
            })
            .collect::<Vec<_>>();
        assert_eq!(terminals.len(), 1);
        assert_eq!(
            matches!(terminals[0], Event::ToolCancelledReported(_)),
            expect_cancelled
        );
        match terminals[0] {
            Event::ToolCancelledReported(cancelled) => {
                assert_eq!(cancelled.tool_name.as_str(), RESTART_TEST_DUMMY_TOOL_NAME);
            }
            Event::ToolErrorReported(error) => {
                assert_eq!(
                    error.message,
                    "hold_no_side_effect reached its 10 second deadline"
                );
                assert_eq!(error.tool_name.as_str(), RESTART_TEST_DUMMY_TOOL_NAME);
            }
            _ => panic!("unexpected correlated terminal"),
        }
    }
}

/// Proves authenticated release success survives production FIFO saturation
/// without giving up the active hold before checked publication.
#[test]
fn release_success_survives_production_fifo_saturation() {
    let socket = unique_socket_path("output-saturation");
    let socket_for_release = socket.clone();
    let frames = saturated_hold_terminal(
        release_config(&socket, "nonce"),
        HOLD_TERMINAL_TIMEOUT,
        move |_| {
            for _ in 0..100 {
                if UnixStream::connect(&socket_for_release).is_ok() {
                    send_release_frame(
                        &socket_for_release,
                        b"{\"call_id\":\"saturated-call\",\"release_nonce\":\"nonce\"}\n",
                    );
                    return;
                }
                thread::sleep(Duration::from_millis(5));
            }
            panic!("release socket did not become ready");
        },
    );
    let terminals = frames
        .iter()
        .filter_map(emitted_event)
        .filter(|event| {
            matches!(event, Event::ToolResultReported(result)
                if result.call_id.as_str() == "saturated-call")
                || matches!(event, Event::ToolErrorReported(error)
                    if error.call_id.as_str() == "saturated-call")
                || matches!(event, Event::ToolCancelledReported(cancelled)
                    if cancelled.call_id.as_str() == "saturated-call")
        })
        .collect::<Vec<_>>();
    assert_eq!(terminals.len(), 1);
    let Event::ToolResultReported(result) = terminals[0] else {
        panic!("expected sole correlated release result");
    };
    assert_eq!(
        result.result,
        CborValue::Text("restart succeeded".to_owned())
    );
    assert_eq!(result.call_id.as_str(), "saturated-call");
    assert_eq!(result.tool_name.as_str(), RESTART_TEST_DUMMY_TOOL_NAME);
}

/// Proves a mandatory hold terminal write failure tears down the extension loop
/// and does not falsely publish or retire the retained invocation.
#[test]
fn mandatory_hold_terminal_failure_exits_extension_loop() {
    let (extension_input, harness_input) = UnixStream::pair().expect("input pair");
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let output_bytes = Arc::clone(&bytes);
    let runner = thread::spawn(move || {
        let mut rng = StdRng::seed_from_u64(1);
        run_with_rng_and_hold_timeout(
            extension_input,
            TerminalFailureWriter {
                bytes: output_bytes,
                failed: false,
            },
            &mut rng,
            Duration::from_millis(20),
        )
        .map_err(|error| error.to_string())
    });
    let mut input = HarnessOutputWriter::new(harness_input);
    input
        .write_message(&restart_config("hold_no_side_effect"))
        .expect("configure");
    input.write_message(&invoke_restart()).expect("invoke");
    input.flush().expect("flush invocation");
    assert!(runner.join().expect("runner").is_err());
    let output = bytes.lock().expect("output bytes");
    assert!(
        !output
            .windows(19)
            .any(|window| window == b"tool.error_reported")
    );
}

/// Verifies an early client cannot release before readiness publication and
/// malformed, mismatched, and boundary-sized frames cannot release afterward.
#[test]
fn hold_until_success_release_requires_exact_typed_frame() {
    let socket_path = unique_socket_path("exact");
    let prefix = restart_input(&[
        release_config(&socket_path, "fixture-nonce"),
        invoke_restart(),
    ]);
    let suffix = restart_input(&[disconnect()]);
    let reader = DelayedReader {
        prefix: Cursor::new(prefix),
        suffix: Cursor::new(suffix),
        delayed: false,
    };
    let output = SharedWriter::default();
    let output_bytes = Arc::clone(&output.bytes);
    let client_path = socket_path.clone();
    let client = std::thread::spawn(move || {
        for _ in 0..100 {
            if client_path.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        send_release_frame(&client_path, b"{not-json}\n");
        send_release_frame(
            &client_path,
            b"{\"call_id\":\"call-1\",\"release_nonce\":\"wrong\"}\n",
        );
        send_release_frame(
            &client_path,
            b"{\"call_id\":\"wrong\",\"release_nonce\":\"fixture-nonce\"}\n",
        );
        send_release_frame(
            &client_path,
            b"{\"call_id\":\"call-1\",\"release_nonce\":\"fixture-nonce\",\"extra\":1}\n",
        );
        const RELEASE_FRAME_LIMIT: usize = 4096;
        for size in [RELEASE_FRAME_LIMIT, RELEASE_FRAME_LIMIT + 1] {
            let prefix = b"{\"call_id\":\"call-1\",\"release_nonce\":\"".len();
            let suffix = b"\"}\n".len();
            let nonce = "x".repeat(size - prefix - suffix);
            let frame = format!("{{\"call_id\":\"call-1\",\"release_nonce\":\"{nonce}\"}}\n");
            assert_eq!(frame.len(), size);
            send_release_frame(&client_path, frame.as_bytes());
        }
        send_release_frame(
            &client_path,
            b"{\"call_id\":\"call-1\",\"release_nonce\":\"fixture-nonce\"}\n",
        );
    });
    let mut rng = StdRng::seed_from_u64(1);
    run_with_rng(reader, output, &mut rng).expect("run release fixture");
    client.join().expect("release client");
    let frames = decode_output(output_bytes.lock().expect("output lock").clone());
    let results = frames
        .iter()
        .filter_map(|frame| match emitted_event(frame) {
            Some(Event::ToolResultReported(result)) => Some(result),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].call_id.as_str(), "call-1");
    assert_eq!(
        results[0].result,
        CborValue::Text("restart succeeded".to_owned())
    );
    let progress_index = frames
        .iter()
        .position(|frame| matches!(emitted_event(frame), Some(Event::ToolProgressReported(_))))
        .expect("readiness progress");
    let result_index = frames
        .iter()
        .position(|frame| matches!(emitted_event(frame), Some(Event::ToolResultReported(_))))
        .expect("success result");
    assert!(progress_index < result_index);
    assert!(!socket_path.exists());
    std::fs::remove_dir(socket_path.parent().expect("fixture root")).expect("remove fixture root");
}

/// Verifies bounded connection saturation cannot prevent cancellation, leak the
/// socket, or synthesize success from partial release frames.
#[test]
fn release_hold_saturation_cancels_and_cleans_up_without_success() {
    let socket_path = unique_socket_path("cancel-partial");
    let prefix = restart_input(&[
        release_config(&socket_path, "fixture-nonce"),
        invoke_restart(),
    ]);
    let suffix = restart_input(&[cancel_restart("call-1"), disconnect()]);
    let reader = DelayedReader {
        prefix: Cursor::new(prefix),
        suffix: Cursor::new(suffix),
        delayed: false,
    };
    let client_path = socket_path.clone();
    let client = std::thread::spawn(move || {
        for _ in 0..100 {
            if client_path.exists() {
                let mut streams = Vec::new();
                for _ in 0..32 {
                    if let Ok(mut stream) = UnixStream::connect(&client_path) {
                        let _ = stream.write_all(b"{\"call_id\":");
                        streams.push(stream);
                    }
                }
                std::thread::sleep(Duration::from_millis(300));
                drop(streams);
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("release socket never became ready");
    });
    let output = SharedWriter::default();
    let mut rng = StdRng::seed_from_u64(1);
    run_with_rng(reader, output.clone(), &mut rng).expect("run cancellation fixture");
    client.join().expect("partial release client");
    let frames = decode_output(output.snapshot());
    assert!(frames.iter().any(|frame| matches!(
        emitted_event(frame),
        Some(Event::ToolCancelledReported(cancelled)) if cancelled.call_id.as_str() == "call-1"
    )));
    assert!(
        frames
            .iter()
            .all(|frame| !matches!(emitted_event(frame), Some(Event::ToolResultReported(_))))
    );
    assert!(!socket_path.exists());
    std::fs::remove_dir(socket_path.parent().expect("fixture root")).expect("remove fixture root");
}

/// Verifies disconnect joins the release worker, removes its socket, and does
/// not synthesize success when no exact release arrived.
#[test]
fn hold_until_success_release_disconnect_cleans_up_without_success() {
    let socket_path = unique_socket_path("disconnect");
    let frames = run_restart_frames(
        &[
            release_config(&socket_path, "fixture-nonce"),
            invoke_restart(),
            disconnect(),
        ],
        1,
    );
    assert!(
        frames
            .iter()
            .all(|frame| !matches!(emitted_event(frame), Some(Event::ToolResultReported(_))))
    );
    assert!(!socket_path.exists());
    std::fs::remove_dir(socket_path.parent().expect("fixture root")).expect("remove fixture root");
}

/// Verifies deterministic success replies preserve non-user invocation origin.
#[test]
fn restart_tool_result_preserves_originator() {
    let frames = run_restart_frames(
        &[restart_config("success"), extension_originated_restart()],
        1,
    );

    let result = frames
        .iter()
        .find_map(|frame| match emitted_event(frame) {
            Some(Event::ToolResultReported(result)) => Some(result),
            _ => None,
        })
        .expect("configured success should return a tool result");
    assert_eq!(result.call_id.as_str(), "extension-call");
    assert_eq!(result.originator, fixture_extension_originator());
}

/// Verifies deterministic error replies preserve non-user invocation origin.
#[test]
fn restart_tool_error_preserves_originator() {
    let frames = run_restart_frames(
        &[restart_config("error"), extension_originated_restart()],
        1,
    );

    let error = frames
        .iter()
        .find_map(|frame| match emitted_event(frame) {
            Some(Event::ToolErrorReported(error)) => Some(error),
            _ => None,
        })
        .expect("configured error should return a tool error");
    assert_eq!(error.call_id.as_str(), "extension-call");
    assert_eq!(error.originator, fixture_extension_originator());
}

/// Verifies replayed tool-start events do not re-run side-effecting tool logic.
#[test]
fn replayed_restart_tool_is_ignored() {
    let frames = run_restart_frames(&[restart_config("success"), replayed_restart()], 1);

    assert!(frames.iter().all(|frame| !matches!(
        emitted_event(frame),
        Some(Event::ToolErrorReported(_)) | Some(Event::ToolResultReported(_))
    )));
    assert_eq!(frames.len(), 5);
}

/// Verifies replayed exit-mode restart events do not terminate the extension.
#[test]
fn replayed_exit_restart_does_not_prevent_later_live_tool_result() {
    let frames = run_restart_frames(
        &[
            restart_config("exit"),
            replayed_restart(),
            restart_config("success"),
            invoke_restart(),
        ],
        1,
    );

    let results = frames
        .iter()
        .filter_map(|frame| match emitted_event(frame) {
            Some(Event::ToolResultReported(result)) => Some(result),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 1, "only the later live invoke should reply");
    assert_eq!(results[0].call_id.as_str(), "call-1");
}

fn intercepted_prompt(prompt: AgentPromptSubmitted) -> HarnessOutputMessage {
    HarnessOutputMessage::InterceptRequest(InterceptRequest {
        event: Box::new(Event::AgentPromptSubmitted(prompt)),
        persist: true,
    })
}

fn test_prompt(text: &str) -> AgentPromptSubmitted {
    AgentPromptSubmitted {
        inference_activation: false,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        text: text.to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: PromptMessageClass::User,
        internal_kind: None,
        originator: PromptOriginator::User,
        submission_source: Default::default(),
        display_name: None,
        ctx_id: None,
    }
}

fn run_intercept(
    prompt: AgentPromptSubmitted,
) -> (Vec<tau_proto::ExtensionNoticeRequest>, Vec<InterceptReply>) {
    let mut input = Vec::new();
    let mut writer = HarnessOutputWriter::new(&mut input);
    writer
        .write_message(&HarnessOutputMessage::Configure(Configure {
            tool_prefix: None,
            instance_name: tau_proto::ExtensionName::parse("test-extension")
                .expect("test extension name must satisfy the identifier grammar"),
            config: CborValue::Map(Vec::new()),
            state_dir: None,
            secrets: path_std_collections::BTreeMap::new(),
            settings_files: Default::default(),
        }))
        .expect("write initial configure");
    writer
        .write_message(&intercepted_prompt(prompt))
        .expect("write intercepted prompt");
    writer.flush().expect("flush");

    let output = SharedWriter::default();
    let mut rng = StdRng::seed_from_u64(1);
    run_with_rng(Cursor::new(input), output.clone(), &mut rng).expect("run");

    let mut reader = HarnessInputReader::new(Cursor::new(output.snapshot()));
    let mut notice_requests = Vec::new();
    let mut replies = Vec::new();
    while let Some(frame) = reader.read_message().expect("read") {
        match frame {
            HarnessInputMessage::ExtensionNoticeRequest(request) => {
                notice_requests.push(request);
            }
            HarnessInputMessage::InterceptReply(reply) => replies.push(reply),
            _ => {}
        }
    }
    (notice_requests, replies)
}
fn replaced_prompt_text(reply: &InterceptReply) -> Option<String> {
    match &reply.action {
        tau_proto::InterceptAction::Pass(Some(boxed)) => match boxed.as_ref() {
            Event::AgentPromptSubmitted(p) => Some(p.text.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// Verifies corrected prompts request a routine user-facing notice.
#[test]
fn prompt_with_tao_is_corrected_with_notice() {
    let (requests, replies) = run_intercept(test_prompt("I love Tao"));

    assert_eq!(
        requests.len(),
        1,
        "exactly one notice request on correction"
    );
    assert_eq!(requests[0].level, tau_proto::NoticeLevel::Info);
    assert!(requests[0].message.contains("Tau"));
    assert!(requests[0].message.contains("corrected"));

    assert_eq!(replies.len(), 1);
    let replaced =
        replaced_prompt_text(&replies[0]).expect("intercept reply carries replacement event");
    assert_eq!(replaced, "I love Tau");
}

/// Verifies the ASCII replacement preserves the case of the original letters.
#[test]
fn prompt_correction_preserves_letter_case() {
    for (input, expected) in [
        ("tao", "tau"),
        ("Tao", "Tau"),
        ("TAO", "TAU"),
        ("tAo", "tAu"),
        ("TaO", "TaU"),
        ("the TAO of Tao and tao", "the TAU of Tau and tau"),
    ] {
        let (_, replies) = run_intercept(test_prompt(input));
        let replaced = replaced_prompt_text(&replies[0]).unwrap_or_else(|| {
            panic!("expected replacement for input {input:?}");
        });
        assert_eq!(replaced, expected, "case preservation for {input:?}");
    }
}

/// Verifies prompt interception preserves non-text identity and routing fields.
#[test]
fn prompt_correction_preserves_prompt_identity_fields() {
    let mut prompt = test_prompt("internal Tao");
    prompt.message_class = PromptMessageClass::Internal;
    prompt.originator = PromptOriginator::Extension {
        name: tau_proto::ExtensionName::parse("fixture")
            .expect("test extension name must satisfy the identifier grammar"),
        query_id: "query-1".to_owned(),
    };
    prompt.display_name = Some("fixture".to_owned());
    prompt.ctx_id = Some("ctx-1".into());
    let original_agent_id = prompt.agent_id.clone();

    let (_, replies) = run_intercept(prompt);

    let tau_proto::InterceptAction::Pass(Some(event)) = &replies[0].action else {
        panic!("expected replacement prompt");
    };
    let Event::AgentPromptSubmitted(replaced) = event.as_ref() else {
        panic!("expected AgentPromptSubmitted replacement");
    };
    assert_eq!(replaced.text, "internal Tau");
    assert_eq!(replaced.agent_id, original_agent_id);
    assert_eq!(replaced.message_class, PromptMessageClass::Internal);
    assert_eq!(
        replaced.originator,
        PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("fixture")
                .expect("test extension name must satisfy the identifier grammar"),
            query_id: "query-1".to_owned(),
        }
    );
    assert_eq!(replaced.display_name.as_deref(), Some("fixture"));
    assert_eq!(replaced.ctx_id.as_deref(), Some("ctx-1"));
}

/// Verifies the fixture does not rewrite `tao` embedded within ASCII words.
#[test]
fn prompt_correction_skips_substrings_inside_words() {
    // `tao` inside `chaotic` is just three letters, not the word —
    // don't touch it.
    let (emits, replies) = run_intercept(test_prompt("a chaotic taoism enjoyer"));

    assert_eq!(emits.len(), 0, "no notice when no whole-word match");
    assert_eq!(replies.len(), 1);
    assert!(
        matches!(&replies[0].action, tau_proto::InterceptAction::Pass(None)),
        "no replacement when no whole-word match"
    );
}

/// Verifies prompts without a matching word are passed through unchanged.
#[test]
fn prompt_without_tao_passes_through_unchanged() {
    let (emits, replies) = run_intercept(test_prompt("hello world"));

    assert_eq!(emits.len(), 0);
    assert_eq!(replies.len(), 1);
    assert!(matches!(
        &replies[0].action,
        tau_proto::InterceptAction::Pass(None)
    ));
}
