use std::collections::BTreeMap;
use std::io::{self, Cursor, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process as path_std_process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use tau_proto::{
    CborValue, Configure, Event, EventSelector, HarnessInputMessage, HarnessInputReader,
    HarnessNotice, HarnessOutputMessage, HarnessOutputWriter, InterceptAction, InterceptRequest,
    InterceptionPriority, UnixMicros,
};

use super::*;

/// Serializes fixtures that observe the test-only detached-overload latch.
static SATURATION_FIXTURE_LOCK: Mutex<()> = Mutex::new(());

/// Ensures arbitrary script-authored text is absent from the normal Rhai
/// baseline and appears only when the private target is explicitly enabled.
#[test]
fn script_logging_requires_the_private_debug_target() {
    fn capture(filter: &str) -> String {
        let writer = SharedWriter::default();
        let captured = writer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::dispatcher::with_default(&tracing::Dispatch::new(subscriber), || {
            log_script_message("info", "private-script-canary");
        });
        String::from_utf8(captured.bytes()).expect("UTF-8 tracing output")
    }

    assert!(!capture("rhai=info,warn").contains("private-script-canary"));
    assert!(capture("rhai-script-private=debug,warn").contains("private-script-canary"));
}

#[derive(Clone, Default)]
struct SharedWriter(Arc<Mutex<Vec<u8>>>);

impl SharedWriter {
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().expect("writer mutex").clone()
    }

    fn into_bytes(self) -> Vec<u8> {
        Arc::try_unwrap(self.0)
            .expect("single writer reference")
            .into_inner()
            .expect("writer mutex")
    }
}

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("writer mutex").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Output sink that blocks the first generic bell emission so detached script
/// output can exhaust tau-client's production FIFO.
struct SaturationWriter {
    /// Bytes accepted after the writer gate opens.
    bytes: Arc<Mutex<Vec<u8>>>,
    /// Gate held while the extension fills detached output.
    gate: Arc<(Mutex<bool>, Condvar)>,
    /// Announces that the first optional frame reached the writer.
    entered: mpsc::Sender<()>,
    /// Prevents repeated gate announcements.
    announced: bool,
}

impl Write for SaturationWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if !self.announced && buf.windows(9).any(|window| window == b"term.bell") {
            self.announced = true;
            self.entered.send(()).expect("announce blocked writer");
            let (lock, condvar) = &*self.gate;
            let mut blocked = lock.lock().expect("writer gate");
            while *blocked {
                blocked = condvar.wait(blocked).expect("wait writer gate");
            }
        }
        self.bytes
            .lock()
            .expect("output bytes")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Writer that fails when a mandatory intercept reply or terminal report is
/// flushed.
struct MandatoryFlushFailureWriter {
    /// True after startup Ready has passed.
    ready_seen: bool,
    /// True when the forced failure was reached.
    failed: Arc<AtomicBool>,
}

impl Write for MandatoryFlushFailureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.windows(5).any(|window| window == b"ready") {
            self.ready_seen = true;
        }
        if self.ready_seen
            && (buf.windows(15).any(|window| window == b"intercept_reply")
                || buf
                    .windows(20)
                    .any(|window| window == b"tool.result_reported")
                || buf
                    .windows(19)
                    .any(|window| window == b"tool.error_reported"))
        {
            self.failed.store(true, Ordering::Release);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.failed.load(Ordering::Acquire) {
            return Err(io::Error::other("forced mandatory output failure"));
        }
        Ok(())
    }
}

fn write_script(dir: &tempfile::TempDir, source: &str) -> std::path::PathBuf {
    let path = dir.path().join("hook.rhai");
    std::fs::write(&path, source).expect("write script");
    path
}

fn configure_with_script(path: &Path) -> HarnessOutputMessage {
    HarnessOutputMessage::Configure(Configure {
        tool_prefix: None,
        instance_name: tau_proto::ExtensionName::parse("test-extension")
            .expect("test extension name must satisfy the identifier grammar"),
        config: CborValue::Map(vec![(
            CborValue::Text("script".to_owned()),
            CborValue::Text(path.display().to_string()),
        )]),
        state_dir: None,
        secrets: BTreeMap::new(),
        settings_files: Default::default(),
    })
}

fn empty_configure() -> HarnessOutputMessage {
    HarnessOutputMessage::Configure(Configure {
        tool_prefix: None,
        instance_name: tau_proto::ExtensionName::parse("test-extension")
            .expect("test extension name must satisfy the identifier grammar"),
        config: CborValue::Map(Vec::new()),
        state_dir: None,
        secrets: BTreeMap::new(),
        settings_files: Default::default(),
    })
}

fn configure_with_script_and_extra(
    path: &Path,
    mut extra: Vec<(CborValue, CborValue)>,
) -> HarnessOutputMessage {
    let mut config = vec![(
        CborValue::Text("script".to_owned()),
        CborValue::Text(path.display().to_string()),
    )];
    config.append(&mut extra);
    HarnessOutputMessage::Configure(Configure {
        tool_prefix: None,
        instance_name: tau_proto::ExtensionName::parse("test-extension")
            .expect("test extension name must satisfy the identifier grammar"),
        config: CborValue::Map(config),
        state_dir: None,
        secrets: BTreeMap::new(),
        settings_files: Default::default(),
    })
}

fn prompt_event(text: &str) -> Event {
    Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
        inference_activation: false,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        text: text.to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: Default::default(),
        display_name: None,
        ctx_id: None,
    })
}

fn tool_started(tool_name: &str, args: CborValue) -> Event {
    Event::ToolStarted(tau_proto::ToolStarted {
        call_id: tau_proto::ToolCallId::new("call_1"),
        tool_name: tau_proto::ToolName::new(tool_name),
        arguments: args,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        originator: tau_proto::PromptOriginator::User,
    })
}
fn run_frames(input_frames: &[HarnessOutputMessage]) -> Vec<HarnessInputMessage> {
    let mut input = Vec::new();
    let mut writer = HarnessOutputWriter::new(&mut input);
    for frame in input_frames {
        writer.write_message(frame).expect("write input frame");
    }
    writer.flush().expect("flush input");

    let output = SharedWriter::default();
    run(Cursor::new(input), output.clone()).expect("run rhai extension");

    let mut reader = HarnessInputReader::new(Cursor::new(output.into_bytes()));
    let mut frames = Vec::new();
    while let Some(frame) = reader.read_message().expect("read output frame") {
        frames.push(frame);
    }
    frames
}

fn run_saturation_fixture(input_frames: &[HarnessOutputMessage]) -> Vec<HarnessInputMessage> {
    let _fixture_guard = SATURATION_FIXTURE_LOCK
        .lock()
        .expect("saturation fixture lock");
    DETACHED_OUTPUT_OVERLOADED.store(false, Ordering::Release);
    let (overloaded_tx, overloaded_rx) = mpsc::channel();
    *DETACHED_OUTPUT_OVERLOAD_NOTIFY
        .lock()
        .expect("detached overload notification") = Some(overloaded_tx);
    let mut input = Vec::new();
    let mut input_writer = HarnessOutputWriter::new(&mut input);
    for frame in input_frames {
        input_writer
            .write_message(frame)
            .expect("write saturation input");
    }
    input_writer.flush().expect("flush saturation input");

    let bytes = Arc::new(Mutex::new(Vec::new()));
    let gate = Arc::new((Mutex::new(true), Condvar::new()));
    let (entered_tx, entered_rx) = mpsc::channel();
    let writer = SaturationWriter {
        bytes: Arc::clone(&bytes),
        gate: Arc::clone(&gate),
        entered: entered_tx,
        announced: false,
    };
    let runner = std::thread::spawn(move || {
        run(Cursor::new(input), writer).map_err(|error| error.to_string())
    });
    entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("optional script output reached blocked writer");
    overloaded_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("script exhausted detached output");
    assert!(DETACHED_OUTPUT_OVERLOADED.load(Ordering::Acquire));
    DETACHED_OUTPUT_OVERLOAD_NOTIFY
        .lock()
        .expect("detached overload notification")
        .take();
    let (lock, condvar) = &*gate;
    *lock.lock().expect("writer gate") = false;
    condvar.notify_all();
    runner
        .join()
        .expect("saturation runner")
        .expect("mandatory output survived saturation");

    frames_from_bytes_lossy(bytes.lock().expect("output bytes").clone())
}

fn frames_from_bytes_lossy(bytes: Vec<u8>) -> Vec<HarnessInputMessage> {
    let mut reader = HarnessInputReader::new(Cursor::new(bytes));
    let mut frames = Vec::new();
    while let Ok(Some(frame)) = reader.read_message() {
        frames.push(frame);
    }
    frames
}

fn emitted_event(message: &HarnessInputMessage) -> Option<&Event> {
    match message {
        HarnessInputMessage::Emit(emit) => Some(emit.event.as_ref()),
        _ => None,
    }
}

fn emitted_transient(message: &HarnessInputMessage) -> Option<bool> {
    match message {
        HarnessInputMessage::Emit(emit) => Some(!emit.persist),
        _ => None,
    }
}

fn requested_notice(message: &HarnessInputMessage) -> Option<&tau_proto::ExtensionNoticeRequest> {
    match message {
        HarnessInputMessage::ExtensionNoticeRequest(request) => Some(request),
        _ => None,
    }
}

fn tool_result_output(frames: &[HarnessInputMessage]) -> &str {
    for frame in frames {
        let Some(Event::ToolResultReported(result)) = emitted_event(frame) else {
            continue;
        };
        let CborValue::Map(fields) = &result.result else {
            continue;
        };
        for (key, value) in fields {
            if let (CborValue::Text(key), CborValue::Text(output)) = (key, value)
                && key == "output"
            {
                return output;
            }
        }
    }
    panic!("tool result output");
}

fn tool_result_has_output(frame: &HarnessInputMessage, expected: &str) -> bool {
    let Some(Event::ToolResultReported(result)) = emitted_event(frame) else {
        return false;
    };
    let CborValue::Map(fields) = &result.result else {
        return false;
    };
    fields.iter().any(|(key, value)| {
        matches!(
            (key, value),
            (CborValue::Text(key), CborValue::Text(output))
                if key == "output" && output == expected
        )
    })
}

fn setsid_available() -> bool {
    path_std_process::Command::new("sh")
        .arg("-c")
        .arg("command -v setsid >/dev/null")
        .status()
        .is_ok_and(|status| status.success())
}

// Owns cleanup of a fixture process that deliberately escaped its parent group.
struct DetachedProcessCleanup {
    // File where the fixture publishes its session-leader PID.
    pid_file: PathBuf,
    // PID recorded before normal fixture cleanup begins.
    pid: Option<i32>,
    // Whether this fixture's process group has already disappeared.
    cleaned: bool,
}

impl DetachedProcessCleanup {
    // Creates cleanup that also covers a panic before the fixture publishes its
    // PID.
    fn new(pid_file: PathBuf) -> Self {
        Self {
            pid_file,
            pid: None,
            cleaned: false,
        }
    }

    // Waits for the fixture to publish the escaped process PID.
    fn wait_for_pid(&mut self) -> i32 {
        self.published_pid(Duration::from_secs(1))
            .unwrap_or_else(|| {
                panic!(
                    "timed out waiting for detached fixture PID at {}",
                    self.pid_file.display()
                )
            })
    }

    // Reads a published PID before a bounded deadline.
    fn published_pid(&mut self, timeout: Duration) -> Option<i32> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(pid) = std::fs::read_to_string(&self.pid_file)
                && let Ok(pid) = pid.trim().parse()
            {
                self.pid = Some(pid);
                return Some(pid);
            }
            if deadline <= Instant::now() {
                return None;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    // Stops the escaped process group and waits for its session leader to
    // disappear.
    fn cleanup(&mut self) -> bool {
        if self.cleaned {
            return true;
        }
        let Some(pid) = self
            .pid
            .or_else(|| self.published_pid(Duration::from_secs(1)))
        else {
            return false;
        };
        #[allow(unsafe_code)]
        // SAFETY: the fixture records its `setsid` session leader PID, so `-pid`
        // names only the process group deliberately created by this test.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
        self.cleaned = process_disappears(pid, Duration::from_secs(1));
        self.cleaned
    }

    // Disarms cleanup after the test has independently confirmed process exit.
    fn mark_cleaned(&mut self) {
        self.cleaned = true;
    }
}

impl Drop for DetachedProcessCleanup {
    fn drop(&mut self) {
        if !self.cleaned {
            let _ = self.cleanup();
        }
    }
}

// Returns once a fixture creates a synchronization file, without a fixed delay.
fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for fixture file {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

// Returns whether a process no longer exists after a bounded polling interval.
fn process_disappears(pid: i32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        #[allow(unsafe_code)]
        // SAFETY: `kill(pid, 0)` only probes the PID recorded by this test fixture.
        let exists = unsafe { libc::kill(pid, 0) } == 0
            || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH);
        if !exists {
            return true;
        }
        if deadline <= Instant::now() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn no_configure_exits_after_hello_only() {
    // Rhai uses tau-client deferred startup: before the first Configure, it must
    // send only Hello and must not leak script-dependent startup declarations or
    // inert Ready frames.
    let frames = run_frames(&[]);

    assert!(matches!(frames[0], HarnessInputMessage::Hello(_)));
    assert_eq!(frames.len(), 1);
}

#[test]
fn bootstrap_waits_for_configure_then_uses_init_plan() {
    // The Rhai extension must not send subscriptions until it has the
    // configured script, because the script decides its own event interest.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) {
                return #{
                    subscribe: [#{ kind: "exact", value: "agent.prompt_submitted" }],
                    ready_message: "demo ready",
                };
            }
        "#,
    );

    let frames = run_frames(&[configure_with_script(&script)]);

    assert!(matches!(frames[0], HarnessInputMessage::Hello(_)));
    assert!(matches!(frames[1], HarnessInputMessage::Subscribe(_)));
    assert!(matches!(frames[2], HarnessInputMessage::Ready(_)));
    let HarnessInputMessage::Ready(ready) = &frames[2] else {
        panic!("expected ready");
    };
    assert_eq!(ready.message.as_deref(), Some("demo ready"));
    assert_eq!(frames.len(), 3);
}

#[test]
fn no_op_init_uses_default_ready_message() {
    // A script can define init for future use without returning a map;
    // unit means the same as an absent init hook.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(&dir, "fn init(config) {}\n");

    let frames = run_frames(&[configure_with_script(&script)]);

    assert!(matches!(frames[0], HarnessInputMessage::Hello(_)));
    let ready = frames
        .iter()
        .find_map(|frame| match frame {
            HarnessInputMessage::Ready(ready) => Some(ready),
            _ => None,
        })
        .expect("ready frame");
    assert_eq!(ready.message.as_deref(), Some("rhai ready"));
    assert!(
        frames
            .iter()
            .all(|frame| !matches!(frame, HarnessInputMessage::ConfigError(_)))
    );
}

#[test]
fn init_host_emit_failure_is_inert() {
    // Host emit helpers are intentionally unavailable during init so a
    // script that fails init cannot leak pre-Ready side effects.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) {
                tau_info("should not leak");
                fail;
            }
        "#,
    );

    let frames = run_frames(&[configure_with_script(&script)]);

    assert!(
        frames
            .iter()
            .any(|frame| matches!(frame, HarnessInputMessage::ConfigError(_)))
    );
    assert!(frames.iter().all(|frame| !matches!(
        requested_notice(frame),
        Some(request) if request.message.contains("should not leak")
    )));
}

#[test]
fn shell_spawn_is_unavailable_during_init_and_has_no_side_effect() {
    // Init must remain an inert staging phase: a script that tries to spawn a
    // trusted shell command during init gets ConfigError and cannot leak host
    // side effects before failing configuration.
    let dir = tempfile::tempdir().expect("tempdir");
    let marker = dir.path().join("marker");
    let script = write_script(
        &dir,
        &format!(
            r#"
                fn init(config) {{
                    shell_spawn("touch {}", #{{ timeout: 5 }});
                }}
            "#,
            marker.display()
        ),
    );

    let frames = run_frames(&[configure_with_script(&script)]);

    assert!(
        frames
            .iter()
            .any(|frame| matches!(frame, HarnessInputMessage::ConfigError(_)))
    );
    assert!(frames.iter().all(|frame| !matches!(
        emitted_event(frame),
        Some(Event::ToolRegistrationDeclared(_))
    )));
    assert!(!marker.exists());
}
#[test]
fn start_runs_after_ready_with_host_functions() {
    // `init` remains a pure planning phase, but `start` is an explicit
    // side-effect phase that runs after host functions are registered.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) {
                return #{ ready_message: "demo ready" };
            }
            fn start(config) {
                tau_info(`started with ${config.vars.greeting}`);
            }
        "#,
    );
    let configure = HarnessOutputMessage::Configure(Configure {
        tool_prefix: None,
        instance_name: tau_proto::ExtensionName::parse("test-extension")
            .expect("test extension name must satisfy the identifier grammar"),
        config: CborValue::Map(vec![
            (
                CborValue::Text("script".to_owned()),
                CborValue::Text(script.display().to_string()),
            ),
            (
                CborValue::Text("vars".to_owned()),
                CborValue::Map(vec![(
                    CborValue::Text("greeting".to_owned()),
                    CborValue::Text("honk".to_owned()),
                )]),
            ),
        ]),
        state_dir: None,
        secrets: BTreeMap::new(),
        settings_files: Default::default(),
    });

    let frames = run_frames(&[configure]);

    let ready_pos = frames
        .iter()
        .position(|frame| matches!(frame, HarnessInputMessage::Ready(_)))
        .expect("ready frame");
    let info_pos = frames
        .iter()
        .position(|frame| {
            matches!(
                requested_notice(frame),
                Some(request) if request.message == "started with honk"
            )
        })
        .expect("start info");
    assert!(ready_pos < info_pos);
}

#[test]
fn start_error_reports_but_keeps_extension_ready() {
    // A broken start hook is isolated like on_event/on_intercept failures: the
    // script is already configured, so report the callback error instead of
    // disabling the extension.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn start() {
                unknown_function();
            }
        "#,
    );

    let frames = run_frames(&[configure_with_script(&script)]);

    assert!(
        frames
            .iter()
            .any(|frame| matches!(frame, HarnessInputMessage::Ready(_)))
    );
    assert!(
        frames
            .iter()
            .all(|frame| !matches!(frame, HarnessInputMessage::ConfigError(_)))
    );
    assert!(frames.iter().any(|frame| matches!(
        requested_notice(frame),
        Some(request) if request.message.contains("rhai start failed")
    )));
}

#[test]
fn tau_emit_respects_transient_flag_and_reports_invalid_events() {
    // The two event-emission host functions differ only in durability metadata,
    // and invalid script-shaped events must become diagnostics instead of being
    // silently dropped.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn start(config) {
                let event = #{ event: "term.bell", payload: #{} };
                tau_emit(event);
                tau_emit_transient(event);
                tau_emit(#{ event: "not.a.real.event", payload: #{} });
            }
        "#,
    );

    let frames = run_frames(&[configure_with_script(&script)]);

    let bell_emits: Vec<_> = frames
        .iter()
        .filter(|frame| matches!(emitted_event(frame), Some(Event::TermBell(_))))
        .collect();
    assert_eq!(bell_emits.len(), 2);
    assert_eq!(emitted_transient(bell_emits[0]), Some(false));
    assert_eq!(emitted_transient(bell_emits[1]), Some(true));
    assert!(frames.iter().any(|frame| matches!(
        requested_notice(frame),
        Some(request) if request.message.contains("rhai invalid event")
    )));
}

#[test]
fn missing_script_config_reports_error_and_stays_inert() {
    // Missing scripts are configuration errors, but the process stays
    // alive long enough to avoid a harness restart loop.
    let frames = run_frames(&[empty_configure()]);

    assert!(matches!(frames[0], HarnessInputMessage::Hello(_)));
    assert!(matches!(frames[1], HarnessInputMessage::ConfigError(_)));
    assert_eq!(frames.len(), 2);
}

#[test]
fn unknown_config_field_reports_config_error() {
    // Extension config uses deny_unknown_fields so misspelled options fail
    // closed and do not silently disable intended limits or script settings.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(&dir, "");
    let configure = configure_with_script_and_extra(
        &script,
        vec![(CborValue::Text("unknown".to_owned()), CborValue::Bool(true))],
    );

    let frames = run_frames(&[configure]);

    assert!(frames.iter().any(|frame| matches!(
        frame,
        HarnessInputMessage::ConfigError(error) if error.message.contains("unknown field")
    )));
}

#[test]
fn max_operations_limit_aborts_runaway_callback() {
    // Script operation limits are a key guardrail for callbacks that accidentally
    // spin forever while handling harness events.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) {
                return #{ subscribe: [#{ kind: "exact", value: "agent.prompt_submitted" }] };
            }
            fn on_event(event, meta) {
                while true {}
            }
        "#,
    );
    let configure = configure_with_script_and_extra(
        &script,
        vec![(
            CborValue::Text("limits".to_owned()),
            CborValue::Map(vec![(
                CborValue::Text("max_operations".to_owned()),
                CborValue::Integer(1000.into()),
            )]),
        )],
    );
    let delivered = HarnessOutputMessage::deliver_live(UnixMicros::new(1), prompt_event("loop"));

    let frames = run_frames(&[configure, delivered]);

    assert!(frames.iter().any(|frame| matches!(
        requested_notice(frame),
        Some(request) if request.message.contains("on_event failed")
    )));
}

#[test]
fn delivered_event_invokes_script_with_replay_meta() {
    // A delivered event is converted to the JSON-shaped Rhai map; the meta
    // map exposes the replay marker and recorded_at timestamp so scripts can
    // distinguish catch-up history from live events.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) {
                return #{ subscribe: [#{ kind: "exact", value: "agent.prompt_submitted" }] };
            }
            fn on_event(event, meta) {
                tau_info(`saw ${meta.replay}/${meta.recorded_at}: ${event.payload.text}`);
            }
        "#,
    );
    let live = HarnessOutputMessage::deliver_live(UnixMicros::new(11), prompt_event("hello"));
    let replayed = HarnessOutputMessage::deliver_replay(UnixMicros::new(7), prompt_event("old"));

    let frames = run_frames(&[configure_with_script(&script), live, replayed]);

    assert!(frames.iter().any(|frame| matches!(
        requested_notice(frame),
        Some(request) if request.message.contains("saw false/11: hello")
    )));
    assert!(frames.iter().any(|frame| matches!(
        requested_notice(frame),
        Some(request) if request.message.contains("saw true/7: old")
    )));
}

#[test]
fn script_error_during_on_event_reports_and_keeps_running() {
    // Callback errors are isolated to the failing callback so one bad hook
    // cannot wedge delivery of later events.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) {
                return #{ subscribe: [#{ kind: "exact", value: "agent.prompt_submitted" }] };
            }
            fn on_event(event, meta) {
                if event.payload.text == "boom" {
                    unknown_function();
                }
                tau_info(`handled ${event.payload.text}`);
            }
        "#,
    );
    let failing = HarnessOutputMessage::deliver_live(UnixMicros::new(12), prompt_event("boom"));
    let following = HarnessOutputMessage::deliver_live(UnixMicros::new(13), prompt_event("after"));

    let frames = run_frames(&[configure_with_script(&script), failing, following]);

    assert!(frames.iter().any(|frame| matches!(
        requested_notice(frame),
        Some(request) if request.message.contains("on_event failed")
    )));
    assert!(frames.iter().any(|frame| matches!(
        requested_notice(frame),
        Some(request) if request.message.contains("handled after")
    )));
}

#[test]
fn init_merges_same_priority_intercepts() {
    // The harness stores one interceptor registration per connection, so
    // same-priority init entries are collapsed into one registration.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) {
                return #{
                    intercept: [
                        #{ selectors: [#{ kind: "exact", value: "agent.prompt_submitted" }], priority: 0 },
                        #{ selectors: [#{ kind: "prefix", value: "tool." }], priority: 0 },
                    ],
                };
            }
        "#,
    );

    let frames = run_frames(&[configure_with_script(&script)]);

    let intercepts: Vec<_> = frames
        .iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::Intercept(intercept) => Some(intercept),
            _ => None,
        })
        .collect();
    assert_eq!(intercepts.len(), 1);
    assert_eq!(intercepts[0].priority, InterceptionPriority::new(0));
    assert_eq!(intercepts[0].selectors.len(), 2);
    assert!(matches!(
        &intercepts[0].selectors[0],
        EventSelector::Exact(name) if name.to_string() == "agent.prompt_submitted"
    ));
    assert!(matches!(
        &intercepts[0].selectors[1],
        EventSelector::Prefix(prefix) if prefix == "tool."
    ));
}

#[test]
fn init_rejects_mixed_priority_intercepts() {
    // Multiple priority levels would require multiple harness
    // registrations, so the prototype rejects that script contract.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) {
                return #{
                    intercept: [
                        #{ selectors: [#{ kind: "exact", value: "agent.prompt_submitted" }], priority: 0 },
                        #{ selectors: [#{ kind: "prefix", value: "tool." }], priority: 1 },
                    ],
                };
            }
        "#,
    );

    let frames = run_frames(&[configure_with_script(&script)]);

    assert!(frames.iter().any(|frame| matches!(
        frame,
        HarnessInputMessage::ConfigError(error) if error.message.contains("same priority")
    )));
    assert!(
        frames
            .iter()
            .all(|frame| !matches!(frame, HarnessInputMessage::Ready(_)))
    );
    assert!(
        frames
            .iter()
            .all(|frame| !matches!(frame, HarnessInputMessage::Intercept(_)))
    );
}
#[test]
fn intercept_callback_can_drop_event() {
    // Intercept callbacks must return exactly one InterceptReply. This
    // covers the simplest script-controlled policy: dropping an event.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) {
                return #{
                    intercept: [#{
                        selectors: [#{ kind: "exact", value: "agent.prompt_submitted" }],
                        priority: 0,
                    }],
                };
            }
            fn on_intercept(event, persist) { return "drop"; }
        "#,
    );
    let req = HarnessOutputMessage::InterceptRequest(InterceptRequest {
        event: Box::new(prompt_event("hello")),
        persist: true,
    });

    let frames = run_frames(&[configure_with_script(&script), req]);
    let replies = frames
        .iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::InterceptReply(reply) => Some(reply),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(replies.len(), 1);
    assert!(matches!(replies[0].action, InterceptAction::Drop));
}

/// Script-authored best-effort output may exhaust the real detached FIFO, but
/// the owned interception reply must wait behind it and publish exactly once.
#[test]
fn intercept_reply_survives_actual_detached_output_saturation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) {
                return #{
                    intercept: [#{
                        selectors: [#{ kind: "exact", value: "agent.prompt_submitted" }],
                        priority: 0,
                    }],
                };
            }
            fn on_intercept(event, persist) {
                for n in 0..96 {
                    tau_emit_transient(#{ event: "term.bell", payload: #{} });
                }
                return "drop";
            }
        "#,
    );
    let request = HarnessOutputMessage::InterceptRequest(InterceptRequest {
        event: Box::new(prompt_event("saturate")),
        persist: true,
    });

    let frames = run_saturation_fixture(&[configure_with_script(&script), request]);

    let bell_count = frames
        .iter()
        .filter(|frame| matches!(emitted_event(frame), Some(Event::TermBell(_))))
        .count();
    assert!(bell_count < 96, "fixture must exhaust the detached FIFO");
    let replies = frames
        .iter()
        .filter(|frame| matches!(frame, HarnessInputMessage::InterceptReply(_)))
        .count();
    assert_eq!(replies, 1);
    assert!(matches!(
        frames.last(),
        Some(HarnessInputMessage::InterceptReply(reply))
            if matches!(reply.action, InterceptAction::Drop)
    ));
}

/// Ordered reply failure must leave the manual loop through its error path so
/// harness disconnect cleanup can release the retained interception.
#[test]
fn intercept_reply_flush_failure_terminates_runtime() {
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) {
                return #{
                    intercept: [#{
                        selectors: [#{ kind: "exact", value: "agent.prompt_submitted" }],
                        priority: 0,
                    }],
                };
            }
            fn on_intercept(event, persist) { return "drop"; }
        "#,
    );
    let request = HarnessOutputMessage::InterceptRequest(InterceptRequest {
        event: Box::new(prompt_event("fail reply")),
        persist: true,
    });
    let mut input = Vec::new();
    let mut input_writer = HarnessOutputWriter::new(&mut input);
    for frame in [configure_with_script(&script), request] {
        input_writer.write_message(&frame).expect("write input");
    }
    input_writer.flush().expect("flush input");
    let failed = Arc::new(AtomicBool::new(false));

    let result = run(
        Cursor::new(input),
        MandatoryFlushFailureWriter {
            ready_seen: false,
            failed: Arc::clone(&failed),
        },
    );

    assert!(
        result.is_err(),
        "reply flush failure must terminate the loop"
    );
    assert!(failed.load(Ordering::Acquire));
}

#[test]
fn intercept_callback_can_return_replacement_event() {
    // A script can mutate the JSON-shaped event map and pass the
    // replacement back through Rust deserialization.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) {
                return #{
                    intercept: [#{
                        selectors: [#{ kind: "exact", value: "agent.prompt_submitted" }],
                        priority: 0,
                    }],
                };
            }
            fn on_intercept(event, persist) {
                event.payload.text = "changed";
                return #{ kind: "pass", event: event };
            }
        "#,
    );
    let req = HarnessOutputMessage::InterceptRequest(InterceptRequest {
        event: Box::new(prompt_event("hello")),
        persist: true,
    });

    let frames = run_frames(&[configure_with_script(&script), req]);

    let replies = frames
        .iter()
        .filter_map(|frame| match frame {
            HarnessInputMessage::InterceptReply(reply) => Some(reply),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(replies.len(), 1);
    let replacement = match &replies[0].action {
        InterceptAction::Pass(Some(event)) => Some(event.as_ref()),
        _ => None,
    };
    assert!(matches!(
        replacement,
        Some(Event::AgentPromptSubmitted(prompt)) if prompt.text == "changed"
    ));
}

#[test]
fn register_tool_emits_registration_before_ready() {
    // Tool registrations are staged during init and emitted before Ready so
    // the harness can route later calls only after the script is configured.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) {
                register_tool_group("host", #{});
                register_tool("project_status", #{
                    group: "host",
                    description: "Get project status",
                    parameters: #{ type: "object", additionalProperties: false },
                }, Fn("project_status"));
            }
            fn project_status(args, c) { return "ok"; }
        "#,
    );

    let frames = run_frames(&[configure_with_script(&script)]);

    let declaration_pos = frames
        .iter()
        .position(|frame| {
            matches!(
                emitted_event(frame),
                Some(Event::ToolRegistrationDeclared(_))
            )
        })
        .expect("tool.registration_declared");
    let ready_pos = frames
        .iter()
        .position(|frame| matches!(frame, HarnessInputMessage::Ready(_)))
        .expect("ready");
    assert!(declaration_pos < ready_pos);
    let Some(Event::ToolRegistrationDeclared(declaration)) =
        emitted_event(&frames[declaration_pos])
    else {
        panic!("expected tool.registration_declared");
    };
    assert_eq!(declaration.tool.name.as_str(), "project_status");
    assert_eq!(
        declaration.tool_group.as_ref().map(|g| g.name.as_str()),
        Some("host")
    );
}

/// A late prefix-composition failure rejects the whole Rhai init plan before
/// any earlier script declaration becomes visible.
#[test]
fn prefixed_init_composition_failure_emits_no_partial_declarations() {
    let dir = tempfile::tempdir().expect("tempdir");
    let long_name = "a".repeat(ToolName::MAX_LEN);
    let script = write_script(
        &dir,
        &format!(
            r#"
                fn init(config) {{
                    register_tool("early", #{{}}, Fn("early"));
                    register_tool("{long_name}", #{{}}, Fn("late"));
                }}
                fn early(args, c) {{ "early"; }}
                fn late(args, c) {{ "late"; }}
            "#
        ),
    );
    let mut configure = configure_with_script(&script);
    let HarnessOutputMessage::Configure(configure) = &mut configure else {
        unreachable!();
    };
    configure.tool_prefix = Some(tau_proto::ToolNamePrefix::parse("work").expect("valid prefix"));

    let frames = run_frames(&[HarnessOutputMessage::Configure(configure.clone())]);

    assert!(matches!(frames[0], HarnessInputMessage::Hello(_)));
    let HarnessInputMessage::ConfigError(error) = &frames[1] else {
        panic!("expected ConfigError, got {:?}", frames[1]);
    };
    assert!(
        error.message.contains("exceed") || error.message.contains("too long"),
        "{}",
        error.message
    );
    assert_eq!(frames.len(), 2);
}

#[test]
fn live_owned_tool_started_invokes_handler_and_replay_is_ignored() {
    // Owned live tool.started events are consumed by the tool dispatcher and
    // produce terminal tool results, while replayed history is ignored to avoid
    // re-running script side effects.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) {
                register_tool("echo_args", #{ description: "Echo args" }, Fn("echo_args"));
            }
            fn echo_args(args, c) { return `saw ${args.text} via ${c.tool_name}`; }
            fn on_event(event, meta) { tau_info("raw should not see owned tool"); }
        "#,
    );
    let mut configure = configure_with_script(&script);
    let HarnessOutputMessage::Configure(configure_frame) = &mut configure else {
        unreachable!();
    };
    configure_frame.tool_prefix =
        Some(tau_proto::ToolNamePrefix::parse("work").expect("valid prefix"));
    let live = HarnessOutputMessage::deliver_live(
        UnixMicros::new(1),
        tool_started(
            "work_echo_args",
            CborValue::Map(vec![(
                CborValue::Text("text".to_owned()),
                CborValue::Text("hello".to_owned()),
            )]),
        ),
    );
    let replay = HarnessOutputMessage::deliver_replay(
        UnixMicros::new(2),
        tool_started("work_echo_args", CborValue::Map(Vec::new())),
    );

    let frames = run_frames(&[configure, live, replay]);

    assert!(frames.iter().any(|frame| matches!(
        emitted_event(frame),
        Some(Event::ToolRegistrationDeclared(register)) if register.tool.name.as_str() == "work_echo_args"
    )));
    let results: Vec<_> = frames
        .iter()
        .filter_map(|frame| match emitted_event(frame) {
            Some(Event::ToolResultReported(result)) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].result,
        CborValue::Text("saw hello via echo_args".to_owned())
    );
    assert_eq!(results[0].tool_name.as_str(), "work_echo_args");
    assert!(frames.iter().all(|frame| !matches!(
        requested_notice(frame),
        Some(request) if request.message.contains("raw should not see")
    )));
}

#[test]
fn tool_handler_throw_emits_tool_error_and_keeps_running() {
    // Handler exceptions fail only the current tool call and do not disable the
    // extension, so a subsequent call can still complete.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) { register_tool("maybe", #{}, Fn("maybe")); }
            fn maybe(args, c) {
                if args.fail { throw "boom"; }
                return "ok";
            }
        "#,
    );
    let fail = HarnessOutputMessage::deliver_live(
        UnixMicros::new(1),
        tool_started(
            "maybe",
            CborValue::Map(vec![(
                CborValue::Text("fail".to_owned()),
                CborValue::Bool(true),
            )]),
        ),
    );
    let ok = HarnessOutputMessage::deliver_live(
        UnixMicros::new(2),
        tool_started(
            "maybe",
            CborValue::Map(vec![(
                CborValue::Text("fail".to_owned()),
                CborValue::Bool(false),
            )]),
        ),
    );

    let frames = run_frames(&[configure_with_script(&script), fail, ok]);

    assert!(
        frames
            .iter()
            .any(|frame| matches!(emitted_event(frame), Some(Event::ToolErrorReported(_))))
    );
    assert!(frames.iter().any(|frame| matches!(
        emitted_event(frame),
        Some(Event::ToolResultReported(result)) if result.result == CborValue::Text("ok".to_owned())
    )));
}

/// Synchronous results, synchronous errors, and shell-owned terminals must all
/// wait behind saturated optional script output and publish exactly once.
#[test]
fn all_rhai_tool_terminals_survive_actual_detached_output_saturation() {
    let cases = [
        (
            "sync_result",
            r#"
                fn init(config) { register_tool("owned", #{}, Fn("owned")); }
                fn owned(args, c) {
                    for n in 0..96 {
                        tau_emit_transient(#{ event: "term.bell", payload: #{} });
                    }
                    return "ok";
                }
            "#,
            false,
        ),
        (
            "sync_error",
            r#"
                fn init(config) { register_tool("owned", #{}, Fn("owned")); }
                fn owned(args, c) {
                    for n in 0..96 {
                        tau_emit_transient(#{ event: "term.bell", payload: #{} });
                    }
                    throw "sync boom";
                }
            "#,
            true,
        ),
        (
            "shell_error",
            r#"
                fn init(config) { register_tool("owned", #{}, Fn("owned")); }
                fn owned(args, c) {
                    let job = shell_spawn(
                        "printf shell-ok",
                        #{ timeout: 5, on_complete: Fn("done") },
                    );
                    for n in 0..96 {
                        tau_emit_transient(#{ event: "term.bell", payload: #{} });
                    }
                    return job;
                }
                fn done(result, job) { throw "shell boom"; }
            "#,
            true,
        ),
    ];

    for (label, source, expect_error) in cases {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = write_script(&dir, source);
        let started = HarnessOutputMessage::deliver_live(
            UnixMicros::new(1),
            tool_started("owned", CborValue::Map(Vec::new())),
        );

        let frames = run_saturation_fixture(&[configure_with_script(&script), started]);

        let bell_count = frames
            .iter()
            .filter(|frame| matches!(emitted_event(frame), Some(Event::TermBell(_))))
            .count();
        assert!(
            bell_count < 96,
            "{label} fixture must exhaust the detached FIFO"
        );
        let terminals = frames
            .iter()
            .filter(|frame| {
                matches!(
                    emitted_event(frame),
                    Some(Event::ToolResultReported(_) | Event::ToolErrorReported(_))
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(terminals.len(), 1, "{label} terminal count");
        assert_eq!(
            matches!(
                emitted_event(terminals[0]),
                Some(Event::ToolErrorReported(_))
            ),
            expect_error,
            "{label} terminal kind"
        );
        assert!(
            matches!(
                emitted_event(frames.last().expect("last frame")),
                Some(Event::ToolResultReported(_) | Event::ToolErrorReported(_))
            ),
            "{label} terminal must follow optional output"
        );
    }
}

/// A failed checked result, error, or shell terminal must tear down the runtime
/// instead of leaving its routed call owned by a connected Rhai extension.
#[test]
fn all_rhai_tool_terminal_flush_failures_terminate_runtime() {
    let cases = [
        r#"
            fn init(config) { register_tool("owned", #{}, Fn("owned")); }
            fn owned(args, c) { return "ok"; }
        "#,
        r#"
            fn init(config) { register_tool("owned", #{}, Fn("owned")); }
            fn owned(args, c) { throw "sync boom"; }
        "#,
        r#"
            fn init(config) { register_tool("owned", #{}, Fn("owned")); }
            fn owned(args, c) {
                return shell_spawn(
                    "printf shell-ok",
                    #{ timeout: 5, on_complete: Fn("done") },
                );
            }
            fn done(result, job) { throw "shell boom"; }
        "#,
    ];

    for source in cases {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = write_script(&dir, source);
        let started = HarnessOutputMessage::deliver_live(
            UnixMicros::new(1),
            tool_started("owned", CborValue::Map(Vec::new())),
        );
        let mut input = Vec::new();
        let mut input_writer = HarnessOutputWriter::new(&mut input);
        for frame in [configure_with_script(&script), started] {
            input_writer.write_message(&frame).expect("write input");
        }
        input_writer.flush().expect("flush input");
        let failed = Arc::new(AtomicBool::new(false));

        let result = run(
            Cursor::new(input),
            MandatoryFlushFailureWriter {
                ready_seen: false,
                failed: Arc::clone(&failed),
            },
        );

        assert!(
            result.is_err(),
            "terminal flush failure must terminate the loop"
        );
        assert!(failed.load(Ordering::Acquire));
    }
}

/// A returned shell job holds its tool call open until its callback supplies
/// one result.
#[test]
fn shell_job_returned_by_tool_defers_until_completion_callback() {
    let dir = tempfile::tempdir().expect("tempdir");
    let started_marker = dir.path().join("shell-started");
    let release = dir.path().join("release-shell");
    let script = write_script(
        &dir,
        &format!(
            r#"
            fn init(config) {{ register_tool("host_echo", #{{}}, Fn("host_echo")); }}
            fn host_echo(args, c) {{
                return shell_spawn(
                    "touch '{}'; while [ ! -e '{}' ]; do sleep 0.01; done; printf shell-ok",
                    #{{ timeout: 5, on_complete: Fn("done") }},
                );
            }}
            fn done(result, job) {{
                if !result.success {{ throw result.output; }}
                return result.output;
            }}
        "#,
            started_marker.display(),
            release.display()
        ),
    );
    let (input_reader, input_writer) = UnixStream::pair().expect("unix stream pair");
    let mut harness_writer = HarnessOutputWriter::new(
        input_writer
            .try_clone()
            .expect("clone harness input writer"),
    );
    let output = SharedWriter::default();
    let run_output = output.clone();
    let (finished_tx, finished_rx) = mpsc::channel();
    let run_thread = std::thread::spawn(move || {
        let result = run(input_reader, run_output).map_err(|error| error.to_string());
        finished_tx
            .send(result)
            .expect("test receiver should stay alive");
    });

    for frame in [
        configure_with_script(&script),
        HarnessOutputMessage::deliver_live(
            UnixMicros::new(1),
            tool_started("host_echo", CborValue::Map(Vec::new())),
        ),
    ] {
        harness_writer
            .write_message(&frame)
            .expect("write harness input");
    }
    harness_writer.flush().expect("flush harness input");
    wait_for_file(&started_marker);

    let before_release = frames_from_bytes_lossy(output.bytes());
    assert!(
        before_release.iter().all(|frame| !matches!(
            emitted_event(frame),
            Some(Event::ToolResultReported(_) | Event::ToolErrorReported(_))
        )),
        "shell-backed tool emitted a terminal before its release gate opened"
    );

    std::fs::write(&release, "").expect("release shell command");
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let frames = frames_from_bytes_lossy(output.bytes());
        let terminals: Vec<_> = frames
            .iter()
            .filter(|frame| {
                matches!(
                    emitted_event(frame),
                    Some(Event::ToolResultReported(_) | Event::ToolErrorReported(_))
                )
            })
            .collect();
        if !terminals.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the deferred shell callback terminal"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(harness_writer);
    drop(input_writer);
    finished_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("extension should exit after input closes")
        .expect("run rhai extension");
    run_thread.join().expect("run thread");

    let frames = frames_from_bytes_lossy(output.bytes());
    let terminals: Vec<_> = frames
        .iter()
        .filter_map(|frame| match emitted_event(frame) {
            Some(Event::ToolResultReported(result)) => Some(Ok(result)),
            Some(Event::ToolErrorReported(error)) => Some(Err(error)),
            _ => None,
        })
        .collect();
    assert_eq!(terminals.len(), 1);
    let result = terminals[0]
        .as_ref()
        .expect("callback should return ToolResult");
    assert_eq!(result.call_id, tau_proto::ToolCallId::new("call_1"));
    assert_eq!(result.result, CborValue::Text("shell-ok".to_owned()));
}

#[test]
fn shell_completion_wakes_runtime_while_harness_input_stays_open() {
    // A completed shell-backed tool must produce its ToolResult without waiting
    // for another harness frame or input EOF; the shell worker wake is the only
    // stimulus after the tool.started frame.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) { register_tool("host_echo", #{}, Fn("host_echo")); }
            fn host_echo(args, c) {
                return shell_spawn("printf live-open", #{ timeout: 5 });
            }
        "#,
    );

    let (input_reader, input_writer) = UnixStream::pair().expect("unix stream pair");
    let mut harness_writer = HarnessOutputWriter::new(
        input_writer
            .try_clone()
            .expect("clone harness input writer"),
    );
    let output = SharedWriter::default();
    let run_output = output.clone();
    let run_thread = std::thread::spawn(move || {
        run(input_reader, run_output).map_err(|error| error.to_string())
    });

    harness_writer
        .write_message(&configure_with_script(&script))
        .expect("write configure");
    harness_writer
        .write_message(&HarnessOutputMessage::deliver_live(
            UnixMicros::new(1),
            tool_started("host_echo", CborValue::Map(Vec::new())),
        ))
        .expect("write tool start");
    harness_writer.flush().expect("flush harness input");

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let frames = frames_from_bytes_lossy(output.bytes());
        if frames
            .iter()
            .any(|frame| tool_result_has_output(frame, "live-open"))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for shell completion wake"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    drop(harness_writer);
    drop(input_writer);
    run_thread
        .join()
        .expect("run thread")
        .expect("run rhai extension");
}

#[test]
fn shell_completions_are_not_starved_by_ready_harness_input() {
    // The runtime checks shell completions between harness messages so replay
    // catch-up or another ready burst cannot postpone completed shell callbacks
    // until all queued harness input has drained.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) { register_tool("host_echo", #{}, Fn("host_echo")); }
            fn host_echo(args, c) {
                return shell_spawn("printf fair", #{ timeout: 5 });
            }
            fn on_event(event, meta) {
                let checksum = 0;
                for n in 0..20000 {
                    checksum += n;
                }
                tau_info(event.payload.message);
            }
        "#,
    );
    let mut input = vec![
        configure_with_script(&script),
        HarnessOutputMessage::deliver_live(
            UnixMicros::new(1),
            tool_started("host_echo", CborValue::Map(Vec::new())),
        ),
    ];
    for i in 0..200 {
        input.push(HarnessOutputMessage::deliver_live(
            UnixMicros::new(2 + i),
            Event::HarnessNotice(HarnessNotice {
                kind: tau_proto::notice_kind::EXTENSION_NOTICE.to_owned(),
                message: format!("flood-{i}"),
                level: NoticeLevel::Info,
                purpose: tau_proto::NoticePurpose::Diagnostic,
            }),
        ));
    }

    let frames = run_frames(&input);
    let result_index = frames
        .iter()
        .position(|frame| tool_result_has_output(frame, "fair"))
        .expect("tool result");
    let last_flood_notice_index = frames
        .iter()
        .rposition(|frame| {
            matches!(
                requested_notice(frame),
                Some(request) if request.message.starts_with("flood-")
            )
        })
        .expect("flood notice");
    assert!(
        result_index < last_flood_notice_index,
        "shell completion was emitted only after the harness burst drained"
    );
}

#[test]
fn shell_completion_callback_throw_emits_tool_error() {
    // A shell completion callback exception maps to ToolError for the deferred
    // call instead of silently dropping the failure.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) { register_tool("bad_shell", #{}, Fn("bad_shell")); }
            fn bad_shell(args, c) {
                return shell_spawn("printf shell-ok", #{ timeout: 5, on_complete: Fn("done") });
            }
            fn done(result, job) { throw "callback boom"; }
        "#,
    );
    let started = HarnessOutputMessage::deliver_live(
        UnixMicros::new(1),
        tool_started("bad_shell", CborValue::Map(Vec::new())),
    );

    let frames = run_frames(&[configure_with_script(&script), started]);

    assert!(frames.iter().any(|frame| matches!(
        emitted_event(frame),
        Some(Event::ToolErrorReported(error)) if error.message.contains("callback boom")
    )));
}

/// Shell results preserve working-directory behavior, stderr appending, nonzero
/// exits, and start failures for script tools.
#[test]
fn shell_result_includes_cwd_stderr_exit_and_start_error_shape() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cwd = tempfile::tempdir().expect("tempdir");
    std::fs::write(cwd.path().join("input.txt"), "ok").expect("write cwd input");
    let missing_cwd = dir.path().join("missing");
    let script = write_script(
        &dir,
        &format!(
            r#"
                fn init(config) {{
                    register_tool("shell_contract", #{{}}, Fn("shell_contract"));
                }}
                fn shell_contract(args, c) {{
                    if args["case"] == "cwd_stderr" {{
                        return shell_spawn("cat input.txt; printf err >&2; exit 7", #{{
                            cwd: "{}",
                            timeout: 5,
                        }});
                    }}
                    return shell_spawn("printf nope", #{{
                        cwd: "{}",
                        timeout: 5,
                    }});
                }}
            "#,
            cwd.path().display(),
            missing_cwd.display()
        ),
    );
    let ok = HarnessOutputMessage::deliver_live(
        UnixMicros::new(1),
        tool_started(
            "shell_contract",
            CborValue::Map(vec![(
                CborValue::Text("case".to_owned()),
                CborValue::Text("cwd_stderr".to_owned()),
            )]),
        ),
    );
    let start_error = HarnessOutputMessage::deliver_live(
        UnixMicros::new(2),
        tool_started(
            "shell_contract",
            CborValue::Map(vec![(
                CborValue::Text("case".to_owned()),
                CborValue::Text("start_error".to_owned()),
            )]),
        ),
    );

    let frames = run_frames(&[configure_with_script(&script), ok, start_error]);

    let results: Vec<_> = frames
        .iter()
        .filter_map(|frame| match emitted_event(frame) {
            Some(Event::ToolResultReported(result)) => Some(&result.result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    let cwd_result = results
        .iter()
        .find_map(|result| match result {
            CborValue::Map(fields)
                if fields.iter().any(|(key, value)| {
                    matches!(
                        (key, value),
                        (CborValue::Text(key), CborValue::Text(output))
                            if key == "output" && output.contains("ok")
                    )
                }) =>
            {
                Some(fields)
            }
            _ => None,
        })
        .expect("cwd shell result");
    assert!(cwd_result.iter().any(|(key, value)| matches!(
        (key, value),
        (CborValue::Text(key), CborValue::Bool(false)) if key == "success"
    )));
    assert!(cwd_result.iter().any(|(key, value)| matches!(
        (key, value),
        (CborValue::Text(key), CborValue::Integer(status)) if key == "status" && *status == 7.into()
    )));
    assert!(cwd_result.iter().any(|(key, value)| matches!(
        (key, value),
        (CborValue::Text(key), CborValue::Text(output))
            if key == "output" && output.contains("ok") && output.contains("[stderr]\nerr")
    )));
    let _start_error_result = results
        .iter()
        .find_map(|result| match result {
            CborValue::Map(fields)
                if fields.iter().any(|(key, value)| {
                    matches!(
                        (key, value),
                        (CborValue::Text(key), CborValue::Text(reason))
                            if key == "termination_reason" && reason == "start_error"
                    )
                }) =>
            {
                Some(fields)
            }
            _ => None,
        })
        .expect("start error shell result");
}

/// An oversized timeout rejects its tool call without spawning the requested
/// command.
#[test]
fn oversized_shell_timeout_is_rejected_without_spawning_or_duplicate_terminal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let marker = dir.path().join("oversized-timeout-command-ran");
    let script = write_script(
        &dir,
        &format!(
            r#"
            fn init(config) {{ register_tool("huge_timeout", #{{}}, Fn("huge_timeout")); }}
            fn huge_timeout(args, c) {{
                return shell_spawn("touch '{}'", #{{ timeout: 999999999999999999 }});
            }}
        "#,
            marker.display()
        ),
    );
    let started = HarnessOutputMessage::deliver_live(
        UnixMicros::new(1),
        tool_started("huge_timeout", CborValue::Map(Vec::new())),
    );

    let frames = run_frames(&[configure_with_script(&script), started]);

    assert!(!marker.exists(), "rejected shell command must not run");
    let terminals: Vec<_> = frames
        .iter()
        .filter_map(|frame| match emitted_event(frame) {
            Some(Event::ToolResultReported(result)) => Some(Ok(result)),
            Some(Event::ToolErrorReported(error)) => Some(Err(error)),
            _ => None,
        })
        .collect();
    assert_eq!(terminals.len(), 1, "tool call must have one terminal");
    let error = terminals[0]
        .as_ref()
        .expect_err("oversized timeout must not emit ToolResult");
    assert_eq!(error.call_id, tau_proto::ToolCallId::new("call_1"));
    assert!(error.message.contains("timeout must be at most"));
}

/// A completed job must release its admission slot before its callback spawns
/// a chained replacement, while its routed tool ownership transfers exactly
/// once.
#[test]
fn shell_callback_can_chain_at_pending_job_admission_cap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let release = dir.path().join("release-waiters");
    let script = write_script(
        &dir,
        &format!(
            r#"
                fn init(config) {{ register_tool("chain_at_cap", #{{}}, Fn("chain_at_cap")); }}
                fn chain_at_cap(args, c) {{
                    let owned = shell_spawn(
                        "sleep 0.1",
                        #{{ timeout: 5, on_complete: Fn("chain") }},
                    );
                    for n in 0..31 {{
                        shell_spawn(
                            "while [ ! -e '{}' ]; do sleep 0.01; done",
                            #{{ timeout: 5 }},
                        );
                    }}
                    return owned;
                }}
                fn chain(result, job) {{
                    return shell_spawn(
                        "touch '{}'; printf chained",
                        #{{ timeout: 5 }},
                    );
                }}
            "#,
            release.display(),
            release.display(),
        ),
    );
    let started = HarnessOutputMessage::deliver_live(
        UnixMicros::new(1),
        tool_started("chain_at_cap", CborValue::Map(Vec::new())),
    );

    let frames = run_frames(&[configure_with_script(&script), started]);

    let terminals = frames
        .iter()
        .filter(|frame| {
            matches!(
                emitted_event(frame),
                Some(Event::ToolResultReported(_) | Event::ToolErrorReported(_))
            )
        })
        .count();
    assert_eq!(terminals, 1);
    assert!(
        frames
            .iter()
            .any(|frame| tool_result_has_output(frame, "chained"))
    );
}

/// Disconnect cancels an admitted shell before it can produce its side effect.
#[test]
fn disconnect_cancels_pending_shell_jobs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let marker = dir.path().join("marker");
    let started_marker = dir.path().join("shell-started");
    let pid_file = dir.path().join("shell-pid");
    let mut child_cleanup = DetachedProcessCleanup::new(pid_file.clone());
    let script = write_script(
        &dir,
        &format!(
            r#"
                fn init(config) {{ register_tool("long_shell", #{{}}, Fn("long_shell")); }}
                fn long_shell(args, c) {{
                    return shell_spawn(
                        "echo $$ > '{}'; touch '{}'; while :; do sleep 1; done; touch '{}'",
                        #{{ timeout: 10 }},
                    );
                }}
            "#,
            pid_file.display(),
            started_marker.display(),
            marker.display()
        ),
    );
    let (input_reader, input_writer) = UnixStream::pair().expect("unix stream pair");
    let mut harness_writer = HarnessOutputWriter::new(
        input_writer
            .try_clone()
            .expect("clone harness input writer"),
    );
    let output = SharedWriter::default();
    let (finished_tx, finished_rx) = mpsc::channel();
    let run_thread = std::thread::spawn(move || {
        let result = run(input_reader, output).map_err(|error| error.to_string());
        finished_tx
            .send(result)
            .expect("test receiver should stay alive");
    });

    for frame in [
        configure_with_script(&script),
        HarnessOutputMessage::deliver_live(
            UnixMicros::new(1),
            tool_started("long_shell", CborValue::Map(Vec::new())),
        ),
    ] {
        harness_writer
            .write_message(&frame)
            .expect("write harness input");
    }
    harness_writer.flush().expect("flush harness input");
    wait_for_file(&started_marker);
    let pid = child_cleanup.wait_for_pid();
    harness_writer
        .write_message(&HarnessOutputMessage::Disconnect(
            tau_proto::Disconnect::default(),
        ))
        .expect("write disconnect");
    harness_writer.flush().expect("flush disconnect");
    finished_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("disconnect should stop extension promptly")
        .expect("run rhai extension");
    run_thread.join().expect("run thread");

    assert!(
        process_disappears(pid, Duration::from_secs(1)),
        "disconnect must kill the admitted shell process"
    );
    child_cleanup.mark_cleaned();
    assert!(
        !marker.exists(),
        "canceled shell must not create its marker"
    );
}

#[test]
fn shell_completion_kills_background_descendant_holding_output_pipe() {
    // A shell that exits while a background child inherits stdout used to wedge
    // when joining pipe readers. Completion must kill the remaining process
    // group before waiting for captured output readers.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) { register_tool("background_pipe", #{}, Fn("background_pipe")); }
            fn background_pipe(args, c) {
                return shell_spawn("sleep 60 & printf done", #{ timeout: 10 });
            }
        "#,
    );
    let started = HarnessOutputMessage::deliver_live(
        UnixMicros::new(1),
        tool_started("background_pipe", CborValue::Map(Vec::new())),
    );

    let started_at = Instant::now();
    let frames = run_frames(&[configure_with_script(&script), started]);

    assert!(started_at.elapsed() < Duration::from_secs(1));
    assert_eq!(tool_result_output(&frames), "done");
}

/// An escaped idle pipe holder cannot delay completion and is fixture-cleaned.
#[test]
fn shell_completion_does_not_wait_for_detached_descendant_holding_output_pipe() {
    if !setsid_available() {
        eprintln!("skipping detached setsid regression: setsid not available");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let pid_file = dir.path().join("detached-pipe-pid");
    let ready_file = dir.path().join("detached-pipe-ready");
    let mut detached_cleanup = DetachedProcessCleanup::new(pid_file.clone());
    let script = write_script(
        &dir,
        &format!(
            r#"
            fn init(config) {{ register_tool("detached_pipe", #{{}}, Fn("detached_pipe")); }}
            fn detached_pipe(args, c) {{
                return shell_spawn("setsid sh -c 'echo $$ > \"{}\"; touch \"{}\"; sleep 2' & while [ ! -e \"{}\" ]; do sleep 0.01; done; printf done", #{{ timeout: 10 }});
            }}
        "#,
            pid_file.display(),
            ready_file.display(),
            ready_file.display()
        ),
    );
    let started = HarnessOutputMessage::deliver_live(
        UnixMicros::new(1),
        tool_started("detached_pipe", CborValue::Map(Vec::new())),
    );

    let started_at = Instant::now();
    let frames = run_frames(&[configure_with_script(&script), started]);

    assert!(started_at.elapsed() < Duration::from_secs(1));
    assert_eq!(tool_result_output(&frames), "done");
    detached_cleanup.wait_for_pid();
    assert!(
        detached_cleanup.cleanup(),
        "fixture cleanup must stop detached pipe holder"
    );
}

/// An escaped writer cannot exceed the post-stop drain bound and is
/// fixture-cleaned.
#[test]
fn shell_completion_bounds_detached_descendant_continuing_to_write() {
    if !setsid_available() {
        eprintln!("skipping detached setsid writer regression: setsid not available");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let pid_file = dir.path().join("writing-pipe-pid");
    let ready_file = dir.path().join("writing-pipe-ready");
    let mut detached_cleanup = DetachedProcessCleanup::new(pid_file.clone());
    let script = write_script(
        &dir,
        &format!(
            r#"
            fn init(config) {{ register_tool("writing_pipe", #{{}}, Fn("writing_pipe")); }}
            fn writing_pipe(args, c) {{
                return shell_spawn("setsid sh -c 'echo $$ > \"{}\"; touch \"{}\"; i=0; while [ $i -lt 200 ]; do printf x; i=$((i+1)); sleep 0.01; done' & while [ ! -e \"{}\" ]; do sleep 0.01; done; printf done", #{{ timeout: 10 }});
            }}
        "#,
            pid_file.display(),
            ready_file.display(),
            ready_file.display()
        ),
    );
    let started = HarnessOutputMessage::deliver_live(
        UnixMicros::new(1),
        tool_started("writing_pipe", CborValue::Map(Vec::new())),
    );

    let started_at = Instant::now();
    let frames = run_frames(&[configure_with_script(&script), started]);

    assert!(started_at.elapsed() < Duration::from_secs(1));
    assert!(tool_result_output(&frames).contains("done"));
    detached_cleanup.wait_for_pid();
    assert!(
        detached_cleanup.cleanup(),
        "fixture cleanup must stop detached writer"
    );
}

#[test]
fn shell_timeout_kills_process_group_and_returns_result() {
    // Timeout cleanup must kill descendants that inherit stdout/stderr so pipe
    // reader joins cannot wedge the extension after the shell parent exits.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) { register_tool("timeout_shell", #{}, Fn("timeout_shell")); }
            fn timeout_shell(args, c) {
                return shell_spawn("sh -c 'sleep 60 & wait'", #{ timeout: 1 });
            }
        "#,
    );
    let started = HarnessOutputMessage::deliver_live(
        UnixMicros::new(1),
        tool_started("timeout_shell", CborValue::Map(Vec::new())),
    );

    let frames = run_frames(&[configure_with_script(&script), started]);

    let result = frames
        .iter()
        .find_map(|frame| match emitted_event(frame) {
            Some(Event::ToolResultReported(result)) => Some(&result.result),
            _ => None,
        })
        .expect("tool result");
    let CborValue::Map(fields) = result else {
        panic!("result map");
    };
    assert!(fields.iter().any(|(key, value)| matches!(
        (key, value),
        (CborValue::Text(key), CborValue::Bool(true)) if key == "timed_out"
    )));
    assert!(fields.iter().any(|(key, value)| matches!(
        (key, value),
        (CborValue::Text(key), CborValue::Text(reason)) if key == "termination_reason" && reason == "timeout"
    )));
}

#[test]
fn shell_spawn_admission_cap_fails_deterministically() {
    // The pending-job cap should reject excess shell work as a tool error while
    // keeping the extension responsive instead of spawning unbounded threads.
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        r#"
            fn init(config) { register_tool("saturate_shell", #{}, Fn("saturate_shell")); }
            fn saturate_shell(args, c) {
                for n in 0..33 {
                    shell_spawn("sleep 1", #{ timeout: 5 });
                }
                return "unexpected";
            }
        "#,
    );
    let started = HarnessOutputMessage::deliver_live(
        UnixMicros::new(1),
        tool_started("saturate_shell", CborValue::Map(Vec::new())),
    );

    let frames = run_frames(&[configure_with_script(&script), started]);

    assert!(frames.iter().any(|frame| matches!(
        emitted_event(frame),
        Some(Event::ToolErrorReported(error)) if error.message.contains("too many pending shell jobs")
    )));
}
