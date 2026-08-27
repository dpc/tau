use std::io::{BufReader, Read, Write};
use std::os::unix as path_std_os_unix;
use std::os::unix::net::UnixStream;
use std::process::Command as path_std_process_Command;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};
use std::{collections as path_std_collections, fs, io as path_std_io, thread};

use tau_config::settings::TauDirs;
use tau_proto::{HarnessOutputMessage, PeerInputReader};
use tempfile::TempDir;

use super::*;
use crate::harness::Harness;

/// Ensures the embedded-options seam applies its explicit runtime parent and
/// restores ambient and nested roots after normal return and early exit.
#[test]
fn embedded_runtime_directory_scope_restores_after_return_and_early_exit() {
    let ambient = crate::runtime_dir::root_runtime_dir();
    let outer = TempDir::new().expect("outer runtime root");
    let inner = TempDir::new().expect("inner runtime root");
    let options = EmbeddedOptions::builder()
        .runtime_dir(outer.path().to_path_buf())
        .build();

    with_embedded_runtime_dir(options.clone(), |_| {
        assert_eq!(
            crate::runtime_dir::root_runtime_dir(),
            outer.path().join("tau")
        );
        crate::runtime_dir::with_runtime_dir(Some(inner.path()), || {
            assert_eq!(
                crate::runtime_dir::root_runtime_dir(),
                inner.path().join("tau")
            );
        });
        assert_eq!(
            crate::runtime_dir::root_runtime_dir(),
            outer.path().join("tau")
        );
    });
    assert_eq!(crate::runtime_dir::root_runtime_dir(), ambient);

    let early_exit = with_embedded_runtime_dir(options, |_| {
        crate::runtime_dir::with_runtime_dir(Some(inner.path()), || {
            Err::<(), _>("leave nested runtime scope")
        })?;
        Ok::<(), &str>(())
    });
    assert_eq!(early_exit, Err("leave nested runtime scope"));
    assert_eq!(crate::runtime_dir::root_runtime_dir(), ambient);
}

/// Ensures the reusable embedded echo fixture does not discover caller-owned
/// skills through the in-process shell extension.
#[test]
fn embedded_echo_fixture_ignores_ambient_discovery() {
    let ambient_home = TempDir::new().expect("ambient home");
    let ambient_skill_dir = ambient_home
        .path()
        .join(".config/agents/skills/ambient-shell-skill");
    fs::create_dir_all(&ambient_skill_dir).expect("ambient skill directory");
    fs::write(
        ambient_skill_dir.join("SKILL.md"),
        "---\nname: ambient-shell-skill\ndescription: ambient fixture\n---\n",
    )
    .expect("ambient skill");

    let output = path_std_process_Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--ignored",
            "--exact",
            "daemon::tests::embedded_echo_fixture_ignores_ambient_discovery_child",
        ])
        .env("HOME", ambient_home.path())
        .env("XDG_CONFIG_HOME", ambient_home.path().join(".config"))
        .output()
        .expect("run isolated fixture child");
    assert!(
        output.status.success(),
        "fixture child failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Exercises the public echo fixture with an ambient skill that would otherwise
/// enable the configured role.
#[test]
#[ignore = "run only through the isolated parent regression"]
fn embedded_echo_fixture_ignores_ambient_discovery_child() {
    let temp = TempDir::new().expect("fixture root");
    let config_dir = temp.path().join("config");
    fs::create_dir_all(&config_dir).expect("fixture config directory");
    fs::write(
        config_dir.join("harness.yaml"),
        r#"
agents:
  default_role: ambient-role
  role_groups:
    fixture:
      roles:
        ambient-role:
          required_skills: [ambient-shell-skill]
"#,
    )
    .expect("fixture harness configuration");

    let error = run_embedded_message_with_echo(temp.path(), "fixture-session", "hello")
        .expect_err("fixture must not discover the ambient skill");
    assert!(error.to_string().contains("role `ambient-role` disabled"));
}

/// Runtime harness startup captures one canonical directory identity so symlink
/// aliases and parent components cannot leak into live-session responses.
#[test]
fn project_root_identity_is_canonical_and_requires_a_directory() {
    let temp = TempDir::new().expect("tempdir");
    let project = temp.path().join("project");
    let alias = temp.path().join("alias");
    let file = temp.path().join("file");
    fs::create_dir(&project).expect("project directory");
    path_std_os_unix::fs::symlink(&project, &alias).expect("project symlink");
    fs::write(&file, b"not a directory").expect("test file");

    assert_eq!(
        canonical_project_root(&alias).expect("canonical project root"),
        project.canonicalize().expect("canonical project directory")
    );
    assert!(
        canonical_project_root(&file)
            .expect_err("file project root")
            .to_string()
            .contains("not a directory")
    );
}

/// Protects the component startup source boundary: direct entrypoints ignore
/// inherited private transport while only spawned initial-UI children consume
/// it.
#[test]
fn component_launch_selects_private_transport_only_for_spawned_child() {
    assert!(
        !ComponentLaunch::Direct(Vec::new()).uses_spawned_transport(),
        "bare/direct harness entrypoints use typed overrides"
    );
    assert!(ComponentLaunch::SpawnedInitialUiStdio.uses_spawned_transport());
    let malformed = Some("not-json".into());
    let direct = vec![tau_config::settings::ExtensionCliOverride::Disable(
        "std-pim".to_owned(),
    )];
    assert_eq!(
        ComponentLaunch::Direct(direct.clone())
            .extension_overrides(malformed.clone())
            .expect("direct launch ignores inherited private transport"),
        direct
    );
    assert!(
        ComponentLaunch::SpawnedInitialUiStdio
            .extension_overrides(malformed)
            .is_err(),
        "spawned child must fail closed on malformed private transport"
    );
}

/// Locks the CLI-to-harness runtime identity boundary: spawned children consume
/// an exact valid value, self-mint when absent, and reject malformed values.
#[test]
fn spawned_component_resolves_runtime_instance_transport() {
    let launch = ComponentLaunch::SpawnedInitialUiStdio;
    let supplied = "0123456789abcdef";

    assert_eq!(
        launch
            .runtime_instance_id(Some(supplied.into()))
            .expect("valid supplied instance")
            .as_str(),
        supplied
    );
    let minted = launch
        .runtime_instance_id(None)
        .expect("missing transport mints");
    assert_eq!(minted.as_str().len(), 16);
    assert!(
        minted
            .as_str()
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    );
    assert!(
        launch
            .runtime_instance_id(Some("not-an-instance".into()))
            .is_err(),
        "malformed supplied identities must fail closed"
    );
}

/// Ensures direct harness components always self-mint instead of consuming a
/// private identity inherited from an unrelated CLI-managed spawn.
#[test]
fn direct_component_self_mints_runtime_instance() {
    let inherited = "not-an-instance";
    let instance = ComponentLaunch::Direct(Vec::new())
        .runtime_instance_id(Some(inherited.into()))
        .expect("direct launch instance");

    assert_eq!(instance.as_str().len(), 16);
    assert!(
        instance
            .as_str()
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    );
}

/// Ensures the synchronous daemon-message helper subscribes only to the
/// concrete trace events it consumes, instead of receiving every future event
/// in broad protocol categories.
#[test]
fn daemon_message_trace_subscription_uses_no_prefix_selectors() {
    let selectors = daemon_message_event_selectors();

    let expected = [
        EventName::AGENT_PROMPT_CREATED,
        EventName::UI_CREATE_AGENT_RESULT,
        EventName::AGENT_PROMPT_FAILED,
        EventName::PROVIDER_RESPONSE_FINISHED,
        EventName::TOOL_PROGRESS,
        EventName::SHELL_COMMAND_PROGRESS,
        EventName::HARNESS_NOTICE,
        EventName::EXTENSION_STARTING,
        EventName::EXTENSION_READY,
        EventName::EXTENSION_EXITED,
        EventName::EXTENSION_RESTARTING,
    ]
    .into_iter()
    .map(EventSelector::Exact)
    .collect::<Vec<_>>();

    assert_eq!(selectors, expected);
}

/// Daemon trace correlation binds the first created-agent prompt only, rejects
/// a foreign agent, and accepts later counters in the bound agent's prompt
/// chain.
#[test]
fn daemon_trace_correlation_requires_created_agent_and_binds_once() {
    let main = tau_proto::AgentId::parse("main").expect("agent id");
    let other = tau_proto::AgentId::parse("other").expect("agent id");
    let mut lifecycle = Vec::new();
    let mut progress = Vec::new();
    let mut prompt_index = None;
    let mut created_agent_id = Some(main.clone());
    let mut state = DaemonTraceState {
        ctx_id: "prompt-1",
        lifecycle_messages: &mut lifecycle,
        progress_messages: &mut progress,
        our_spid_counter: &mut prompt_index,
        created_agent_id: &mut created_agent_id,
    };
    let make_prompt =
        |agent_id: tau_proto::AgentId, prompt_id: &str| tau_proto::AgentPromptCreated {
            agent_prompt_id: prompt_id.parse().expect("prompt id"),
            agent_id,
            session_id: tau_proto::SessionId::parse("session-1").expect("session id"),
            system_prompt: String::new(),
            context: tau_proto::PromptContext::default(),
            tools: Vec::new(),
            tools_ref: None,
            model: "test/model".parse().expect("model id"),
            model_params: tau_proto::ModelParams::default(),
            tool_choice: Default::default(),
            originator: tau_proto::PromptOriginator::User,
            share_user_cache_key: false,
            ctx_id: Some("prompt-1".to_owned()),
            compaction: None,
            operation: tau_proto::PromptOperation::Inference,
        };
    state.bind_prompt(&make_prompt(other.clone(), "ap-other-9"));
    assert_eq!(*state.our_spid_counter, None);
    state.bind_prompt(&make_prompt(main.clone(), "ap-main-2"));
    state.bind_prompt(&make_prompt(main.clone(), "ap-main-7"));
    assert_eq!(*state.our_spid_counter, Some(2));

    let make_finished =
        |agent_id: tau_proto::AgentId, prompt_id: &str| tau_proto::ProviderResponseFinished {
            automatic_compaction_decision: None,
            agent_prompt_id: prompt_id.parse().expect("prompt id"),
            agent_id,
            output_items: Vec::new(),
            stop_reason: tau_proto::ProviderStopReason::EndTurn,
            error: None,
            failure_kind: None,
            context_limit_telemetry: None,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            output_length_disposition: tau_proto::OutputLengthDisposition::None,
            originator: tau_proto::PromptOriginator::User,
            usage: None,
            estimated_api_cost_rates: None,
            estimated_api_cost_increment: None,
            compaction_original_input_tokens: None,
            compaction_output_tokens: None,
            backend: None,
            provider_attempt: Default::default(),
            provider_response_id: None,
            ws_pool_delta: None,
        };
    assert!(!state.owns_finished(&make_finished(other, "ap-other-99")));
    assert!(state.owns_finished(&make_finished(main, "ap-main-3")));
}

/// Ensures dropping the listener forwarder wakes an idle poll/accept wait
/// without forwarding the internal wake endpoint as a harness client.
#[test]
fn listener_forwarder_drop_wakes_idle_accept() {
    let td = TempDir::new().expect("tempdir");
    let socket_path = td.path().join("daemon.sock");
    let listener = bind_listener(&socket_path).expect("bind listener");
    let (forwarder, rx) = spawn_waiting_test_forwarder(&listener);

    drop_forwarder_with_timeout(forwarder);

    assert!(
        rx.try_recv().is_err(),
        "wake endpoint must not be delivered as a daemon client"
    );
}

/// Ensures listener forwarder shutdown uses its owned wake endpoint rather than
/// relying on the daemon socket pathname to remain present.
#[test]
fn listener_forwarder_drop_wakes_after_socket_path_removed() {
    let td = TempDir::new().expect("tempdir");
    let socket_path = td.path().join("daemon.sock");
    let listener = bind_listener(&socket_path).expect("bind listener");
    let (forwarder, rx) = spawn_waiting_test_forwarder(&listener);

    fs::remove_file(&socket_path).expect("remove daemon socket path");
    drop_forwarder_with_timeout(forwarder);

    assert!(
        rx.try_recv().is_err(),
        "wake endpoint must not be delivered as a daemon client"
    );
}

/// Ensures listener forwarder shutdown is tied to the owned accept thread even
/// when another listener later occupies the same daemon socket pathname.
#[test]
fn listener_forwarder_drop_wakes_after_socket_path_replaced() {
    let td = TempDir::new().expect("tempdir");
    let socket_path = td.path().join("daemon.sock");
    let listener = bind_listener(&socket_path).expect("bind listener");
    let (forwarder, rx) = spawn_waiting_test_forwarder(&listener);

    fs::remove_file(&socket_path).expect("remove daemon socket path");
    let _replacement = bind_listener(&socket_path).expect("bind replacement listener");
    drop_forwarder_with_timeout(forwarder);

    assert!(
        rx.try_recv().is_err(),
        "wake endpoint must not be delivered as a daemon client"
    );
}

/// Ensures shutdown wakeup wins over a listener that is already receiving
/// clients, so lifecycle control cannot be starved by accept draining.
#[test]
fn listener_forwarder_drop_wakes_while_accept_ready() {
    let td = TempDir::new().expect("tempdir");
    let socket_path = td.path().join("daemon.sock");
    let listener = bind_listener(&socket_path).expect("bind listener");
    let (forwarder, rx) = spawn_waiting_test_forwarder(&listener);
    let (stop_tx, stop_rx) = mpsc::channel();
    let connector_path = socket_path.clone();
    let connector = thread::spawn(move || {
        while stop_rx.try_recv().is_err() {
            let _ = UnixStream::connect(&connector_path);
        }
    });
    rx.recv_timeout(Duration::from_secs(1))
        .expect("forwarder should accept at least one traffic client");

    let (done_tx, done_rx) = mpsc::channel();
    let drop_join = thread::spawn(move || {
        drop(forwarder);
        let _ = done_tx.send(());
    });
    let drop_result = done_rx.recv_timeout(Duration::from_secs(1));
    let _ = stop_tx.send(());
    connector.join().expect("connector should not panic");
    drop_result.expect("forwarder drop should not wait for accept traffic to quiesce");
    drop_join.join().expect("drop thread should not panic");
}

fn spawn_waiting_test_forwarder(
    listener: &SocketListener,
) -> (ListenerForwarder, mpsc::Receiver<HarnessEvent>) {
    let (tx, rx) = mpsc::channel();
    let (before_wait_tx, before_wait_rx) = mpsc::channel();
    let forwarder = ListenerForwarder::spawn_for_test(
        listener.try_clone_raw_listener().expect("clone listener"),
        tx,
        before_wait_tx,
    )
    .expect("spawn listener forwarder");
    before_wait_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("forwarder should reach poll wait");
    (forwarder, rx)
}

fn drop_forwarder_with_timeout(forwarder: ListenerForwarder) {
    let (done_tx, done_rx) = mpsc::channel();
    let join = thread::spawn(move || {
        drop(forwarder);
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("forwarder drop should wake and join accept thread");
    join.join().expect("drop thread should not panic");
}

/// Ensures startup failures are reported to the initial UI through the Tau
/// protocol, rather than requiring the UI to scrape harness stderr logs.
#[test]
fn startup_error_is_sent_as_protocol_disconnect() {
    let (harness_end, ui_end) = UnixStream::pair().expect("stream pair");
    let temp = TempDir::new().expect("temporary directory");
    let failing_parent = temp.path().join("regular-file-parent");
    std::fs::write(&failing_parent, b"not a directory").expect("create failing parent");
    let metadata_path = failing_parent.join("daemon.json");
    let error = tau_util_fs_err::write(&metadata_path, b"metadata")
        .expect_err("metadata write below a regular file must fail");

    send_initial_client_startup_error(
        Some(InitialClientStartupErrorOutput::Stream(harness_end)),
        &error,
    );

    let mut reader = PeerInputReader::new(BufReader::new(ui_end));
    let message = reader
        .read_message()
        .expect("read disconnect frame")
        .expect("disconnect frame");
    let HarnessOutputMessage::Disconnect(disconnect) = message else {
        panic!("expected disconnect frame");
    };
    let reason = disconnect.reason.expect("disconnect reason");
    assert!(reason.contains("harness startup failed"));
    assert!(reason.contains("failed to create file"));
    assert!(reason.contains(&metadata_path.display().to_string()));
}

/// Ensures daemon-owned startup failures after the initial UI has been accepted
/// are routed through the normal connection writer and flushed before the
/// process can exit, rather than falling back to EOF or racing a side-channel
/// write.
#[test]
fn post_accept_startup_error_is_sent_through_normal_writer() {
    fn echo_runner(r: UnixStream, w: UnixStream) -> Result<(), String> {
        crate::harness::run_echo_provider(r, w).map_err(|e| e.to_string())
    }

    let td = TempDir::new().expect("tempdir");
    let state_dir = td.path().join("state");
    let dirs = TauDirs {
        config_dir: Some(td.path().join("config")),
        state_dir: Some(td.path().join("runtime")),
    };
    let mut harness = Harness::new_with_provider(
        &state_dir,
        dirs,
        echo_runner,
        echo_tools(),
        "s1",
        tau_proto::SessionStartReason::Initial,
        crate::HarnessStorageMode::Durable,
    )
    .expect("harness");
    let (server_end, ui_end) = UnixStream::pair().expect("stream pair");
    let client_id = harness.accept_client(server_end).expect("accept client");
    let mut pre_accept_stream = None;

    let result = notify_startup_error_after_accept::<(), _>(
        Err(path_std_io::Error::other("marker write failed")),
        &mut pre_accept_stream,
        &mut harness,
        Some(&client_id),
    );

    assert!(result.is_err());
    drop(harness);
    let mut reader = PeerInputReader::new(BufReader::new(ui_end));
    let message = reader
        .read_message()
        .expect("read disconnect frame")
        .expect("disconnect frame");
    let HarnessOutputMessage::Disconnect(disconnect) = message else {
        panic!("expected disconnect frame");
    };
    let reason = disconnect.reason.expect("disconnect reason");
    assert!(reason.contains("harness startup failed"));
    assert!(reason.contains("marker write failed"));
    assert!(
        reader.read_message().expect("read terminal EOF").is_none(),
        "a complete queued Disconnect remains readable before socket EOF"
    );
}

/// Ensures a fatal startup response cannot retain a writer, reader, cursor, and
/// transport forever when the accepted Unix-socket client does not read.
#[test]
fn post_accept_startup_error_cancels_a_blocked_socket_writer() {
    fn echo_runner(r: UnixStream, w: UnixStream) -> Result<(), String> {
        crate::harness::run_echo_provider(r, w).map_err(|e| e.to_string())
    }

    let td = TempDir::new().expect("tempdir");
    let state_dir = td.path().join("state");
    let dirs = TauDirs {
        config_dir: Some(td.path().join("config")),
        state_dir: Some(td.path().join("runtime")),
    };
    let mut harness = Harness::new_with_provider(
        &state_dir,
        dirs,
        echo_runner,
        echo_tools(),
        "s1",
        tau_proto::SessionStartReason::Initial,
        crate::HarnessStorageMode::Durable,
    )
    .expect("harness");
    let (mut server_end, mut ui_end) = UnixStream::pair().expect("stream pair");

    server_end
        .set_nonblocking(true)
        .expect("make fill writes nonblocking");
    let fill = [0_u8; 8 * 1024];
    loop {
        match server_end.write(&fill) {
            Ok(0) => panic!("socket fill made no progress"),
            Ok(_) => {}
            Err(error) if error.kind() == path_std_io::ErrorKind::WouldBlock => break,
            Err(error) => panic!("fill client receive queue: {error}"),
        }
    }
    loop {
        match server_end.write(&[0]) {
            Ok(1) => {}
            Ok(written) => panic!("single-byte socket fill wrote {written} bytes"),
            Err(error) if error.kind() == path_std_io::ErrorKind::WouldBlock => break,
            Err(error) => panic!("exhaust final client receive-queue capacity: {error}"),
        }
    }
    server_end
        .set_nonblocking(false)
        .expect("restore blocking writes");

    let event_log = Arc::clone(&harness.runtime_io.event_log);
    let baseline_consumers = event_log.consumer_count();
    let client_id = harness.accept_client(server_end).expect("accept client");
    assert_eq!(event_log.consumer_count(), baseline_consumers + 1);
    let mut pre_accept_stream = None;
    let started = Instant::now();
    let result = notify_startup_error_after_accept::<(), _>(
        Err(path_std_io::Error::other("blocked marker failure")),
        &mut pre_accept_stream,
        &mut harness,
        Some(&client_id),
    );
    assert!(result.is_err());
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "fatal startup handling must not wait for the blocked socket writer"
    );
    assert!(harness.ui_runtime.client_writers.is_empty());

    let deadline = Instant::now() + Duration::from_secs(1);
    while event_log.consumer_count() != baseline_consumers && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        event_log.consumer_count(),
        baseline_consumers,
        "socket shutdown must wake the writer and retire its live cursor"
    );

    ui_end
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("set bounded peer read");
    let mut scratch = [0_u8; 16 * 1024];
    loop {
        match ui_end.read(&mut scratch) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) if error.kind() == path_std_io::ErrorKind::ConnectionReset => break,
            Err(error) => panic!("shutdown transport must reach EOF or reset: {error}"),
        }
    }
}

/// A create rejection crosses only the initiating real socket and cannot leak
/// to a second attached socket without relying on in-memory bus inspection.
#[test]
fn create_agent_rejection_isolated_to_requester_socket() {
    fn echo_runner(r: UnixStream, w: UnixStream) -> Result<(), String> {
        crate::harness::run_echo_provider(r, w).map_err(|error| error.to_string())
    }

    let td = TempDir::new().expect("tempdir");
    let mut harness = Harness::new_with_provider(
        td.path().join("state"),
        TauDirs {
            config_dir: Some(td.path().join("config")),
            state_dir: Some(td.path().join("runtime")),
        },
        echo_runner,
        echo_tools(),
        "s1",
        tau_proto::SessionStartReason::Initial,
        crate::HarnessStorageMode::Durable,
    )
    .expect("harness");
    let (requester_server, requester_client) = UnixStream::pair().expect("requester pair");
    let (observer_server, observer_client) = UnixStream::pair().expect("observer pair");
    observer_client
        .set_read_timeout(Some(Duration::from_millis(100)))
        .expect("observer timeout");
    let requester_id = harness.accept_client(requester_server).expect("requester");
    harness.accept_client(observer_server).expect("observer");

    harness
        .handle_ui_create_agent_from(
            &requester_id,
            tau_proto::UiCreateAgent {
                request_id: "socket-rejection".to_owned(),
                session_id: tau_proto::SessionId::parse("s1").expect("session id"),
                role: "missing-role".to_owned(),
                model_override: None,
                metadata: Vec::new(),
                initial_prompt: Some("never admitted".to_owned()),
                literal: false,
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                ctx_id: Some("socket-prompt".to_owned()),
                parent_agent: None,
                ephemeral: false,
            },
        )
        .expect("reject create");

    let mut requester = PeerInputReader::new(BufReader::new(requester_client));
    let message = requester
        .read_message()
        .expect("requester frame")
        .expect("requester result");
    let HarnessOutputMessage::Deliver(delivery) = message else {
        panic!("expected directed create result")
    };
    assert!(matches!(
        delivery.into_event(),
        tau_proto::Event::UiCreateAgentResult(tau_proto::UiCreateAgentResult {
            outcome: tau_proto::UiCreateAgentOutcome::Rejected { .. },
            ..
        })
    ));

    let mut observer = PeerInputReader::new(BufReader::new(observer_client));
    assert!(
        observer.read_message().is_err(),
        "observer socket must receive no directed result"
    );
}

/// Ensures pre-resolved entrypoints cannot silently accept the environment
/// bypass and enforce any exact extension allowlist before construction.
#[test]
fn pre_resolved_daemon_rejects_environment_bypass_and_enforces_allowlist() {
    let config = crate::settings::default_config();
    let bypass = ServeOptions::builder()
        .ignore_startup_environment(true)
        .build();
    assert!(validate_pre_resolved_serve_options(&bypass, &config).is_err());

    let mismatch = ServeOptions::builder()
        .allowed_extensions(path_std_collections::BTreeSet::from([
            crate::test_extension_name("not-configured"),
        ]))
        .build();
    let error = validate_pre_resolved_serve_options(&mismatch, &config)
        .expect_err("pre-resolved allowlist mismatch must fail before harness construction");
    assert!(
        error
            .to_string()
            .contains("resolved extensions differ from deterministic allowlist")
    );
}
