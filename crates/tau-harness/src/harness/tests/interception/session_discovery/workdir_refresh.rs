//! Source-local refresh, degraded recovery, and stable initialization oracles.

use super::*;
use crate::harness::interception::PostCommitContinuation;
use crate::harness::tests::dispatch::{
    enable_remote_compaction_for_test_model, standalone_compaction_success_response,
};
use crate::harness::tests::lifecycle::reasoning_only_length_response;

/// Clearing an ordinary canceled FIFO invalidates its deferred fold, while
/// teardown removes the retained callback so a later installation cannot revive
/// discarded work.
#[test]
fn workdir_refresh_discards_canceled_ordinary_fold() {
    for teardown in [false, true] {
        let tmp = TempDir::new().expect("tempdir");
        let mut h = echo_harness(tmp.path()).expect("harness");
        connect_ready_configured_extension(&mut h, "one", "one", tau_proto::ClientKind::Tool);
        register_discovery_test_provider(&mut h, "one");
        let role = h.config.selected_role.clone();
        let cid = h.create_durable_user_agent(h.session_runtime.current_session_id.clone(), &role);
        let agent = durable_agent_id_for_conversation(&h, &cid);
        let user = skill(&tmp.path().join("user.md"), "user", None);
        initialize_source(
            &mut h,
            &agent,
            "one",
            user.clone(),
            skill(&tmp.path().join("old.md"), "old", None),
        );
        h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("active turn".to_owned()))
            .expect("active prompt to cancel");
        let revision = commit_cwd(&mut h, &agent, "one", "cancel-fold");
        let mut queued = PendingPrompt::user(":skill new".to_owned());
        queued.expand_user_skill_on_dispatch = true;
        h.agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent")
            .dispatch
            .pending_prompts
            .push_back(queued);
        assert_eq!(
            h.fold_pending_prompts_as_steered_with_completion(&cid, None),
            crate::harness::tool_runtime::PromptFoldDisposition::Deferred
        );
        h.handle_cancel_prompt(
            &crate::harness::harness_connection_id(),
            &tau_proto::UiCancelPrompt {
                session_id: h.session_runtime.current_session_id.clone(),
                target_agent_id: Some(agent.clone()),
                agent_prompt_id: None,
            },
        );
        assert!(
            !h.agent_runtime.agent_registry.agents[&cid]
                .dispatch
                .discovery_fold_pending
        );
        if teardown {
            h.cancel_agent_synchronized_publications(&cid);
            assert!(
                !h.prompt_coordination
                    .context_discovery
                    .deferred_folds
                    .contains_key(&cid)
            );
        }
        refresh_reply(&mut h, &agent, "one", revision, vec![user]);
        assert_eq!(prompt_created_count(&h), 1);
        assert!(
            !h.prompt_coordination
                .context_discovery
                .deferred_folds
                .contains_key(&cid)
        );
        h.shutdown().expect("shutdown");
    }
}

/// A standalone continuation owns its completion while skill expansion waits,
/// including when every deferred skill fails expansion and the fold is empty.
#[test]
fn workdir_refresh_preserves_standalone_fold_completion() {
    for available in [false, true] {
        let tmp = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(tmp.path()).expect("harness");
        enable_remote_compaction_for_test_model(&mut h);
        h.provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("model")
            .supports_standalone_compaction = true;
        connect_ready_configured_extension(&mut h, "one", "one", tau_proto::ClientKind::Tool);
        register_discovery_test_provider(&mut h, "one");
        let role = h.config.selected_role.clone();
        let cid = h.create_durable_user_agent(h.session_runtime.current_session_id.clone(), &role);
        let agent = durable_agent_id_for_conversation(&h, &cid);
        let user = skill(&tmp.path().join("user.md"), "user", None);
        initialize_source(
            &mut h,
            &agent,
            "one",
            user.clone(),
            skill(&tmp.path().join("old.md"), "old", None),
        );
        let parent = h
            .selected_head_for_agent(&cid)
            .unwrap_or(tau_proto::AgentHead::Root);
        let transaction_id =
            tau_proto::CompactionTransactionId::parse("ct-discovery-fold").expect("transaction");
        h.publish_for_agent(
            &cid,
            Event::AgentStandaloneCompactionStarted(tau_proto::AgentStandaloneCompactionStarted {
                agent_id: agent.clone(),
                transaction_id: transaction_id.clone(),
                compact_prompt_id: tau_proto::AgentPromptId::parse("ap-discovery-fold")
                    .expect("prompt"),
                cut: parent,
                resume_through: Some(parent),
                model: "test/model".into(),
                operation: tau_proto::PromptOperation::StandaloneCompaction,
                originator: tau_proto::PromptOriginator::User,
                supersedes: None,
                trigger: tau_proto::StandaloneCompactionTrigger::Manual,
            }),
        );
        let compact = read_nth_prompt_created(&h, 0);
        assert_eq!(
            compact.operation,
            tau_proto::PromptOperation::StandaloneCompaction
        );
        let revision = commit_cwd(&mut h, &agent, "one", "standalone-fold");
        let mut queued = PendingPrompt::user(":skill new".to_owned());
        queued.expand_user_skill_on_dispatch = true;
        h.agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent")
            .dispatch
            .pending_prompts
            .push_back(queued);
        h.handle_provider_response_finished(standalone_compaction_success_response(
            &compact,
            "replacement",
        ))
        .expect("compacted");
        assert!(
            h.agent_runtime.agent_registry.agents[&cid]
                .dispatch
                .discovery_fold_pending
        );
        assert!(matches!(
            h.prompt_coordination.context_discovery.deferred_folds[&cid].completion,
            Some(
                crate::harness::interception::AgentPublishCompletion::StandaloneContinuation { .. }
            ),
        ));
        assert_eq!(prompt_created_count(&h), 1);
        let mut skills = vec![user];
        if available {
            skills.push(skill(&tmp.path().join("new.md"), "new", None));
        }
        refresh_reply(&mut h, &agent, "one", revision, skills);
        assert!(
            !h.agent_runtime.agent_registry.agents[&cid]
                .dispatch
                .discovery_fold_pending
        );
        let events = event_log_events(&h);
        assert_eq!(events.iter().filter(|event| matches!(event,
            Event::AgentInferenceDispatchStarted(started) if started.transaction_id.as_ref() == Some(&transaction_id)
        )).count(), 1, "exact standalone continuation once, available={available}");
        assert_eq!(prompt_created_count(&h), 2);
        let continuation = read_nth_prompt_created(&h, 1);
        assert_eq!(
            serde_json::to_string(&continuation.context)
                .expect("context")
                .contains("body new"),
            available
        );
        h.resume_discovery_fold(&cid);
        assert_eq!(prompt_created_count(&h), 2);
        h.shutdown().expect("shutdown");
    }
}

/// Synchronous finalization on the no-provider Resume path must not reenter
/// session startup as Initial before the outer readiness transaction completes.
#[test]
fn workdir_discovery_resume_finalization_does_not_reenter_initial_startup() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent = durable_agent_id_for_conversation(&h, &cid);
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .dispatch
        .pending_prompts
        .push_back(PendingPrompt::user("queued resume input".to_owned()));
    let session = h.session_runtime.current_session_id.clone();
    h.prompt_coordination
        .context_discovery
        .initialized_sessions
        .remove(&session);
    h.prompt_coordination
        .context_discovery
        .frozen_agents
        .remove(&agent);
    h.prompt_coordination
        .context_discovery
        .pending_agents
        .remove(&agent);
    let initial_count = |h: &Harness| {
        event_log_events(h).iter().filter(|event| matches!(event,
        Event::SessionStarted(started) if started.reason == tau_proto::SessionStartReason::Initial
    )).count()
    };
    let baseline = initial_count(&h);
    h.complete_session_init(session.clone(), tau_proto::SessionStartReason::Resume)
        .expect("resume");
    drive_harness_until_history_complete(&mut h);
    assert_eq!(initial_count(&h), baseline);
    assert!(h.session_initialized(&session));
    assert_eq!(prompt_created_count(&h), 1);
    h.shutdown().expect("shutdown");
}

/// A replay-selected output-length owner resumes its captured through cut,
/// leaving mixed queued inputs and restore notices for an ordinary boundary.
#[test]
fn workdir_discovery_preserves_ready_output_length_cut_before_restore_notices() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(&mut h, "one", "one", tau_proto::ClientKind::Tool);
    register_discovery_test_provider(&mut h, "one");
    let role = h.config.selected_role.clone();
    let cid = h.create_durable_user_agent(h.session_runtime.current_session_id.clone(), &role);
    let agent = durable_agent_id_for_conversation(&h, &cid);
    let user = skill(&tmp.path().join("user.md"), "user", None);
    initialize_source(
        &mut h,
        &agent,
        "one",
        user.clone(),
        skill(&tmp.path().join("old.md"), "old", None),
    );
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("begin".to_owned()))
        .expect("dispatch");
    let source = read_nth_prompt_created(&h, 0);
    let revision = commit_cwd(&mut h, &agent, "one", "owned-cut");
    h.handle_provider_response_finished(reasoning_only_length_response(&source, 17))
        .expect("plan and steer");
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&cid]
            .turn
            .output_length_continuation,
        crate::agent::OutputLengthContinuationState::OwnerReady(_),
    ));
    let through = h.selected_head_for_agent(&cid).expect("captured through");
    // Cold replay does not have the live deferred-dispatch queue.
    h.runtime_io.publication.idle_dispatches.clear();
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .dispatch
        .pending_replay_activation = true;
    let mut skill_prompt = PendingPrompt::user(":skill user".to_owned());
    skill_prompt.expand_user_skill_on_dispatch = true;
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .dispatch
        .pending_prompts
        .extend([
            skill_prompt,
            PendingPrompt::user("after restart".to_owned()),
        ]);
    h.queue_restore_notice_for_resumed_session(&h.session_runtime.current_session_id.clone());
    refresh_reply(&mut h, &agent, "one", revision, vec![user]);
    assert_eq!(prompt_created_count(&h), 2);
    let successor = read_nth_prompt_created(&h, 1);
    assert!(event_log_events(&h).iter().any(|event| matches!(event,
        Event::AgentInferenceDispatchStarted(started)
            if started.agent_prompt_id == successor.agent_prompt_id && started.through == through
    )));
    let context = serde_json::to_string(&successor.context).expect("context");
    assert!(!context.contains("Previous session was"));
    assert!(!context.contains("body user"));
    assert!(!context.contains("after restart"));
    assert!(context.contains(tau_proto::OUTPUT_LENGTH_CONTINUATION_INSTRUCTION));
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_prompts
            .iter()
            .filter(|prompt| !prompt.is_internal())
            .map(|prompt| prompt.text.as_str())
            .collect::<Vec<_>>(),
        vec![":skill user", "after restart"],
    );
    h.shutdown().expect("shutdown");
}

/// A reserved length continuation precedes discovery-dependent ordinary inputs,
/// which retain their relative FIFO; cancellation still settles the
/// reserved owner instead of dropping or dispatching it as an ordinary turn.
#[test]
fn workdir_refresh_defers_mixed_steers_without_losing_output_length_owner() {
    for cancel in [false, true] {
        let tmp = TempDir::new().expect("tempdir");
        let mut h = echo_harness(tmp.path()).expect("harness");
        connect_ready_configured_extension(&mut h, "one", "one", tau_proto::ClientKind::Tool);
        register_discovery_test_provider(&mut h, "one");
        let role = h.config.selected_role.clone();
        let cid = h.create_durable_user_agent(h.session_runtime.current_session_id.clone(), &role);
        let agent = durable_agent_id_for_conversation(&h, &cid);
        let user = skill(&tmp.path().join("user.md"), "user", None);
        initialize_source(
            &mut h,
            &agent,
            "one",
            user.clone(),
            skill(&tmp.path().join("old.md"), "old", None),
        );
        h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("begin".to_owned()))
            .expect("dispatch");
        let source = read_nth_prompt_created(&h, 0);
        let revision = commit_cwd(&mut h, &agent, "one", "mixed-steer");
        h.handle_authenticated_ui_prompt_submitted(
            crate::harness::harness_connection_id(),
            tau_proto::UiPromptSubmitted {
                literal: false,
                session_id: h.session_runtime.current_session_id.clone(),
                text: ":skill new".to_owned(),
                agent_id: agent.clone(),
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                ctx_id: None,
            },
        )
        .expect("queued skill");
        h.agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent")
            .dispatch
            .pending_prompts
            .push_back(PendingPrompt::user("later ordinary".to_owned()));
        h.handle_provider_response_finished(reasoning_only_length_response(&source, 17))
            .expect("planned length continuation");
        assert_eq!(prompt_created_count(&h), 1);
        assert!(
            h.agent_runtime.agent_registry.agents[&cid]
                .dispatch
                .discovery_fold_pending
        );
        assert!(matches!(
            h.prompt_coordination.context_discovery.deferred_folds[&cid].completion,
            Some(crate::harness::interception::AgentPublishCompletion::OutputLengthSteer { .. }),
        ));
        if cancel {
            h.handle_cancel_prompt(
                &crate::harness::harness_connection_id(),
                &tau_proto::UiCancelPrompt {
                    session_id: h.session_runtime.current_session_id.clone(),
                    target_agent_id: Some(agent.clone()),
                    agent_prompt_id: None,
                },
            );
            assert!(
                h.agent_runtime.agent_registry.agents[&cid]
                    .dispatch
                    .discovery_fold_pending
            );
        }
        refresh_reply(
            &mut h,
            &agent,
            "one",
            revision,
            vec![user, skill(&tmp.path().join("new.md"), "new", None)],
        );
        assert!(
            !h.agent_runtime.agent_registry.agents[&cid]
                .dispatch
                .discovery_fold_pending
        );
        assert!(
            !h.prompt_coordination
                .context_discovery
                .deferred_folds
                .contains_key(&cid)
        );
        let events = event_log_events(&h);
        let checkpoints = events
            .iter()
            .filter_map(|event| match event {
                Event::AgentInferenceDispatchStarted(started)
                    if started.output_length_continuation.is_some() =>
                {
                    Some(started)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            checkpoints.len(),
            1,
            "one reserved successor checkpoint; cancel={cancel}; state={:?}; head={:?}; events={:?}",
            h.agent_runtime.agent_registry.agents[&cid]
                .turn
                .output_length_continuation,
            h.selected_head_for_agent(&cid),
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    Event::HarnessNotice(_) | Event::AgentPromptSteered(_)
                ))
                .collect::<Vec<_>>(),
        );
        if cancel {
            assert_eq!(
                prompt_created_count(&h),
                1,
                "cancelled successor never sent"
            );
            assert_eq!(events.iter().filter(|event| matches!(event,
                Event::ProviderResponseFinished(terminal)
                    if terminal.agent_prompt_id == checkpoints[0].agent_prompt_id
                        && matches!(terminal.output_length_disposition,
                            tau_proto::OutputLengthDisposition::ContinuationTerminal {
                                outcome: tau_proto::OutputLengthContinuationOutcome::Cancelled,
                                outer_turn_finish_owed: true,
                                ..
                            })
            )).count(), 1);
        } else {
            assert_eq!(prompt_created_count(&h), 2);
            let successor = read_nth_prompt_created(&h, 1);
            assert_eq!(successor.agent_prompt_id, checkpoints[0].agent_prompt_id);
            let context = serde_json::to_string(&successor.context).expect("context");
            assert!(context.contains(tau_proto::OUTPUT_LENGTH_CONTINUATION_INSTRUCTION));
            assert!(!context.contains("body new"));
            assert!(!context.contains("later ordinary"));
            assert_eq!(
                h.agent_runtime.agent_registry.agents[&cid]
                    .dispatch
                    .pending_prompts
                    .iter()
                    .map(|prompt| prompt.text.as_str())
                    .collect::<Vec<_>>(),
                vec![":skill new", "later ordinary"],
            );
            // A subsequent ordinary tool-round fold can now consume both in
            // their original order using the installed catalog.
            h.fold_pending_prompts_as_steered(&cid);
            let ordinary = event_log_events(&h)
                .into_iter()
                .filter_map(|event| match event {
                    Event::AgentPromptSteered(steered)
                        if !steered
                            .text
                            .contains(tau_proto::OUTPUT_LENGTH_CONTINUATION_INSTRUCTION) =>
                    {
                        Some(steered.text)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(ordinary.len(), 2);
            assert!(ordinary[0].contains("body new"));
            assert_eq!(ordinary[1], "later ordinary");
        }
        h.shutdown().expect("shutdown");
    }
}

/// Hidden user eligibility is sampled at initial acceptance, not when a project
/// stops shadowing it. A combined legacy winner also remains eligible even when
/// the user-only resolver chose a different same-name candidate.
#[test]
fn workdir_refresh_preserves_complete_user_eligibility() {
    for (combined_legacy, initially_loadable) in [(false, true), (true, true), (false, false)] {
        let tmp = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(tmp.path()).expect("harness");
        connect_ready_configured_extension(&mut h, "one", "one", tau_proto::ClientKind::Tool);
        register_discovery_test_provider(&mut h, "one");
        let role = h.config.selected_role.clone();
        let cid = h.create_durable_user_agent(h.session_runtime.current_session_id.clone(), &role);
        let agent = durable_agent_id_for_conversation(&h, &cid);
        let init = h.prompt_coordination.context_discovery.pending_agents[&agent]
            .initialization_id
            .clone();
        let user = skill(&tmp.path().join("user.md"), "shared", None);
        let legacy = skill(&tmp.path().join("legacy.md"), "shared", None);
        let project = skill(&tmp.path().join("project.md"), "shared", None);
        if !initially_loadable {
            std::fs::remove_file(&user.file_path).expect("initially unavailable");
        }
        h.apply_agent_discovery_snapshot(
            &crate::test_connection_id("one"),
            tau_proto::ExtensionAgentDiscoverySnapshotDeclared {
                session_id: h.session_runtime.current_session_id.clone(),
                agent_id: agent.clone(),
                agent_initialization_id: init.clone(),
                workdir_binding: Some(tau_proto::DiscoveryWorkdirBinding {
                    metadata_key: tau_proto::AgentMetadataKey::new("ext_one_cwd"),
                    user_skills: vec![user.clone()],
                    user_candidates: vec![user.clone(), legacy.clone()],
                    retained_user_state: vec![1, 2, 3],
                    user_agents_files: Vec::new(),
                }),
                refresh_id: None,
                discovery_error: None,
                frontmatter_diagnostics: Vec::new(),
                skills: vec![if combined_legacy {
                    legacy.clone()
                } else {
                    project
                }],
                agents_files: Vec::new(),
            },
        );
        h.handle_extension_event_inner(
            &crate::test_connection_id("one"),
            Event::ExtensionContextReady(tau_proto::ExtensionContextReady {
                session_id: h.session_runtime.current_session_id.clone(),
                agent_id: agent.clone(),
                agent_initialization_id: init,
            }),
        )
        .expect("initial ready");
        let selected = if combined_legacy { legacy } else { user };
        if initially_loadable {
            std::fs::remove_file(&selected.file_path).expect("delete sampled user");
        } else {
            skill(&selected.file_path, "shared", None);
        }
        let revision = commit_cwd(&mut h, &agent, "one", "eligibility");
        refresh_reply(&mut h, &agent, "one", revision, vec![selected.clone()]);
        let frozen = &h.prompt_coordination.context_discovery.frozen_agents[&agent];
        assert_eq!(frozen.skills.contains_key("shared"), initially_loadable);
        if initially_loadable {
            assert_eq!(
                frozen.skills["shared"].source.label(),
                selected.file_path.display().to_string()
            );
            assert!(
                crate::harness::user_skill_invocation::read_user_invoked_skill_body(
                    &frozen.skills["shared"].source,
                )
                .is_err(),
                "later invocation still reads the live body"
            );
        }
        h.shutdown().expect("shutdown");
    }
}

/// A refresh cannot first sample a previously hidden foreign source's project
/// candidate: both readable-then-deleted and unreadable-then-repaired
/// eligibility stay unchanged until that foreign source itself refreshes.
#[test]
fn workdir_refresh_preserves_hidden_other_source_eligibility() {
    for initially_loadable in [true, false] {
        let tmp = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(tmp.path()).expect("harness");
        for source in ["one", "two"] {
            connect_ready_configured_extension(&mut h, source, source, tau_proto::ClientKind::Tool);
            register_discovery_test_provider(&mut h, source);
        }
        let role = h.config.selected_role.clone();
        let cid = h.create_durable_user_agent(h.session_runtime.current_session_id.clone(), &role);
        let agent = durable_agent_id_for_conversation(&h, &cid);
        let user = skill(&tmp.path().join("user.md"), "user", None);
        let winner = skill(&tmp.path().join("winner.md"), "shared", None);
        let hidden = skill(&tmp.path().join("hidden.md"), "shared", None);
        if !initially_loadable {
            std::fs::remove_file(&hidden.file_path).expect("initially unreadable");
        }
        initialize_source(&mut h, &agent, "one", user.clone(), winner.clone());
        initialize_source(&mut h, &agent, "two", user.clone(), hidden.clone());
        assert_eq!(
            h.prompt_coordination.context_discovery.frozen_agents[&agent].skills["shared"]
                .source
                .label(),
            winner.file_path.display().to_string(),
        );
        if initially_loadable {
            std::fs::remove_file(&hidden.file_path).expect("delete hidden foreign candidate");
        } else {
            skill(&hidden.file_path, "shared", None);
        }
        let revision = commit_cwd(&mut h, &agent, "one", "reveal-foreign");
        refresh_reply(&mut h, &agent, "one", revision, vec![user.clone()]);
        let frozen = &h.prompt_coordination.context_discovery.frozen_agents[&agent];
        assert_eq!(frozen.skills.contains_key("shared"), initially_loadable);
        if initially_loadable {
            assert_eq!(
                frozen.skills["shared"].source.label(),
                hidden.file_path.display().to_string(),
            );
        }
        // Only the foreign source's own refresh may observe its changed body.
        let revision = commit_cwd(&mut h, &agent, "two", "resample-foreign");
        refresh_reply(&mut h, &agent, "two", revision, vec![user, hidden]);
        assert_eq!(
            h.prompt_coordination.context_discovery.frozen_agents[&agent]
                .skills
                .contains_key("shared"),
            !initially_loadable,
        );
        h.shutdown().expect("shutdown");
    }
}

/// Install one source with distinct retained user and replaceable project
/// input.
fn initialize_source(
    h: &mut Harness,
    agent: &tau_proto::AgentId,
    source: &str,
    user: tau_proto::DiscoverySkillCandidate,
    project: tau_proto::DiscoverySkillCandidate,
) {
    let initialization_id = h.prompt_coordination.context_discovery.pending_agents[agent]
        .initialization_id
        .clone();
    h.apply_agent_discovery_snapshot(
        &crate::test_connection_id(source),
        tau_proto::ExtensionAgentDiscoverySnapshotDeclared {
            session_id: h.session_runtime.current_session_id.clone(),
            agent_id: agent.clone(),
            agent_initialization_id: initialization_id.clone(),
            workdir_binding: Some(tau_proto::DiscoveryWorkdirBinding {
                metadata_key: tau_proto::AgentMetadataKey::new(format!("ext_{source}_cwd")),
                user_skills: vec![user.clone()],
                user_candidates: vec![user.clone()],
                retained_user_state: vec![1, 2, 3],
                user_agents_files: vec![tau_proto::DiscoveryAgentsFile {
                    file_path: user.file_path.with_file_name("AGENTS.user.md"),
                    content: format!("user instructions {source}"),
                }],
            }),
            refresh_id: None,
            discovery_error: None,
            frontmatter_diagnostics: Vec::new(),
            skills: vec![user.clone(), project.clone()],
            agents_files: vec![
                tau_proto::DiscoveryAgentsFile {
                    file_path: user.file_path.with_file_name("AGENTS.user.md"),
                    content: format!("user instructions {source}"),
                },
                tau_proto::DiscoveryAgentsFile {
                    file_path: project.file_path.with_file_name("AGENTS.project.md"),
                    content: format!("old project instructions {source}"),
                },
            ],
        },
    );
    h.handle_extension_event_inner(
        &crate::test_connection_id(source),
        Event::ExtensionContextReady(tau_proto::ExtensionContextReady {
            session_id: h.session_runtime.current_session_id.clone(),
            agent_id: agent.clone(),
            agent_initialization_id: initialization_id,
        }),
    )
    .expect("initial readiness");
}

/// Commit the authoritative metadata fact, not the setter's requested path.
fn commit_cwd(h: &mut Harness, agent: &tau_proto::AgentId, source: &str, mutation: &str) -> u64 {
    h.publish_event(
        Some(crate::harness::harness_connection_id()),
        Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
            agent_id: agent.clone(),
            key: tau_proto::AgentMetadataKey::new(format!("ext_{source}_cwd")),
            value: tau_proto::CborValue::Text("/committed/path".to_owned()),
            mutation_id: Some(
                tau_proto::AgentMetadataMutationId::parse(mutation).expect("mutation"),
            ),
            inheritable: true,
        }),
    );
    h.prompt_coordination.context_discovery.pending_agents[agent].revision
}

/// Reply to one exact source scan without restarting the agent's load
/// lifecycle.
fn refresh_reply(
    h: &mut Harness,
    agent: &tau_proto::AgentId,
    source: &str,
    refresh_id: u64,
    skills: Vec<tau_proto::DiscoverySkillCandidate>,
) {
    let initialization_id = h.prompt_coordination.context_discovery.pending_agents[agent]
        .initialization_id
        .clone();
    h.apply_agent_discovery_snapshot(
        &crate::test_connection_id(source),
        tau_proto::ExtensionAgentDiscoverySnapshotDeclared {
            session_id: h.session_runtime.current_session_id.clone(),
            agent_id: agent.clone(),
            agent_initialization_id: initialization_id,
            workdir_binding: None,
            refresh_id: Some(refresh_id),
            discovery_error: None,
            frontmatter_diagnostics: Vec::new(),
            skills,
            agents_files: Vec::new(),
        },
    );
}

/// Same-path commits replace only their owning source; stale scans cannot
/// release readiness, and another loaded agent remains frozen independently.
#[test]
fn workdir_refresh_is_source_local_revisioned_and_agent_local() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    for source in ["one", "two"] {
        connect_ready_configured_extension(&mut h, source, source, tau_proto::ClientKind::Tool);
        register_discovery_test_provider(&mut h, source);
    }
    let role = h.config.selected_role.clone();
    let cid = h.create_durable_user_agent(h.session_runtime.current_session_id.clone(), &role);
    let other_cid =
        h.create_durable_user_agent(h.session_runtime.current_session_id.clone(), &role);
    let agent = durable_agent_id_for_conversation(&h, &cid);
    let other = durable_agent_id_for_conversation(&h, &other_cid);
    let user = skill(&tmp.path().join("user.md"), "user", None);
    let old = skill(&tmp.path().join("old.md"), "old", None);
    let foreign = skill(&tmp.path().join("foreign.md"), "foreign", None);
    for target in [&agent, &other] {
        initialize_source(&mut h, target, "one", user.clone(), old.clone());
        initialize_source(&mut h, target, "two", user.clone(), foreign.clone());
    }
    let load = h.prompt_coordination.context_discovery.frozen_agents[&agent]
        .initialization_id
        .clone();
    let first = commit_cwd(&mut h, &agent, "one", "first");
    let second = commit_cwd(&mut h, &agent, "one", "second");
    assert!(second > first);
    assert!(!h.agent_context_ready_for(&cid));
    assert!(h.agent_context_ready_for(&other_cid));
    refresh_reply(&mut h, &agent, "one", first, Vec::new());
    assert!(
        !h.agent_context_ready_for(&cid),
        "stale response cannot release barrier"
    );
    let new = skill(&tmp.path().join("new.md"), "new", None);
    refresh_reply(&mut h, &agent, "one", second, vec![user, new]);
    assert!(h.agent_context_ready_for(&cid));
    let frozen = &h.prompt_coordination.context_discovery.frozen_agents[&agent];
    assert_eq!(frozen.initialization_id, load);
    assert!(frozen.skills.contains_key("new"));
    assert!(frozen.skills.contains_key("foreign"));
    assert!(!frozen.skills.contains_key("old"));
    assert!(
        h.prompt_coordination.context_discovery.frozen_agents[&other]
            .skills
            .contains_key("old")
    );
    let context = h
        .session_runtime
        .agent_store
        .agent(agent.as_str())
        .expect("agent")
        .initialization_context()
        .expect("context");
    assert_eq!(context.discovery_revision, second);
    assert_eq!(context.discovery_refreshes.len(), 2);
    assert!(context.discovery_refreshes[0].error.is_some());
    assert!(context.discovery_refreshes[1].error.is_none());
    assert!(
        !context
            .agents_message
            .as_deref()
            .unwrap_or_default()
            .contains("old project instructions one")
    );
    assert!(
        context
            .agents_message
            .as_deref()
            .unwrap_or_default()
            .contains("old project instructions two")
    );
}

/// Timeout and disconnect remove stale project input, preserve sampled user
/// eligibility, and leave an established required-skill agent able to repair.
#[test]
fn workdir_refresh_degrades_required_skills_and_repairs_without_reinitialization() {
    for disconnect in [false, true] {
        let tmp = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(tmp.path()).expect("harness");
        connect_ready_configured_extension(&mut h, "one", "one", tau_proto::ClientKind::Tool);
        register_discovery_test_provider(&mut h, "one");
        let role = h.config.selected_role.clone();
        let cid = h.create_durable_user_agent(h.session_runtime.current_session_id.clone(), &role);
        let agent = durable_agent_id_for_conversation(&h, &cid);
        let user = skill(&tmp.path().join("user.md"), "user", None);
        let required = skill(&tmp.path().join("required.md"), "required", None);
        initialize_source(&mut h, &agent, "one", user.clone(), required.clone());
        h.config
            .available_roles
            .get_mut(&role)
            .expect("role")
            .required_skills = vec!["required".into()];
        std::fs::remove_file(&user.file_path).expect("remove user after initial sample");
        let revision = commit_cwd(&mut h, &agent, "one", "degrade");
        if disconnect {
            h.handle_disconnect(&crate::test_connection_id("one"));
        } else {
            h.process_discovery_refresh_deadlines(Instant::now() + Duration::from_secs(60));
        }
        assert!(h.agent_context_ready_for(&cid));
        assert!(h.agent_runtime.agent_registry.agents.contains_key(&cid));
        let frozen = &h.prompt_coordination.context_discovery.frozen_agents[&agent];
        assert!(
            frozen.skills.contains_key("user"),
            "unchanged user eligibility stays sampled"
        );
        assert!(!frozen.skills.contains_key("required"));
        let context = h
            .session_runtime
            .agent_store
            .agent(agent.as_str())
            .expect("agent")
            .initialization_context()
            .expect("context");
        assert_eq!(context.discovery_revision, revision);
        assert!(
            context
                .discovery_diagnostics
                .iter()
                .any(|message| message.contains("Required skills"))
        );
        assert!(context.discovery_refreshes[0].error.is_some());
        assert!(
            !context
                .agents_message
                .as_deref()
                .unwrap_or_default()
                .contains("old project")
        );
        assert!(
            context
                .agents_message
                .as_deref()
                .unwrap_or_default()
                .contains("user instructions")
        );
        if !disconnect {
            let repair = commit_cwd(&mut h, &agent, "one", "repair");
            refresh_reply(&mut h, &agent, "one", repair, vec![user, required]);
            let context = h
                .session_runtime
                .agent_store
                .agent(agent.as_str())
                .expect("agent")
                .initialization_context()
                .expect("context");
            assert!(context.discovery_diagnostics.is_empty());
            assert!(context.discovery_refreshes[0].error.is_none());
            assert!(
                h.prompt_coordination.context_discovery.frozen_agents[&agent]
                    .skills
                    .contains_key("required")
            );
        }
    }
}

/// Both retained full-prompt phases park outside the global queue. Installation
/// rebuilds only current discovery text and resumes the original owner once.
#[test]
fn workdir_refresh_resumes_exact_undelivered_prompt_in_both_phases() {
    for selector in [
        tau_proto::EventName::AGENT_PROMPT_STARTED,
        tau_proto::EventName::AGENT_PROMPT_CREATED,
    ] {
        let tmp = TempDir::new().expect("tempdir");
        let mut h = echo_harness(tmp.path()).expect("harness");
        connect_ready_configured_extension(&mut h, "one", "one", tau_proto::ClientKind::Tool);
        register_discovery_test_provider(&mut h, "one");
        let role = h.config.selected_role.clone();
        let cid = h.create_durable_user_agent(h.session_runtime.current_session_id.clone(), &role);
        let agent = durable_agent_id_for_conversation(&h, &cid);
        let user = skill(&tmp.path().join("user.md"), "user", None);
        let old = skill(&tmp.path().join("old.md"), "old", None);
        initialize_source(&mut h, &agent, "one", user.clone(), old);
        let interceptor = connect_test_tool(&mut h, "prompt-holder");
        h.handle_extension_event(
            "prompt-holder",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(selector)],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("intercept");
        h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("unchanged history".to_owned()))
            .expect("dispatch");

        // Capture the exact live envelope and feed it through the first-entry
        // seam after a canonical cwd mutation. This models a retained phase
        // queued behind that mutation, not a restart or a re-materialization.
        let parked = h
            .runtime_io
            .publication
            .pending_intercept
            .take()
            .expect("retained phase");
        let original = match &parked.event {
            Event::AgentPromptCreated(prompt) => prompt.clone(),
            Event::AgentPromptStarted(_) => {
                let sync = parked.sync_head_for.as_ref().expect("owner");
                let Some(PostCommitContinuation::PromptMaterialization(continuation)) =
                    &sync.continuation
                else {
                    panic!("materialization continuation");
                };
                continuation.prompt.as_ref().clone()
            }
            _ => panic!("prompt phase"),
        };
        let revision = commit_cwd(&mut h, &agent, "one", "prompt-refresh");
        h.enqueue_publish(
            None,
            parked.event,
            parked.persist,
            parked.must_pass,
            parked.sync_head_for,
        );
        assert!(
            h.runtime_io.publication.pending_intercept.is_none(),
            "refresh traffic remains unblocked"
        );
        assert!(
            h.prompt_coordination
                .context_discovery
                .parked_prompts
                .contains_key(&agent)
        );
        let new = skill(&tmp.path().join("new.md"), "new", None);
        refresh_reply(&mut h, &agent, "one", revision, vec![user, new]);
        assert!(
            !h.prompt_coordination
                .context_discovery
                .parked_prompts
                .contains_key(&agent)
        );
        h.handle_extension_event(
            "prompt-holder",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("release resumed phase");
        let delivered = read_nth_prompt_created(&h, 0);
        assert_eq!(delivered.agent_prompt_id, original.agent_prompt_id);
        assert_eq!(delivered.ctx_id, original.ctx_id);
        assert_eq!(delivered.model, original.model);
        assert_eq!(delivered.model_params, original.model_params);
        assert_eq!(delivered.tools, original.tools);
        assert_eq!(delivered.compaction, original.compaction);
        assert_eq!(
            delivered.local_summary_continuation,
            original.local_summary_continuation
        );
        assert_eq!(
            delivered.context.blocks,
            original.context.blocks[1..],
            "only the owned old bootstrap is removed"
        );
        assert!(delivered.system_prompt.contains("<name>new</name>"));
        assert!(!delivered.system_prompt.contains("<name>old</name>"));
        assert_eq!(event_log_events(&h).iter().filter(|event| matches!(
            event, Event::AgentPromptStarted(started) if started.agent_prompt_id == original.agent_prompt_id
        )).count(), 1);
        assert!(
            h.prompt_coordination
                .context_discovery
                .parked_materializations
                .is_empty()
        );
        assert!(
            !h.prompt_coordination
                .prompt_runtime
                .pending_dispatches
                .contains(&original.agent_prompt_id)
        );
        // Duplicate readiness cannot reconstruct a consumed continuation.
        h.resume_discovery_dispatches(&agent);
        assert_eq!(event_log_events(&h).iter().filter(|event| matches!(
            event, Event::AgentPromptCreated(created) if created.agent_prompt_id == original.agent_prompt_id
        )).count(), 1);
        drop(interceptor);
        h.shutdown().expect("shutdown");
    }
}

/// Supervisor replacement changes only source ownership. Old-generation replies
/// remain inert, while a same-path setter can repair with the replacement
/// shell.
#[test]
fn workdir_refresh_reconnect_rebinds_retained_source_and_allows_repair() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(&mut h, "one", "one", tau_proto::ClientKind::Tool);
    register_discovery_test_provider(&mut h, "one");
    let role = h.config.selected_role.clone();
    let cid = h.create_durable_user_agent(h.session_runtime.current_session_id.clone(), &role);
    let agent = durable_agent_id_for_conversation(&h, &cid);
    let user = skill(&tmp.path().join("user.md"), "user", None);
    let project = skill(&tmp.path().join("project.md"), "project", None);
    initialize_source(&mut h, &agent, "one", user.clone(), project.clone());
    let load = h.prompt_coordination.context_discovery.frozen_agents[&agent]
        .initialization_id
        .clone();
    h.handle_disconnect(&crate::test_connection_id("one"));
    connect_ready_configured_extension(&mut h, "replacement", "one", tau_proto::ClientKind::Tool);
    h.rebind_workdir_discovery_source(
        &crate::test_connection_id("one"),
        &crate::test_connection_id("replacement"),
    );
    let frozen = &h.prompt_coordination.context_discovery.frozen_agents[&agent];
    assert_eq!(frozen.initialization_id, load);
    assert_eq!(
        frozen.skills["user"].source_id,
        crate::test_connection_id("replacement")
    );
    assert!(
        !frozen
            .inputs
            .workdir_sources
            .contains_key(&crate::test_connection_id("one"))
    );
    let revision = commit_cwd(&mut h, &agent, "one", "reconnect-repair");
    let original_binding = &h.prompt_coordination.context_discovery.pending_agents[&agent]
        .workdir_sources[&crate::test_connection_id("replacement")];
    assert_eq!(original_binding.retained_user_state, vec![1, 2, 3]);
    assert_eq!(
        original_binding.user_candidates[0].1.source_id,
        crate::test_connection_id("replacement")
    );
    std::fs::remove_file(&user.file_path).expect("remove original user after reconnect");
    h.apply_agent_discovery_snapshot(
        &crate::test_connection_id("replacement"),
        tau_proto::ExtensionAgentDiscoverySnapshotDeclared {
            session_id: h.session_runtime.current_session_id.clone(),
            agent_id: agent.clone(),
            agent_initialization_id: load,
            workdir_binding: Some(tau_proto::DiscoveryWorkdirBinding {
                metadata_key: tau_proto::AgentMetadataKey::new("ext_one_cwd"),
                user_skills: Vec::new(),
                user_candidates: Vec::new(),
                retained_user_state: vec![9],
                user_agents_files: Vec::new(),
            }),
            refresh_id: None,
            discovery_error: None,
            frontmatter_diagnostics: Vec::new(),
            skills: Vec::new(),
            agents_files: Vec::new(),
        },
    );
    assert_eq!(
        h.prompt_coordination.context_discovery.pending_agents[&agent].workdir_sources
            [&crate::test_connection_id("replacement")]
            .retained_user_state,
        vec![1, 2, 3]
    );
    refresh_reply(&mut h, &agent, "one", revision, Vec::new());
    assert!(!h.agent_context_ready_for(&cid));
    refresh_reply(
        &mut h,
        &agent,
        "replacement",
        revision,
        vec![user.clone(), project],
    );
    assert!(h.agent_context_ready_for(&cid));
    assert!(
        h.prompt_coordination
            .context_discovery
            .initialized_agent_context[&agent]
            .discovery_diagnostics
            .is_empty()
    );
    assert!(
        h.prompt_coordination.context_discovery.frozen_agents[&agent]
            .skills
            .contains_key("user")
    );
    commit_cwd(&mut h, &agent, "one", "reconnect-timeout");
    h.process_discovery_refresh_deadlines(Instant::now() + Duration::from_secs(60));
    assert!(h.agent_context_ready_for(&cid));
    assert_eq!(
        h.prompt_coordination.context_discovery.frozen_agents[&agent].skills["user"]
            .source
            .label(),
        user.file_path.display().to_string(),
    );
    h.shutdown().expect("shutdown");
}

/// A resume's startup-cwd session inventory cannot prune roles before restored
/// agents have declared their own remembered-project skills.
#[test]
fn workdir_refresh_resume_defers_required_role_checks_to_agent_discovery() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let role = h.config.selected_role.clone();
    h.config
        .available_roles
        .get_mut(&role)
        .expect("role")
        .required_skills = vec!["restored-only".into()];
    assert!(
        h.required_skill_unavailable_reason(&"restored-only".into(), &role)
            .is_some()
    );
    h.complete_session_init(
        h.session_runtime.current_session_id.clone(),
        tau_proto::SessionStartReason::Resume,
    )
    .expect("resume must await per-agent project");
    assert_eq!(
        h.config.available_roles[&role].required_skills,
        vec![tau_proto::SkillName::from("restored-only")]
    );
    connect_ready_configured_extension(&mut h, "one", "one", tau_proto::ClientKind::Tool);
    register_discovery_test_provider(&mut h, "one");
    let cid = h.create_durable_user_agent(h.session_runtime.current_session_id.clone(), &role);
    let agent = durable_agent_id_for_conversation(&h, &cid);
    initialize_source(
        &mut h,
        &agent,
        "one",
        skill(&tmp.path().join("user.md"), "user", None),
        skill(&tmp.path().join("restored-only.md"), "restored-only", None),
    );
    assert!(h.agent_context_ready_for(&cid));
    assert!(
        h.prompt_coordination.context_discovery.frozen_agents[&agent]
            .skills
            .contains_key("restored-only")
    );
}

/// Definite capacity rejection retains one approved replacement, without
/// releasing readiness or rerunning interceptors; a newer mutation supersedes
/// it.
#[test]
fn workdir_refresh_retries_only_current_capacity_rejected_install() {
    for supersede in [false, true] {
        let tmp = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(tmp.path()).expect("harness");
        connect_ready_configured_extension(&mut h, "one", "one", tau_proto::ClientKind::Tool);
        register_discovery_test_provider(&mut h, "one");
        let role = h.config.selected_role.clone();
        let cid = h.create_durable_user_agent(h.session_runtime.current_session_id.clone(), &role);
        let agent = durable_agent_id_for_conversation(&h, &cid);
        let user = skill(&tmp.path().join("user.md"), "user", None);
        initialize_source(
            &mut h,
            &agent,
            "one",
            user.clone(),
            skill(&tmp.path().join("old.md"), "old", None),
        );
        connect_snapshot_interceptor(
            &mut h,
            tau_proto::EventName::AGENT_INITIALIZATION_CONTEXT_SET,
        );
        let revision = commit_cwd(&mut h, &agent, "one", "capacity");
        refresh_reply(&mut h, &agent, "one", revision, vec![user.clone()]);
        assert!(h.runtime_io.publication.pending_intercept.is_some());
        reject_next_semantic_admission(&h);
        h.handle_extension_event(
            "snapshot-interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("definite capacity rejection");
        assert!(
            h.prompt_coordination.context_discovery.pending_agents[&agent]
                .retained_install
                .is_some()
        );
        assert!(!h.agent_context_ready_for(&cid));
        assert_eq!(
            h.prompt_coordination
                .context_discovery
                .initialized_agent_context[&agent]
                .discovery_revision,
            0
        );
        let installed_revision = if supersede {
            let newer = commit_cwd(&mut h, &agent, "one", "newer");
            h.handle_publication_capacity_ready();
            assert!(!h.agent_context_ready_for(&cid));
            assert!(h.runtime_io.publication.pending_intercept.is_none());
            refresh_reply(&mut h, &agent, "one", newer, vec![user]);
            h.handle_extension_event(
                "snapshot-interceptor",
                TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                    action: InterceptAction::Pass(None),
                })),
            )
            .expect("install newer");
            newer
        } else {
            h.handle_publication_capacity_ready();
            assert!(
                h.runtime_io.publication.pending_intercept.is_none(),
                "retry skips completed interception"
            );
            revision
        };
        h.handle_publication_capacity_ready();
        assert!(h.agent_context_ready_for(&cid));
        assert_eq!(
            h.prompt_coordination
                .context_discovery
                .initialized_agent_context[&agent]
                .discovery_revision,
            installed_revision
        );
        let records = h
            .session_runtime
            .agent_store
            .snapshot_agent_events_for_test(agent.as_str())
            .expect("records");
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(&record.event,
                    Event::AgentInitializationContextSet(context) if context.discovery_revision > 0
                ))
                .count(),
            1
        );
        let cold = tau_core::AgentTree::from_events(agent.clone(), &records);
        assert_eq!(
            cold.initialization_context(),
            h.session_runtime
                .agent_store
                .agent(agent.as_str())
                .expect("agent")
                .initialization_context()
        );
        assert!(
            !cold
                .initialization_context()
                .expect("cold context")
                .agents_message
                .as_deref()
                .unwrap_or_default()
                .contains("old project")
        );
    }
}

/// A UI skill command admitted during the scan waits for the latest catalog,
/// including a skill that did not exist in the previous project.
#[test]
fn workdir_refresh_defers_selected_agent_skill_expansion() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(&mut h, "one", "one", tau_proto::ClientKind::Tool);
    register_discovery_test_provider(&mut h, "one");
    let role = h.config.selected_role.clone();
    let cid = h.create_durable_user_agent(h.session_runtime.current_session_id.clone(), &role);
    let agent = durable_agent_id_for_conversation(&h, &cid);
    let user = skill(&tmp.path().join("user.md"), "user", None);
    initialize_source(
        &mut h,
        &agent,
        "one",
        user.clone(),
        skill(&tmp.path().join("old.md"), "old", None),
    );
    let revision = commit_cwd(&mut h, &agent, "one", "skill");
    h.handle_authenticated_ui_prompt_submitted(
        crate::harness::harness_connection_id(),
        tau_proto::UiPromptSubmitted {
            literal: false,
            session_id: h.session_runtime.current_session_id.clone(),
            text: ":skill new arguments".to_owned(),
            agent_id: agent.clone(),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        },
    )
    .expect("queue skill");
    assert_eq!(prompt_created_count(&h), 0);
    refresh_reply(
        &mut h,
        &agent,
        "one",
        revision,
        vec![user, skill(&tmp.path().join("new.md"), "new", None)],
    );
    let prompt = read_nth_prompt_created(&h, 0);
    let context = serde_json::to_string(&prompt.context).expect("context");
    assert!(context.contains("body new"));
    assert!(!context.contains("old project instructions"));
    assert!(!context.contains(":skill new"));
    h.shutdown().expect("shutdown");
}

/// A committed checkpoint paused before full preparation resumes its exact
/// callback, rather than scanning durable uncertain owners or creating another.
#[test]
fn workdir_refresh_retains_pre_materialization_checkpoint_callback() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(&mut h, "one", "one", tau_proto::ClientKind::Tool);
    register_discovery_test_provider(&mut h, "one");
    let role = h.config.selected_role.clone();
    let cid = h.create_durable_user_agent(h.session_runtime.current_session_id.clone(), &role);
    let agent = durable_agent_id_for_conversation(&h, &cid);
    let user = skill(&tmp.path().join("user.md"), "user", None);
    initialize_source(
        &mut h,
        &agent,
        "one",
        user.clone(),
        skill(&tmp.path().join("old.md"), "old", None),
    );
    connect_snapshot_interceptor(
        &mut h,
        tau_proto::EventName::AGENT_INFERENCE_DISPATCH_STARTED,
    );
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("checkpoint".to_owned()))
        .expect("dispatch");
    let parked = h
        .runtime_io
        .publication
        .pending_intercept
        .take()
        .expect("checkpoint");
    let Event::AgentInferenceDispatchStarted(checkpoint) = &parked.event else {
        panic!("checkpoint");
    };
    let prompt_id = checkpoint.agent_prompt_id.clone();
    let revision = commit_cwd(&mut h, &agent, "one", "checkpoint");
    h.enqueue_publish(
        None,
        parked.event,
        parked.persist,
        parked.must_pass,
        parked.sync_head_for,
    );
    h.handle_extension_event(
        "snapshot-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit checkpoint");
    assert_eq!(prompt_created_count(&h), 0);
    assert!(
        h.prompt_coordination
            .context_discovery
            .parked_materializations
            .contains_key(&agent)
    );
    assert!(!h.uncertain_supersession_is_eligible(&cid, &prompt_id));
    h.handle_authenticated_ui_prompt_submitted(
        crate::harness::harness_connection_id(),
        tau_proto::UiPromptSubmitted {
            literal: true,
            session_id: h.session_runtime.current_session_id.clone(),
            text: "second input while discovery waits".to_owned(),
            agent_id: agent.clone(),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        },
    )
    .expect("queue without superseding live checkpoint");
    assert!(!h.uncertain_supersession_is_eligible(&cid, &prompt_id));
    refresh_reply(&mut h, &agent, "one", revision, vec![user]);
    assert_eq!(read_nth_prompt_created(&h, 0).agent_prompt_id, prompt_id);
    assert!(
        h.prompt_coordination
            .context_discovery
            .parked_materializations
            .is_empty()
    );
    h.resume_discovery_dispatches(&agent);
    assert_eq!(prompt_created_count(&h), 1);
    h.shutdown().expect("shutdown");
}

/// Terminal persistence failure cannot be mistaken for installed discovery or
/// authorize retries after an ambiguous/unavailable durable boundary.
#[test]
fn workdir_refresh_hard_persistence_failure_never_acknowledges_installation() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(&mut h, "one", "one", tau_proto::ClientKind::Tool);
    register_discovery_test_provider(&mut h, "one");
    let role = h.config.selected_role.clone();
    let cid = h.create_durable_user_agent(h.session_runtime.current_session_id.clone(), &role);
    let agent = durable_agent_id_for_conversation(&h, &cid);
    let user = skill(&tmp.path().join("user.md"), "user", None);
    initialize_source(
        &mut h,
        &agent,
        "one",
        user.clone(),
        skill(&tmp.path().join("old.md"), "old", None),
    );
    let revision = commit_cwd(&mut h, &agent, "one", "hard-failure");
    h.session_runtime
        .persistence_owner
        .as_ref()
        .expect("owner")
        .fail_stop();
    refresh_reply(&mut h, &agent, "one", revision, vec![user]);
    assert!(!h.agent_context_ready_for(&cid));
    assert!(
        h.prompt_coordination.context_discovery.pending_agents[&agent]
            .retained_install
            .is_none()
    );
    h.handle_publication_capacity_ready();
    assert_eq!(
        h.prompt_coordination
            .context_discovery
            .initialized_agent_context[&agent]
            .discovery_revision,
        0
    );
    assert!(
        h.runtime_io
            .replayable_harness_notices
            .iter()
            .any(
                |notice| notice.message.contains("agent.initialization_context_set")
                    && notice.message.contains("rejected")
            )
    );
}

/// Cold resume retains roles whose requirements exist only in their restored
/// project, then degrades an unavailable project without unloading the agent.
#[test]
fn workdir_refresh_cold_resume_preserves_required_role_and_allows_repair() {
    let tmp = TempDir::new().expect("tempdir");
    let state = tmp.path().join("state");
    let mut original = echo_harness(&state).expect("harness");
    connect_ready_configured_extension(&mut original, "one", "one", tau_proto::ClientKind::Tool);
    register_discovery_test_provider(&mut original, "one");
    let role = original.config.selected_role.clone();
    let cid = original
        .create_durable_user_agent(original.session_runtime.current_session_id.clone(), &role);
    let agent = durable_agent_id_for_conversation(&original, &cid);
    let user = skill(&tmp.path().join("user.md"), "user", None);
    let required = skill(&tmp.path().join("required.md"), "required", None);
    initialize_source(&mut original, &agent, "one", user.clone(), required.clone());
    original.shutdown().expect("joined shutdown");
    drop(original);
    let restored_role = role.clone();
    // The convenience echo fixture force-finalizes all discovery waits; use its
    // underlying constructor to retain the real restored-source barrier.
    let mut resumed = Harness::new_with_provider_and_internal_tools(
        &state,
        tau_config::settings::TauDirs {
            config_dir: Some(state.join("config")),
            state_dir: Some(state.join("runtime")),
        },
        echo_runner,
        Vec::new(),
        crate::harness::TestProviderHarnessStartup {
            session_id: "s1",
            reason: tau_proto::SessionStartReason::Resume,
            storage_mode: crate::HarnessStorageMode::Durable,
            internal_tool_handlers: Vec::new(),
            before_session_init: Some(Box::new(move |h| {
                h.config
                    .available_roles
                    .get_mut(&restored_role)
                    .expect("role")
                    .required_skills = vec!["required".into()];
                connect_ready_configured_extension(h, "one", "one", tau_proto::ClientKind::Tool);
                register_discovery_test_provider(h, "one");
            })),
        },
    )
    .expect("resume with session catalog missing required project skill");
    let restored_cid = resumed
        .runtime_agent_id_for_target_agent(Some(agent.as_str()))
        .expect("restored route");
    assert!(resumed.config.available_roles.contains_key(&role));
    assert!(
        resumed
            .prompt_coordination
            .context_discovery
            .pending_agents
            .contains_key(&agent)
    );
    let init = resumed.prompt_coordination.context_discovery.pending_agents[&agent]
        .initialization_id
        .clone();
    resumed.apply_agent_discovery_snapshot(
        &crate::test_connection_id("one"),
        tau_proto::ExtensionAgentDiscoverySnapshotDeclared {
            session_id: resumed.session_runtime.current_session_id.clone(),
            agent_id: agent.clone(),
            agent_initialization_id: init.clone(),
            workdir_binding: Some(tau_proto::DiscoveryWorkdirBinding {
                metadata_key: tau_proto::AgentMetadataKey::new("ext_one_cwd"),
                user_skills: vec![user.clone()],
                user_candidates: vec![user.clone()],
                retained_user_state: vec![1, 2, 3],
                user_agents_files: Vec::new(),
            }),
            refresh_id: None,
            discovery_error: Some("restored project unavailable".to_owned()),
            frontmatter_diagnostics: Vec::new(),
            skills: Vec::new(),
            agents_files: Vec::new(),
        },
    );
    resumed
        .handle_extension_event_inner(
            &crate::test_connection_id("one"),
            Event::ExtensionContextReady(tau_proto::ExtensionContextReady {
                session_id: resumed.session_runtime.current_session_id.clone(),
                agent_id: agent.clone(),
                agent_initialization_id: init,
            }),
        )
        .expect("degraded restored readiness");
    assert!(
        resumed
            .agent_runtime
            .agent_registry
            .agents
            .contains_key(&restored_cid)
    );
    assert!(resumed.agent_context_ready_for(&restored_cid));
    assert!(
        resumed
            .prompt_coordination
            .context_discovery
            .initialized_agent_context[&agent]
            .discovery_diagnostics
            .iter()
            .any(|message| message.contains("Required skills"))
    );
    let repair = commit_cwd(&mut resumed, &agent, "one", "cold-repair");
    refresh_reply(&mut resumed, &agent, "one", repair, vec![user, required]);
    assert!(
        resumed
            .prompt_coordination
            .context_discovery
            .initialized_agent_context[&agent]
            .discovery_diagnostics
            .is_empty()
    );
    resumed.shutdown().expect("shutdown");
}
