use std::collections::BTreeMap;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use tau_proto::{
    Configure, HarnessInputMessage, HarnessInputReader, HarnessOutputMessage, HarnessOutputWriter,
    SecretValue,
};
use tau_swarm_api::{
    CorrelationId, DeliveryOutcome, Hostname, PromptRequest, SessionId, SessionIdentity,
};
use tau_swarm_client::{Connector, ErrorKind, ExpectedPeer};
use tau_swarm_client_api::{Credential, CredentialId, Secret};
use tau_swarm_iroh::{Credentials, Server};

use super::*;

fn resolved_config() -> ResolvedConfig {
    let peer_id = iroh::SecretKey::generate().public().to_string();
    let config: ExtConfig = serde_json::from_value(serde_json::json!({
        "endpoint": {"peer_id": peer_id},
        "credential_id": "worker",
        "credential_secret": "swarm",
        "hostname": "host"
    }))
    .expect("config shape");
    config
        .resolve(&BTreeMap::from([(
            "swarm".into(),
            SecretValue::new("secret"),
        )]))
        .expect("resolved config")
}

/// Worker notices preserve UTF-8 boundaries while bounding untrusted remote
/// diagnostics.
#[test]
fn bounds_terminal_worker_error() {
    let error = "é".repeat(4 * 1024);
    let bounded = bounded_error(&error);
    assert!(bounded.len() <= 4 * 1024);
    assert!(bounded.is_char_boundary(bounded.len()));
}

/// A loaded agent remains unpublished until its replay-valid boundary; a bound
/// failure then clears all partial projection state and prevents later folds.
#[test]
fn gates_publication_on_agent_replay_and_clears_invalid_projection() {
    let mut state = SwarmRuntime::new();
    state.config = Some(resolved_config());
    let agent = tau_swarm_api::AgentId::new("agent");
    let mut draft = AgentDraft::new(&agent);
    draft.loaded = true;
    state.agents.insert(agent.clone(), draft);
    state.publish_agent(&agent).expect("pre-boundary fold");
    assert!(
        state
            .projection
            .blocking_lock()
            .snapshot()
            .snapshot
            .agents
            .is_empty()
    );

    state.agents.get_mut(&agent).expect("draft").replay_valid = true;
    state.publish_agent(&agent).expect("post-boundary fold");
    assert_eq!(
        state
            .projection
            .blocking_lock()
            .snapshot()
            .snapshot
            .agents
            .len(),
        1
    );

    state.config.as_mut().expect("config").limits.agent_entries = 0;
    fold_event(
        &mut state,
        &Event::AgentDisplayNameSet(tau_proto::AgentDisplayNameSet {
            agent_id: tau_proto::AgentId::parse("agent").expect("agent ID"),
            display_name: "Changed".into(),
        }),
    )
    .expect("bound-triggering event fold");
    assert!(!state.projection_valid);
    assert!(state.agents.is_empty());
    assert!(
        state
            .projection
            .blocking_lock()
            .snapshot()
            .snapshot
            .agents
            .is_empty()
    );
    state
        .publish_agent(&agent)
        .expect("invalid folds are ignored");
    assert!(
        state
            .projection
            .blocking_lock()
            .snapshot()
            .snapshot
            .agents
            .is_empty()
    );
    fold_event(
        &mut state,
        &Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
            session_id: "session".parse().expect("session ID"),
            agent_id: tau_proto::AgentId::parse("other").expect("agent ID"),
            agent_initialization_id: tau_proto::AgentInitializationId::parse("init")
                .expect("valid initialization id"),
            ephemeral: false,
        }),
    )
    .expect("invalid-state load is ignored");
    assert!(state.agents.is_empty());
}

/// Any replay error invalidates the whole projection rather than publishing an
/// incomplete subset.
#[test]
fn replay_error_invalidates_projection() {
    let mut state = SwarmRuntime::new();
    state.config = Some(resolved_config());
    state.session_id = Some("session".parse().expect("session ID"));
    fold_event(
        &mut state,
        &Event::AgentReplayComplete(tau_proto::AgentReplayComplete {
            agent_id: tau_proto::AgentId::parse("agent").expect("agent ID"),
            session_id: Some("session".parse().expect("session ID")),
            error: Some("corrupt replay".into()),
        }),
    )
    .expect("error is folded authoritatively");
    assert!(!state.projection_valid);
    assert!(state.agents.is_empty());
    assert!(state.worker.is_none());

    let mut session_state = SwarmRuntime::new();
    session_state.config = Some(resolved_config());
    session_state.session_id = Some("session".parse().expect("session ID"));
    fold_event(
        &mut session_state,
        &Event::SessionReplayComplete(tau_proto::SessionReplayComplete {
            session_id: "session".parse().expect("session ID"),
            error: Some("session replay failed".into()),
        }),
    )
    .expect("session replay error");
    assert!(!session_state.projection_valid);
    assert!(!session_state.replay_complete);
    assert!(session_state.worker.is_none());
}

/// A session-start lifecycle event owns the incarnation reset and clears old
/// projected agents, blockers, updates, and pending loopbacks before replay.
#[test]
fn session_switch_clears_incarnation_state_before_replay() {
    let mut state = SwarmRuntime::new();
    state.config = Some(resolved_config());
    state.session_id = Some("old".parse().expect("session ID"));
    state.replay_complete = true;
    let agent = tau_swarm_api::AgentId::new("agent");
    let mut draft = AgentDraft::new(&agent);
    draft.loaded = true;
    draft.replay_valid = true;
    state.agents.insert(agent, draft);
    state
        .projection
        .blocking_lock()
        .add_update(tau_swarm_api::UpdatePublication {
            id: tau_swarm_api::UpdateId::new("update"),
            owner: tau_swarm_api::AgentId::new("agent"),
            title: "title".into(),
            description: "description".into(),
            task_id: None,
            source_timestamp: tau_swarm_api::Timestamp(1),
        })
        .expect("old update");
    state
        .blocker_history
        .lock()
        .expect("history")
        .push(crate::tools::BlockerRecord {
            blocker_id: tau_swarm_api::BlockerId::new("blocker"),
            revision: tau_swarm_api::BlockerRevisionNumber(1),
            owner: tau_swarm_api::AgentId::new("agent"),
            title: "title".into(),
            description: "description".into(),
            recommended_answer: None,
            task_id: None,
            state: crate::tools::BlockerState::Active,
            answer: None,
            answer_kind: None,
            reason: None,
            reserved_answer_bytes: 0,
        });
    let (completion, _result) = tokio::sync::oneshot::channel();
    state.pending.lock().expect("pending").insert(
        PendingKey {
            agent_id: tau_swarm_api::AgentId::new("agent"),
            ctx_id: "ctx".into(),
            text: "text".into(),
        },
        completion,
    );
    fold_event(
        &mut state,
        &Event::SessionStarted(tau_proto::SessionStarted {
            session_id: "new".parse().expect("session ID"),
            reason: tau_proto::SessionStartReason::Resume,
        }),
    )
    .expect("session switch");
    assert_eq!(
        state.session_id.as_ref().map(tau_proto::SessionId::as_str),
        Some("new")
    );
    assert!(!state.replay_complete);
    assert!(state.agents.is_empty());
    assert!(state.blocker_history.lock().expect("history").is_empty());
    assert!(state.pending.lock().expect("pending").is_empty());
    assert_eq!(state.projection.blocking_lock().update_usage(), (0, 0));
    for stale in [
        Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
            session_id: "old".parse().expect("session ID"),
            agent_id: tau_proto::AgentId::parse("stale").expect("agent ID"),
            agent_initialization_id: tau_proto::AgentInitializationId::parse("stale")
                .expect("valid initialization id"),
            ephemeral: false,
        }),
        Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
            session_id: "old".parse().expect("session ID"),
            agent_id: tau_proto::AgentId::parse("agent").expect("agent ID"),
        }),
        Event::SessionShutdown(tau_proto::SessionShutdown {
            session_id: "old".parse().expect("session ID"),
        }),
        Event::AgentReplayComplete(tau_proto::AgentReplayComplete {
            agent_id: tau_proto::AgentId::parse("stale").expect("agent ID"),
            session_id: Some("old".parse().expect("session ID")),
            error: Some("stale error".into()),
        }),
        Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
            session_id: "old".parse().expect("session ID"),
            agent_id: tau_proto::AgentId::parse("stale").expect("agent ID"),
            navigation_mode: tau_proto::AgentNavigationMode::Active,
            runtime_state: tau_proto::AgentRuntimeState::Running,
            tools: tau_proto::AgentToolStats::default(),
            context: tau_proto::AgentContextStats::default(),
            estimated_api_cost: tau_proto::EstimatedApiCost::default(),
        }),
        Event::AgentWatchesUpdated(tau_proto::AgentWatchesUpdated {
            session_id: "old".parse().expect("session ID"),
            watcher_id: tau_proto::AgentId::parse("stale").expect("agent ID"),
            watched_agent_ids: vec![tau_proto::AgentId::parse("other").expect("agent ID")],
            changed_agent_id: None,
            cause: tau_proto::AgentWatchUpdateCause::SessionSnapshot,
        }),
    ] {
        fold_event(&mut state, &stale).expect("stale event ignored");
    }
    assert_eq!(
        state.session_id.as_ref().map(tau_proto::SessionId::as_str),
        Some("new")
    );
    assert!(state.projection_valid);
    assert!(state.agents.is_empty());
}

/// Stats and watch replacement folds converge exactly in the published agent,
/// and unload removes that agent from current Swarm state.
#[test]
fn stats_watches_and_unload_converge_projection() {
    let mut state = SwarmRuntime::new();
    state.config = Some(resolved_config());
    let session_id: tau_proto::SessionId = "session".parse().expect("session ID");
    let agent_id = tau_proto::AgentId::parse("agent").expect("agent ID");
    for event in [
        Event::SessionStarted(tau_proto::SessionStarted {
            session_id: session_id.clone(),
            reason: tau_proto::SessionStartReason::Initial,
        }),
        Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
            session_id: session_id.clone(),
            agent_id: agent_id.clone(),
            agent_initialization_id: tau_proto::AgentInitializationId::parse("init")
                .expect("valid initialization id"),
            ephemeral: false,
        }),
        Event::AgentReplayComplete(tau_proto::AgentReplayComplete {
            agent_id: agent_id.clone(),
            session_id: Some(session_id.clone()),
            error: None,
        }),
        Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
            session_id: session_id.clone(),
            agent_id: agent_id.clone(),
            navigation_mode: tau_proto::AgentNavigationMode::Suspended,
            runtime_state: tau_proto::AgentRuntimeState::Running,
            tools: tau_proto::AgentToolStats::default(),
            context: tau_proto::AgentContextStats::default(),
            estimated_api_cost: tau_proto::EstimatedApiCost::default(),
        }),
        Event::AgentWatchesUpdated(tau_proto::AgentWatchesUpdated {
            session_id: session_id.clone(),
            watcher_id: agent_id.clone(),
            watched_agent_ids: vec![tau_proto::AgentId::parse("other").expect("agent ID")],
            changed_agent_id: None,
            cause: tau_proto::AgentWatchUpdateCause::SessionSnapshot,
        }),
    ] {
        fold_event(&mut state, &event).expect("agent fold");
    }
    let snapshot = state.projection.blocking_lock().snapshot();
    let agent = snapshot.snapshot.agents.first().expect("published agent");
    assert_eq!(agent.activity, tau_swarm_api::AgentActivity::Running);
    assert_eq!(
        agent.navigation_mode,
        tau_swarm_api::AgentNavigationMode::Suspended
    );
    assert_eq!(
        agent.watches,
        std::collections::BTreeSet::from([tau_swarm_api::AgentId::new("other")])
    );
    fold_event(
        &mut state,
        &Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
            session_id,
            agent_id,
        }),
    )
    .expect("unload fold");
    assert!(
        state
            .projection
            .blocking_lock()
            .snapshot()
            .snapshot
            .agents
            .is_empty()
    );
}

/// Canonical Tau loopback completes only an exact agent/context/text match.
#[test]
fn canonical_prompt_loopback_requires_exact_identity() {
    let state = SwarmRuntime::new();
    let (completion, mut result) = tokio::sync::oneshot::channel();
    state.pending.lock().expect("pending").insert(
        PendingKey {
            agent_id: tau_swarm_api::AgentId::new("agent"),
            ctx_id: "ctx".into(),
            text: "text".into(),
        },
        completion,
    );
    for event in [
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            agent_id: tau_proto::AgentId::parse("other").expect("agent ID"),
            inference_activation: false,
            text: "text".into(),
            message_class: tau_proto::PromptMessageClass::default(),
            internal_kind: None,
            originator: tau_proto::PromptOriginator::default(),
            submission_source: tau_proto::PromptSubmissionSource::default(),
            display_name: None,
            ctx_id: Some("ctx".into()),
        }),
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            agent_id: tau_proto::AgentId::parse("agent").expect("agent ID"),
            inference_activation: false,
            text: "text".into(),
            message_class: tau_proto::PromptMessageClass::default(),
            internal_kind: None,
            originator: tau_proto::PromptOriginator::default(),
            submission_source: tau_proto::PromptSubmissionSource::default(),
            display_name: None,
            ctx_id: Some("other".into()),
        }),
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            agent_id: tau_proto::AgentId::parse("agent").expect("agent ID"),
            inference_activation: false,
            text: "text".into(),
            message_class: tau_proto::PromptMessageClass::default(),
            internal_kind: None,
            originator: tau_proto::PromptOriginator::default(),
            submission_source: tau_proto::PromptSubmissionSource::default(),
            display_name: None,
            ctx_id: None,
        }),
    ] {
        fold_canonical_prompt(&state, &event);
    }
    state.complete_prompt("agent", Some("ctx"), "different");
    assert!(matches!(
        result.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    fold_canonical_prompt(
        &state,
        &Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            agent_id: tau_proto::AgentId::parse("agent").expect("agent ID"),
            inference_activation: false,
            text: "text".into(),
            message_class: tau_proto::PromptMessageClass::default(),
            internal_kind: None,
            originator: tau_proto::PromptOriginator::default(),
            submission_source: tau_proto::PromptSubmissionSource::default(),
            display_name: None,
            ctx_id: Some("ctx".into()),
        }),
    );
    assert_eq!(result.try_recv(), Ok(Ok(())));

    let (completion, mut steered_result) = tokio::sync::oneshot::channel();
    state.pending.lock().expect("pending").insert(
        PendingKey {
            agent_id: tau_swarm_api::AgentId::new("agent"),
            ctx_id: "steered".into(),
            text: "text".into(),
        },
        completion,
    );
    fold_canonical_prompt(
        &state,
        &Event::AgentPromptSteered(tau_proto::AgentPromptSteered {
            agent_id: tau_proto::AgentId::parse("agent").expect("agent ID"),
            inference_activation: false,
            submission_source: tau_proto::PromptSubmissionSource::default(),
            text: "text".into(),
            message_class: tau_proto::PromptMessageClass::default(),
            internal_kind: None,
            ctx_id: Some("steered".into()),
        }),
    );
    assert_eq!(steered_result.try_recv(), Ok(Ok(())));
}

/// The real extension runner advertises both historical and live selectors for
/// every projection fact, so attaching to an existing session receives catch-up
/// before live publication.
#[test]
fn runner_subscribes_projection_for_restore_and_live_delivery() {
    let peer_id = iroh::SecretKey::generate().public().to_string();
    let mut input = Vec::new();
    let mut input_writer = HarnessOutputWriter::new(&mut input);
    input_writer
        .write_message(&HarnessOutputMessage::Configure(Configure {
            tool_prefix: None,
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "endpoint": {"peer_id": peer_id},
                "credential_id": "worker",
                "credential_secret": "swarm",
                "hostname": "host"
            })),
            instance_name: tau_proto::ExtensionName::parse("std-swarm")
                .expect("valid extension name"),
            state_dir: None,
            secrets: BTreeMap::from([("swarm".into(), SecretValue::new("secret"))]),
        }))
        .expect("configure frame");
    input_writer.flush().expect("configure flush");
    let mut output = Vec::new();
    TauExtensionRunner::new(SwarmExtension)
        .run(
            std::io::Cursor::new(input),
            &mut output,
            SwarmRuntime::new(),
        )
        .expect("runner startup");
    let mut reader = HarnessInputReader::new(output.as_slice());
    let frames = std::iter::from_fn(|| reader.read_message().transpose())
        .collect::<Result<Vec<_>, _>>()
        .expect("output frames");
    let subscribe = frames
        .iter()
        .find_map(|frame| match frame {
            HarnessInputMessage::Subscribe(subscribe) => Some(subscribe.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("startup subscription in {frames:?}"));
    let loaded = EventSelector::Exact(tau_proto::EventName::SESSION_AGENT_LOADED);
    let boundary = EventSelector::Exact(tau_proto::EventName::AGENT_REPLAY_COMPLETE);
    assert!(subscribe.historical_selectors.contains(&loaded));
    assert!(subscribe.historical_selectors.contains(&boundary));
    assert!(subscribe.live_selectors.contains(&loaded));
    assert!(subscribe.live_selectors.contains(&boundary));
}

/// The concrete Iroh connector rejects a mismatched configured peer before
/// opening a network connection or transmitting credentials.
#[tokio::test]
async fn iroh_connector_rejects_peer_mismatch_before_network() {
    let endpoint = Endpoint::builder(presets::N0)
        .bind()
        .await
        .expect("client endpoint");
    let configured = iroh::SecretKey::generate().public();
    let expected = iroh::SecretKey::generate().public();
    let connector = tau_swarm_iroh::IrohConnector::new(
        endpoint.clone(),
        iroh::EndpointAddr::from_parts(configured, []),
    );
    let error = match connector
        .connect(&ExpectedPeer::new(expected.as_bytes()))
        .await
    {
        Ok(_) => panic!("peer mismatch connected"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), ErrorKind::Protocol);
    endpoint.close().await;
}

/// The real Tau runner and published Swarm 0.1 server compose Configure,
/// replay folding, worker startup, snapshot publication, remote prompt routing,
/// transient internal submission, canonical loopback, and accepted RPC result.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_and_published_server_complete_remote_prompt_vertical() {
    let server_endpoint = Endpoint::builder(presets::Minimal)
        .bind()
        .await
        .expect("server endpoint");
    let credential = Credential {
        id: CredentialId::new("worker"),
        secret: Secret::new(b"secret"),
    };
    let server = Server::spawn(
        server_endpoint,
        Credentials::single(credential),
        tau_swarm_core::CoreService::new(()),
    );
    let server_addr = server.addr();
    let direct_addresses: Vec<_> = server_addr.ip_addrs().map(ToString::to_string).collect();
    assert!(!direct_addresses.is_empty());

    let (mut harness_input, extension_input) = UnixStream::pair().expect("input sockets");
    let (extension_output, harness_output) = UnixStream::pair().expect("output sockets");
    harness_output
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("output deadline");
    let (runner_done_tx, runner_done_rx) = std::sync::mpsc::channel();
    let runner = std::thread::spawn(move || {
        let result = TauExtensionRunner::new(SwarmExtension)
            .run(extension_input, extension_output, SwarmRuntime::new())
            .map_err(|error| error.to_string());
        let signal = result.as_ref().map(|_| ()).map_err(Clone::clone);
        let _ = runner_done_tx.send(signal);
        result
    });
    let mut writer = HarnessOutputWriter::new(&mut harness_input);
    writer
        .write_message(&HarnessOutputMessage::Configure(Configure {
            tool_prefix: None,
            config: tau_proto::json_to_cbor(&serde_json::json!({
                "endpoint": {
                    "peer_id": server_addr.id.to_string(),
                    "direct_addresses": direct_addresses
                },
                "credential_id": "worker",
                "credential_secret": "swarm",
                "hostname": "host",
                "command_timeout_ms": 1000
            })),
            instance_name: tau_proto::ExtensionName::parse("std-swarm")
                .expect("valid extension name"),
            state_dir: None,
            secrets: BTreeMap::from([("swarm".into(), SecretValue::new("secret"))]),
        }))
        .expect("configure");
    writer.flush().expect("configure flush");
    let mut output_reader = HarnessInputReader::new(harness_output);
    loop {
        match output_reader
            .read_message()
            .expect("startup output")
            .expect("startup frame")
        {
            HarnessInputMessage::Ready(_) => break,
            HarnessInputMessage::ConfigError(error) => {
                panic!("vertical configure failed: {}", error.message)
            }
            _ => {}
        }
    }
    let session_id: tau_proto::SessionId = "session".parse().expect("session ID");
    let agent_id = tau_proto::AgentId::parse("agent").expect("agent ID");
    for message in [
        HarnessOutputMessage::deliver(Event::SessionStarted(tau_proto::SessionStarted {
            session_id: session_id.clone(),
            reason: tau_proto::SessionStartReason::Initial,
        })),
        HarnessOutputMessage::deliver_replay(
            tau_proto::UnixMicros::new(1),
            Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                session_id: session_id.clone(),
                agent_id: agent_id.clone(),
                agent_initialization_id: tau_proto::AgentInitializationId::parse("init")
                    .expect("valid initialization id"),
                ephemeral: false,
            }),
        ),
        HarnessOutputMessage::deliver(Event::AgentReplayComplete(tau_proto::AgentReplayComplete {
            agent_id: agent_id.clone(),
            session_id: Some(session_id.clone()),
            error: None,
        })),
        HarnessOutputMessage::deliver(Event::SessionReplayComplete(
            tau_proto::SessionReplayComplete {
                session_id: session_id.clone(),
                error: None,
            },
        )),
    ] {
        writer.write_message(&message).expect("lifecycle event");
    }
    writer.flush().expect("lifecycle flush");

    let swarm_session = SessionIdentity::new(Hostname::new("host"), SessionId::new("session"));
    tokio::time::timeout(Duration::from_secs(5), async {
        while !server.view().snapshot().sessions.iter().any(|view| {
            view.identity == swarm_session
                && view.connection == tau_swarm_core::ConnectionState::Synchronized
                && view
                    .agents
                    .contains_key(&tau_swarm_api::AgentId::new("agent"))
        }) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("runner-published snapshot");

    let commands = server.commands();
    let dispatch_session = swarm_session.clone();
    let dispatch = tokio::spawn(async move {
        commands
            .prompt(
                &dispatch_session,
                PromptRequest {
                    correlation_id: CorrelationId::new("vertical"),
                    agent_id: tau_swarm_api::AgentId::new("agent"),
                    message: "continue".into(),
                },
            )
            .await
    });
    let (reader, request) = tokio::task::spawn_blocking(move || {
        let mut reader = output_reader;
        loop {
            let frame = reader
                .read_message()
                .expect("runner output")
                .expect("runner frame");
            if let HarnessInputMessage::Emit(emit) = frame
                && let Event::ExtInternalPromptSubmitRequest(request) = *emit.event
            {
                break (reader, request);
            }
        }
    })
    .await
    .expect("output reader");
    assert_eq!(request.agent_id, agent_id);
    assert_eq!(request.ctx_id.as_deref(), Some("vertical"));
    assert_eq!(request.text, "continue");
    writer
        .write_message(&HarnessOutputMessage::deliver(Event::AgentPromptSubmitted(
            tau_proto::AgentPromptSubmitted {
                agent_id,
                inference_activation: true,
                text: request.text,
                message_class: tau_proto::PromptMessageClass::Internal,
                internal_kind: None,
                originator: tau_proto::PromptOriginator::default(),
                submission_source: tau_proto::PromptSubmissionSource::default(),
                display_name: None,
                ctx_id: request.ctx_id,
            },
        )))
        .expect("canonical prompt");
    writer.flush().expect("canonical flush");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), dispatch)
            .await
            .expect("bounded dispatch")
            .expect("dispatch task")
            .expect("dispatch result"),
        DeliveryOutcome::Accepted
    );

    let output = reader.into_inner();
    output
        .shutdown(std::net::Shutdown::Both)
        .expect("close output socket");
    let input = writer.into_inner();
    input
        .shutdown(std::net::Shutdown::Both)
        .expect("close input socket");
    tokio::task::spawn_blocking(move || {
        runner_done_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("bounded runner completion")
    })
    .await
    .expect("completion waiter")
    .expect("runner result");
    runner
        .join()
        .expect("runner thread")
        .expect("runner result");
    server.shutdown().await.expect("server shutdown");
}
