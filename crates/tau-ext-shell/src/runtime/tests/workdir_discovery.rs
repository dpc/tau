//! Workdir discovery uses explicit agent state and acknowledges installed
//! scans.

use super::*;

/// Extract one declared replacement from already-emitted protocol frames.
fn take_snapshot(
    rx: &mpsc::Receiver<HarnessInputMessage>,
) -> tau_proto::ExtensionAgentDiscoverySnapshotDeclared {
    std::iter::from_fn(|| rx.try_recv().ok())
        .find_map(|message| match message {
            HarnessInputMessage::Emit(emit) => match *emit.event {
                Event::ExtensionAgentDiscoverySnapshotDeclared(snapshot) => Some(snapshot),
                _ => None,
            },
            _ => None,
        })
        .expect("discovery snapshot")
}

/// A replacement process restores the original user scope before successful or
/// failed scans, rather than changing same-load users according to scan
/// success.
#[test]
fn workdir_discovery_reconnect_restores_original_user_scope() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    let root = home.join(".config/agents");
    let project = tmp.path().join("project");
    std::fs::create_dir_all(root.join("skills")).expect("user roots");
    std::fs::create_dir(&project).expect("project");
    let path = root.join("skills/user.md");
    std::fs::write(&path, "---\nname: user\ndescription: original\n---\n").expect("user");
    std::fs::write(root.join("AGENTS.md"), "original user instructions").expect("instructions");
    let original = DiscoverySource::new(Some(home.clone()));
    let retained_user_state = original.retained_user_state();
    drop(original);
    std::fs::remove_file(path).expect("remove original user");
    std::fs::write(root.join("AGENTS.md"), "replacement user instructions").expect("edit");
    let (tx, rx) = mpsc::channel();
    let mut runtime = ShellRuntime::new(
        Output::channel(tx),
        ExtConfig::default(),
        DiscoverySourcePolicy::Environment,
    );
    let agent_id = tau_proto::AgentId::parse("restored-user-agent").expect("agent");
    let session_id = tau_proto::SessionId::parse("restored-user-session").expect("session");
    let initialization_id = tau_proto::AgentInitializationId::parse("same-load").expect("load");
    runtime.cwd_state.set_pending_ready(
        agent_id.clone(),
        session_id.clone(),
        initialization_id.clone(),
    );
    runtime
        .discovery_sources
        .insert(agent_id.clone(), DiscoverySource::new(Some(home.clone())));
    let mut request = tau_proto::HarnessAgentDiscoveryRefreshRequested {
        session_id,
        agent_id: agent_id.clone(),
        agent_initialization_id: initialization_id,
        metadata_key: runtime.cwd_state.key(),
        metadata_value: Some(CborValue::Text(project.display().to_string())),
        refresh_id: 1,
        retained_user_state,
    };
    runtime
        .handle_discovery_refresh(request.clone())
        .expect("restore and scan");
    let success = take_snapshot(&rx);
    assert!(success.discovery_error.is_none());
    assert_eq!(success.skills[0].description, "original");
    assert!(
        success
            .agents_files
            .iter()
            .any(|file| file.content == "original user instructions")
    );
    std::fs::remove_dir(&project).expect("remove project");
    request.refresh_id += 1;
    runtime
        .handle_discovery_refresh(request.clone())
        .expect("unavailable project");
    let failed = take_snapshot(&rx);
    assert!(failed.discovery_error.is_some());
    assert_eq!(failed.skills, success.skills);
    assert_eq!(failed.agents_files, success.agents_files);
    request.retained_user_state = vec![255];
    request.refresh_id += 1;
    runtime
        .handle_discovery_refresh(request)
        .expect("report invalid retained scope");
    assert!(
        take_snapshot(&rx)
            .discovery_error
            .expect("error")
            .contains("invalid retained user")
    );
    let new_load = DiscoverySource::new(Some(home));
    let new_user = new_load
        .scan_project("new-session".parse().expect("session"), None)
        .snapshot;
    assert!(new_user.skills.is_empty());
    assert!(
        new_user
            .agents_files
            .iter()
            .any(|file| file.content == "replacement user instructions")
    );
}

/// Restored and inherited metadata both select the agent's project instead of
/// the shell process startup directory; subsequent scans use the committed
/// value.
#[test]
fn workdir_discovery_scans_explicit_initial_and_committed_paths() {
    for replayed in [false, true] {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        for (path, text) in [(&first, "first-project"), (&second, "second-project")] {
            std::fs::create_dir(path).expect("project");
            std::fs::write(path.join("AGENTS.md"), text).expect("instructions");
        }
        let (tx, rx) = mpsc::channel();
        let mut runtime = ShellRuntime::new(
            Output::channel(tx),
            ExtConfig::default(),
            DiscoverySourcePolicy::Environment,
        );
        let agent_id = tau_proto::AgentId::parse("project-agent").expect("agent");
        let session_id = tau_proto::SessionId::parse("project-session").expect("session");
        let initialization_id =
            tau_proto::AgentInitializationId::parse("project-load").expect("load");
        runtime
            .handle_event(
                Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
                    agent_id: agent_id.clone(),
                    key: runtime.cwd_state.key(),
                    value: CborValue::Text(first.display().to_string()),
                    mutation_id: None,
                    inheritable: true,
                }),
                replayed,
            )
            .expect("remember cwd");
        while rx.try_recv().is_ok() {}
        runtime
            .handle_event(
                Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                    session_id: session_id.clone(),
                    agent_id: agent_id.clone(),
                    agent_initialization_id: initialization_id.clone(),
                    ephemeral: false,
                }),
                false,
            )
            .expect("load");
        runtime
            .handle_agent_replay_complete(tau_proto::AgentReplayComplete {
                agent_id: agent_id.clone(),
                session_id: Some(session_id.clone()),
                error: None,
            })
            .expect("replay boundary");
        let initial = take_snapshot(&rx);
        assert!(initial.workdir_binding.is_some());
        assert!(
            initial
                .agents_files
                .iter()
                .any(|file| file.content == "first-project")
        );
        assert!(
            !initial
                .agents_files
                .iter()
                .any(|file| file.content == "second-project")
        );
        while rx.try_recv().is_ok() {}

        // Deliberately leave the local cwd fold at `first`: the harness request
        // owns the actual committed scan value even when delivery is reordered.
        let request = tau_proto::HarnessAgentDiscoveryRefreshRequested {
            session_id,
            agent_id: agent_id.clone(),
            agent_initialization_id: initialization_id,
            metadata_key: runtime.cwd_state.key(),
            metadata_value: Some(CborValue::Text(second.display().to_string())),
            refresh_id: 7,
            retained_user_state: runtime.discovery_sources[&agent_id].retained_user_state(),
        };
        runtime
            .handle_discovery_refresh(request.clone())
            .expect("refresh");
        let refreshed = take_snapshot(&rx);
        assert_eq!(refreshed.refresh_id, Some(7));
        assert!(refreshed.workdir_binding.is_none());
        assert!(
            refreshed
                .agents_files
                .iter()
                .any(|file| file.content == "second-project")
        );
        assert!(
            !refreshed
                .agents_files
                .iter()
                .any(|file| file.content == "first-project")
        );

        std::fs::remove_file(second.join("AGENTS.md")).expect("delete instructions");
        runtime
            .handle_discovery_refresh(request.clone())
            .expect("same path rescan");
        assert!(
            !take_snapshot(&rx)
                .agents_files
                .iter()
                .any(|file| file.content == "second-project")
        );
        std::fs::remove_dir(&second).expect("remove cwd");
        runtime
            .handle_discovery_refresh(request)
            .expect("unavailable cwd");
        let failed = take_snapshot(&rx);
        assert!(failed.discovery_error.is_some());
        assert!(
            !failed
                .agents_files
                .iter()
                .any(|file| file.content == "first-project" || file.content == "second-project")
        );
    }
}

/// A loaded setter remains reserved after metadata echo and cancellation until
/// its matching durable discovery ACK; duplicate ACKs never duplicate
/// terminals.
#[test]
fn workdir_discovery_setter_waits_for_installed_ack_even_when_cancelled() {
    for cancel in [false, true] {
        let (tx, rx) = mpsc::channel();
        let mut runtime = ShellRuntime::new(
            Output::channel(tx),
            ExtConfig::default(),
            DiscoverySourcePolicy::Environment,
        );
        let agent_id = tau_proto::AgentId::parse("ack-agent").expect("agent");
        let session_id = tau_proto::SessionId::parse("ack-session").expect("session");
        let initialization_id = tau_proto::AgentInitializationId::parse("ack-load").expect("load");
        runtime.cwd_state.set_pending_ready(
            agent_id.clone(),
            session_id.clone(),
            initialization_id.clone(),
        );
        runtime
            .discovery_sources
            .insert(agent_id.clone(), DiscoverySource::empty());
        let invoke = tau_proto::ToolStarted {
            invocation_policy: Default::default(),
            call_id: tau_proto::ToolCallId::new("ack-setter"),
            tool_name: tau_proto::ToolName::new(crate::tools::WORKDIR_TOOL_NAME),
            arguments: CborValue::Map(Vec::new()),
            agent_id: agent_id.clone(),
            originator: tau_proto::PromptOriginator::User,
        };
        runtime
            .cwd_state
            .start_pending_workdir_result(
                agent_id.clone(),
                PathBuf::from("/tmp"),
                invoke.clone(),
                None,
            )
            .expect("reserve");
        let mutation = runtime
            .cwd_state
            .pending_workdir_mutation_id(&agent_id, &invoke.call_id)
            .expect("mutation");
        assert!(
            runtime
                .cwd_state
                .mark_pending_workdir_awaiting_echo(&agent_id, &invoke.call_id)
        );
        runtime
            .handle_agent_metadata_set(
                tau_proto::AgentMetadataSet {
                    agent_id: agent_id.clone(),
                    key: runtime.cwd_state.key(),
                    value: CborValue::Text("/tmp".to_owned()),
                    mutation_id: Some(mutation.clone()),
                    inheritable: true,
                },
                false,
            )
            .expect("metadata echo");
        if cancel {
            runtime.handle_tool_cancel_request(tau_proto::ToolCancelRequest {
                target_call_id: invoke.call_id.clone(),
            });
        }
        assert!(
            runtime
                .cwd_state
                .pending_workdir_mutation_id(&agent_id, &invoke.call_id)
                .is_some()
        );
        let terminal_count = |frames: Vec<HarnessInputMessage>| {
            frames.into_iter().filter(|message| matches!(
            message, HarnessInputMessage::Emit(emit) if matches!(emit.event.as_ref(),
                Event::ToolResultReported(_) | Event::ToolErrorReported(_) | Event::ToolCancelledReported(_))
        )).count()
        };
        assert_eq!(
            terminal_count(std::iter::from_fn(|| rx.try_recv().ok()).collect()),
            0
        );
        let ack = tau_proto::HarnessAgentContextInitialized {
            session_id,
            agent_id: agent_id.clone(),
            agent_initialization_id: initialization_id,
            discovery_revision: 1,
            discovery_refreshes: vec![tau_proto::DiscoveryRefreshOutcome {
                metadata_key: runtime.cwd_state.key(),
                refresh_id: 1,
                mutation_id: Some(mutation),
                error: None,
            }],
            discovery_diagnostics: Vec::new(),
            listed_skills: Vec::new(),
            effective_skills: Vec::new(),
            agents_files: Vec::new(),
        };
        runtime
            .handle_discovery_installed(ack.clone())
            .expect("ack");
        runtime
            .handle_discovery_installed(ack)
            .expect("duplicate ack");
        assert_eq!(
            terminal_count(std::iter::from_fn(|| rx.try_recv().ok()).collect()),
            1
        );
        assert!(
            runtime
                .cwd_state
                .pending_workdir_mutation_id(&agent_id, &invoke.call_id)
                .is_none()
        );
    }
}
