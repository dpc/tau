//! Production protocol-output and configuration lifecycle regressions.

use super::*;

/// Writer that blocks on the first optional bell while the detached FIFO fills.
struct SaturationWriter {
    /// Bytes accepted by the production protocol writer.
    bytes: Arc<Mutex<Vec<u8>>>,
    /// Gate held until the fixture observes actual detached overload.
    gate: Arc<(Mutex<bool>, Condvar)>,
    /// Announces that optional output reached the writer.
    entered: mpsc::Sender<()>,
    /// Prevents repeated gate announcements.
    announced: bool,
}

/// Writer that rejects the first post-Ready mandatory tool terminal flush.
struct MandatoryFailureWriter {
    /// Captured non-failing protocol bytes.
    bytes: Arc<Mutex<Vec<u8>>>,
    /// Whether startup Ready has passed.
    ready: bool,
    /// Whether the selected mandatory frame was rejected.
    failed: Arc<AtomicBool>,
}

impl Write for MandatoryFailureWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.windows(5).any(|window| window == b"ready") {
            self.ready = true;
        }
        if self.ready
            && (bytes
                .windows(20)
                .any(|window| window == b"tool.result_reported")
                || bytes
                    .windows(19)
                    .any(|window| window == b"tool.error_reported")
                || bytes
                    .windows(23)
                    .any(|window| window == b"tool.cancelled_reported"))
        {
            self.failed.store(true, Ordering::Release);
            return Ok(bytes.len());
        }
        self.bytes
            .lock()
            .expect("output bytes")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.failed.load(Ordering::Acquire) {
            return Err(io::Error::other("forced mandatory Rostra output failure"));
        }
        Ok(())
    }
}

impl Write for SaturationWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if !self.announced && bytes.windows(9).any(|window| window == b"term.bell") {
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
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Build one valid production configuration for an isolated state directory.
fn production_config(
    state_dir: std::path::PathBuf,
    secret: RostraIdSecretKey,
) -> tau_proto::Configure {
    tau_proto::Configure {
        config: tau_proto::json_to_cbor(&serde_json::json!({
            "identity_mnemonic_secret": "rostra_identity_mnemonic",
        })),
        instance_name: tau_proto::ExtensionName::parse("std-rostra").expect("extension name"),
        tool_prefix: None,
        state_dir: Some(state_dir),
        secrets: BTreeMap::from([(
            "rostra_identity_mnemonic".to_owned(),
            tau_proto::SecretValue::new(secret.to_string()),
        )]),
        settings_files: Default::default(),
    }
}

/// Run one invocation after exhausting tau-client's real detached output FIFO.
fn saturated_terminal(invoke: tau_proto::ToolStarted, cancel: bool) -> Vec<Event> {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let secret = RostraIdSecretKey::generate();
    let publication_gate = cancel.then(|| pause_before_test_publication(invoke.call_id.clone()));
    let (extension_input, harness_input) = UnixStream::pair().expect("input stream pair");
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let writer_gate = Arc::new((Mutex::new(true), Condvar::new()));
    let (entered_tx, entered_rx) = mpsc::channel();
    let (overloaded_tx, overloaded_rx) = mpsc::channel();
    MandatoryOutput::install_saturation_notify(invoke.call_id.clone(), overloaded_tx);
    let output_bytes = Arc::clone(&bytes);
    let output_gate = Arc::clone(&writer_gate);
    let runner = thread::spawn(move || {
        run(
            extension_input,
            SaturationWriter {
                bytes: output_bytes,
                gate: output_gate,
                entered: entered_tx,
                announced: false,
            },
        )
        .map_err(|error| error.to_string())
    });
    let mut input = HarnessOutputWriter::new(harness_input);
    input
        .write_message(&HarnessOutputMessage::Configure(production_config(
            temporary.path().join("state"),
            secret,
        )))
        .expect("configure Rostra");
    input
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(
            invoke.clone(),
        )))
        .expect("start tool");
    input.flush().expect("flush tool");
    if let Some(gate) = publication_gate.as_ref() {
        gate.entered
            .recv_timeout(TEST_GATE_WAIT)
            .expect("cancelled publication reaches held boundary");
        input
            .write_message(&HarnessOutputMessage::deliver(Event::ToolCancelRequest(
                ToolCancelRequest {
                    target_call_id: invoke.call_id.clone(),
                },
            )))
            .expect("cancel held tool");
        input.flush().expect("flush cancellation");
    }
    entered_rx
        .recv_timeout(TEST_GATE_WAIT)
        .expect("optional output reached blocked production writer");
    overloaded_rx
        .recv_timeout(TEST_GATE_WAIT)
        .expect("real detached FIFO reached overload");
    MandatoryOutput::clear_saturation_notify();
    let (lock, condvar) = &*writer_gate;
    *lock.lock().expect("writer gate") = false;
    condvar.notify_all();
    wait_for_bytes_event(&bytes, |event| match event {
        Event::ToolResultReported(result) => result.call_id == invoke.call_id,
        Event::ToolErrorReported(error) => error.call_id == invoke.call_id,
        Event::ToolCancelledReported(cancelled) => cancelled.call_id == invoke.call_id,
        _ => false,
    });
    if let Some(gate) = publication_gate {
        gate.release
            .send(())
            .expect("release cancelled publication");
        gate.committed
            .recv_timeout(TEST_GATE_WAIT)
            .expect("cancelled publication commits once");
    }
    drop(input);
    runner
        .join()
        .expect("Rostra runner thread")
        .expect("checked terminal survives saturation");
    events_from_bytes(&bytes.lock().expect("output bytes"))
}

/// Decode complete events from captured protocol bytes.
fn events_from_bytes(bytes: &[u8]) -> Vec<Event> {
    let mut reader = HarnessInputReader::new(bytes);
    let mut events = Vec::new();
    while let Ok(Some(message)) = reader.read_message() {
        if let HarnessInputMessage::Emit(emit) = message {
            events.push(*emit.event);
        }
    }
    events
}

/// Wait until captured protocol bytes contain one expected event.
fn wait_for_bytes_event(bytes: &Mutex<Vec<u8>>, predicate: impl Fn(&Event) -> bool) {
    let deadline = Instant::now() + TEST_GATE_WAIT;
    loop {
        if events_from_bytes(&bytes.lock().expect("output bytes"))
            .iter()
            .any(&predicate)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for captured event"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

/// Ensures result, error, and cancellation terminals use checked ordered output
/// and remain exact after actual production detached-FIFO exhaustion.
#[test]
fn mandatory_terminals_survive_production_fifo_saturation() {
    let _fixture = SIGNED_PUBLICATION_FIXTURE_LOCK
        .lock()
        .expect("signed publication fixture lock");
    let cases = [
        (
            signed_invoke(STATUS_TOOL, serde_json::json!({})),
            false,
            "result",
        ),
        (
            signed_invoke(READ_TOOL, serde_json::json!({})),
            false,
            "error",
        ),
        (
            signed_invoke(
                POST_TOOL,
                serde_json::json!({"body":"cancel under saturation"}),
            ),
            true,
            "cancelled",
        ),
    ];
    for (mut invoke, cancel, expected) in cases {
        invoke.call_id = tau_proto::ToolCallId::new(format!("saturated-{expected}"));
        let events = saturated_terminal(invoke.clone(), cancel);
        let matching = events
            .iter()
            .filter(|event| match event {
                Event::ToolResultReported(result) => result.call_id == invoke.call_id,
                Event::ToolErrorReported(error) => error.call_id == invoke.call_id,
                Event::ToolCancelledReported(cancelled) => cancelled.call_id == invoke.call_id,
                _ => false,
            })
            .collect::<Vec<_>>();
        assert_eq!(matching.len(), 1, "{expected} terminal must be exact");
        assert_eq!(
            match matching[0] {
                Event::ToolResultReported(_) => "result",
                Event::ToolErrorReported(_) => "error",
                Event::ToolCancelledReported(_) => "cancelled",
                _ => unreachable!("filtered terminal"),
            },
            expected
        );
    }
}

/// Ensures malformed and busy reconfiguration reject only the candidate config:
/// the active call keeps ownership, publishes its original exact completion,
/// and the previous client remains usable afterward.
#[test]
fn rejected_configuration_preserves_active_call_and_previous_client() {
    let _fixture = SIGNED_PUBLICATION_FIXTURE_LOCK
        .lock()
        .expect("signed publication fixture lock");
    let temporary = tempfile::tempdir().expect("temporary directory");
    let secret = RostraIdSecretKey::generate();
    let (extension_input, harness_input) = UnixStream::pair().expect("input stream pair");
    let output = SharedWriter::default();
    let runner_output = output.clone();
    let runner = thread::spawn(move || {
        run(extension_input, runner_output).map_err(|error| error.to_string())
    });
    let mut input = HarnessOutputWriter::new(harness_input);
    input
        .write_message(&HarnessOutputMessage::Configure(production_config(
            temporary.path().join("state"),
            secret,
        )))
        .expect("configure Rostra");
    let mut held = signed_invoke(POST_TOOL, serde_json::json!({"body":"original completion"}));
    held.call_id = tau_proto::ToolCallId::new("held-during-config-rejection");
    let gate = pause_before_test_publication(held.call_id.clone());
    input
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(
            held.clone(),
        )))
        .expect("start held call");
    input.flush().expect("flush held call");
    gate.entered
        .recv_timeout(TEST_GATE_WAIT)
        .expect("held call reaches publication boundary");

    let mut malformed = production_config(temporary.path().join("malformed"), secret);
    malformed.config = tau_proto::json_to_cbor(&serde_json::json!({
        "identity_mnemonic_secret": 7,
    }));
    input
        .write_message(&HarnessOutputMessage::Configure(malformed))
        .expect("send malformed reconfiguration");
    input
        .write_message(&HarnessOutputMessage::Configure(production_config(
            temporary.path().join("busy-reconfiguration"),
            RostraIdSecretKey::generate(),
        )))
        .expect("send busy reconfiguration");
    input.flush().expect("flush rejected configurations");
    let deadline = Instant::now() + TEST_GATE_WAIT;
    while output_messages(&output)
        .iter()
        .filter(|message| matches!(message, HarnessInputMessage::ConfigError(_)))
        .count()
        < 2
    {
        assert!(
            Instant::now() < deadline,
            "both rejected configurations must report ConfigError"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert!(
        !output_events(&output).iter().any(|event| match event {
            Event::ToolResultReported(result) => result.call_id == held.call_id,
            Event::ToolErrorReported(error) => error.call_id == held.call_id,
            Event::ToolCancelledReported(cancelled) => cancelled.call_id == held.call_id,
            _ => false,
        }),
        "configuration rejection must not terminalize the active call"
    );

    gate.release.send(()).expect("release original call");
    gate.committed
        .recv_timeout(TEST_GATE_WAIT)
        .expect("original call commits once");
    wait_for_output_event(&output, |event| {
        matches!(
            event,
            Event::ToolResultReported(result) if result.call_id == held.call_id
        )
    });
    let mut status = signed_invoke(STATUS_TOOL, serde_json::json!({}));
    status.call_id = tau_proto::ToolCallId::new("status-after-config-rejection");
    input
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(
            status.clone(),
        )))
        .expect("query preserved client");
    input.flush().expect("flush preserved-client query");
    wait_for_output_event(&output, |event| {
        matches!(
            event,
            Event::ToolResultReported(result) if result.call_id == status.call_id
        )
    });
    drop(input);
    runner
        .join()
        .expect("Rostra runner thread")
        .expect("Rostra runner");
    let events = output_events(&output);
    let held_results = events
        .iter()
        .filter_map(|event| match event {
            Event::ToolResultReported(result) if result.call_id == held.call_id => Some(result),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(held_results.len(), 1);
    let tau_proto::CborValue::Text(held_text) = &held_results[0].result else {
        panic!("write result text");
    };
    let held_payload: serde_json::Value =
        serde_json::from_str(held_text).expect("write result JSON");
    assert_eq!(held_payload["identity"], secret.id().to_string());
    assert_eq!(held_payload["operation"], "post");
    assert_eq!(held_payload["local_state"], "stored");
    assert_eq!(held_payload["publication"], "asynchronous_best_effort");
    let status_result = events
        .iter()
        .find_map(|event| match event {
            Event::ToolResultReported(result) if result.call_id == status.call_id => Some(result),
            _ => None,
        })
        .expect("status result from preserved client");
    let tau_proto::CborValue::Text(status_text) = &status_result.result else {
        panic!("status result text");
    };
    assert!(status_text.contains(&format!("identity: {}", secret.id())));
    assert!(status_text.contains("database: open"));
}

/// Ensures a worker-side mandatory terminal failure escapes the protocol loop
/// without falsely publishing the terminal; harness integration owns subsequent
/// disconnect settlement.
#[test]
fn mandatory_terminal_failure_exits_for_disconnect_cleanup() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let secret = RostraIdSecretKey::generate();
    let (extension_input, harness_input) = UnixStream::pair().expect("input stream pair");
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let failed = Arc::new(AtomicBool::new(false));
    let runner_bytes = Arc::clone(&bytes);
    let runner_failed = Arc::clone(&failed);
    let (done_tx, done_rx) = mpsc::channel();
    thread::spawn(move || {
        let result = run(
            extension_input,
            MandatoryFailureWriter {
                bytes: runner_bytes,
                ready: false,
                failed: runner_failed,
            },
        )
        .map_err(|error| error.to_string());
        let _ = done_tx.send(result);
    });
    let mut input = HarnessOutputWriter::new(harness_input);
    input
        .write_message(&HarnessOutputMessage::Configure(production_config(
            temporary.path().join("state"),
            secret,
        )))
        .expect("configure Rostra");
    let mut invoke = signed_invoke(STATUS_TOOL, serde_json::json!({}));
    invoke.call_id = tau_proto::ToolCallId::new("mandatory-failure-retained");
    input
        .write_message(&HarnessOutputMessage::deliver(Event::ToolStarted(
            invoke.clone(),
        )))
        .expect("start tool");
    input.flush().expect("flush tool");
    let result = done_rx
        .recv_timeout(TEST_GATE_WAIT)
        .expect("mandatory failure must terminate the extension loop");
    assert!(result.is_err());
    assert!(failed.load(Ordering::Acquire));
    assert!(
        !events_from_bytes(&bytes.lock().expect("output bytes"))
            .iter()
            .any(|event| matches!(
                event,
                Event::ToolResultReported(result) if result.call_id == invoke.call_id
            )),
        "a failed flush cannot release ownership as a published terminal"
    );
}
