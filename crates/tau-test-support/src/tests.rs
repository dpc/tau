use std::os::unix::net::UnixStream;
use std::sync::{Arc, Barrier};
use std::{io, thread};

use socket2::{Domain, Socket, Type};

use super::*;

/// Malformed JSON must fail the causal trace instead of silently dropping a
/// lifecycle record and weakening cardinality assertions.
#[test]
fn causal_trace_parser_rejects_malformed_line() {
    let error = parse_published_trace_events("{not-json")
        .expect_err("malformed causal trace must fail closed");
    assert!(matches!(error, CausalQuotaError::TraceJson { line: 1, .. }));
}

/// Published records without decodable events fail while valid diagnostics are
/// ignored.
#[test]
fn causal_trace_parser_rejects_invalid_published_payload() {
    let raw = concat!(
        "{\"type\":\"disconnected\"}\n",
        "{\"type\":\"published\",\"event\":{\"type\":\"not-an-event\"}}\n"
    );
    let error =
        parse_published_trace_events(raw).expect_err("invalid published payload must fail closed");
    assert!(matches!(
        error,
        CausalQuotaError::TraceEvent { line: 2, .. }
    ));
}

/// Full prompts use a bounded debug-only summary rather than a round-trippable
/// protocol event and are intentionally absent from causal event fixtures.
#[test]
fn causal_trace_parser_skips_full_prompt_debug_summary() {
    let raw = concat!(
        "{\"type\":\"published\",\"event_name\":\"agent.prompt_created\",",
        "\"event\":{\"event\":\"agent.prompt_created\",\"payload\":{\"summary\":{}}}}\n"
    );
    assert!(
        parse_published_trace_events(raw)
            .expect("bounded prompt summary")
            .is_empty()
    );
}

/// Malformed record shapes must fail closed so non-object rows, invalid
/// discriminators, and missing published events cannot silently weaken causal
/// trace assertions.
#[test]
fn causal_trace_parser_rejects_invalid_record_shapes() {
    for raw in ["{}", "null", "{\"type\":7}", "{\"type\":\"published\"}"] {
        assert!(
            matches!(
                parse_published_trace_events(raw),
                Err(CausalQuotaError::TraceShape { line: 1, .. })
            ),
            "unexpected acceptance for {raw}"
        );
    }
}

/// Runtime construction keeps every test's config, state, socket, and store
/// roots inside a distinct temporary directory.
#[test]
fn runtime_construction_isolates_paths_and_stores() {
    let runtime = TestRuntime::new().expect("runtime should be created");
    let other_runtime = TestRuntime::new().expect("second runtime should be created");
    let root = runtime._tempdir.path();
    let config_dir = runtime
        .dirs
        .config_dir
        .as_deref()
        .expect("runtime should configure an isolated config directory");

    assert_eq!(runtime.socket_path.parent(), Some(root));
    assert!(runtime.state_dir.starts_with(root));
    assert!(config_dir.starts_with(root));
    assert_ne!(runtime.socket_path, runtime.state_dir);
    assert_ne!(runtime.socket_path, config_dir);
    assert_ne!(runtime.state_dir, config_dir);
    assert_ne!(runtime.socket_path, other_runtime.socket_path);
    assert_ne!(runtime.state_dir, other_runtime.state_dir);

    runtime
        .open_session_store()
        .expect("isolated session store should open");
    runtime
        .open_agent_store()
        .expect("isolated agent store should open");
    assert!(runtime.state_dir.join("sessions").is_dir());
    assert!(runtime.state_dir.join("agents").is_dir());
}

/// The public embedded helper must run the deterministic echo path and return
/// the submitted message without requiring a daemon socket.
#[test]
fn runtime_embedded_echoes_message() {
    let runtime = TestRuntime::new().expect("runtime should be created");

    let response = runtime
        .run_embedded("embedded-session", "embedded hello")
        .expect("embedded echo should succeed");

    assert_eq!(response, "embedded hello");
}

/// One bounded daemon client must observe readiness, receive its echo, and let
/// the caller join the daemon thread after the configured client limit.
#[test]
fn runtime_daemon_echoes_one_client_and_joins() {
    let runtime = TestRuntime::new().expect("runtime should be created");
    let daemon = runtime
        .spawn_daemon("daemon-session", Some(1))
        .expect("daemon listener should bind");

    let response = runtime
        .send_daemon_message("daemon-session", "daemon hello")
        .expect("daemon echo should succeed");

    assert_eq!(response, "daemon hello");
    daemon
        .join()
        .expect("one-client daemon should exit cleanly");
}

/// Filesystem socket publication is not listener readiness: under concurrent
/// startup, every worker holds a stream socket after `bind` but before
/// `listen`, where the former path-existence predicate would pass and every
/// client gets `ConnectionRefused`. Each worker then starts a pre-bound test
/// daemon and proves the new ownership boundary accepts its exact single client
/// and joins.
#[test]
fn daemon_listener_readiness_survives_forced_pre_listen_contention() {
    const WORKERS: usize = 16;

    let pre_listen_sockets = (0..WORKERS)
        .map(|worker| {
            let tempdir =
                tempfile::TempDir::new().expect("temporary socket root should be created");
            let path = tempdir.path().join(format!("pre-listen-{worker}.sock"));
            let socket = Socket::new(Domain::UNIX, Type::STREAM, None)
                .expect("stream socket should be created");
            socket
                .bind(&socket2::SockAddr::unix(&path).expect("socket path should be valid"))
                .expect("stream socket should bind without listening");
            (tempdir, path, socket)
        })
        .collect::<Vec<_>>();
    let pre_listen_start = Arc::new(Barrier::new(WORKERS));
    let pre_listen_workers = pre_listen_sockets
        .into_iter()
        .map(|(tempdir, path, socket)| {
            let pre_listen_start = Arc::clone(&pre_listen_start);
            thread::spawn(move || {
                pre_listen_start.wait();
                assert!(
                    path.exists(),
                    "bound socket path was not published: {}",
                    path.display()
                );
                let error = UnixStream::connect(&path)
                    .expect_err("a bound but unlistening socket must refuse a stream client");
                assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
                drop(socket);
                std::fs::remove_file(&path).expect("unlistening socket path should be removed");
                drop(tempdir);
            })
        })
        .collect::<Vec<_>>();
    for worker in pre_listen_workers {
        worker
            .join()
            .expect("pre-listen contention worker should not panic");
    }

    let daemons = (0..WORKERS)
        .map(|worker| {
            let session_id = format!("daemon-{worker}");
            let runtime = TestRuntime::new().expect("runtime should be created");
            let daemon = runtime
                .spawn_daemon(&session_id, Some(1))
                .expect("daemon listener should bind");
            (runtime, daemon, session_id)
        })
        .collect::<Vec<_>>();
    let daemon_start = Arc::new(Barrier::new(WORKERS));
    let daemon_workers = daemons
        .into_iter()
        .map(|(runtime, daemon, session_id)| {
            let daemon_start = Arc::clone(&daemon_start);
            thread::spawn(move || {
                daemon_start.wait();
                let response = runtime
                    .send_daemon_message(&session_id, "daemon hello")
                    .expect("daemon echo should succeed");
                assert_eq!(response, "daemon hello");
                daemon
                    .join()
                    .expect("one-client daemon should exit cleanly");
            })
        })
        .collect::<Vec<_>>();
    for worker in daemon_workers {
        worker
            .join()
            .expect("daemon contention worker should not panic");
    }
}

/// Ensures panic payload formatting keeps useful string payloads instead of
/// replacing them with an opaque daemon-thread join error message.
#[test]
fn panic_payload_label_reports_string_payloads() {
    let static_str_payload: Box<dyn std::any::Any + Send> = Box::new("static panic");
    assert_eq!(panic_payload_label(&*static_str_payload), "static panic");

    let string_payload: Box<dyn std::any::Any + Send> = Box::new(String::from("owned panic"));
    assert_eq!(panic_payload_label(&*string_payload), "owned panic");
}
