//! Tests for rendered previews behavior.

use super::*;

/// Cancelling a disconnected preview requester must discard its waiting
/// ephemeral agent so a context provider that never becomes ready cannot leak
/// runtime routes or deferred response state.
#[test]
fn disconnect_cancels_pending_rendered_preview() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let requester = crate::test_connection_id("preview-disconnect-requester");
    let _frames = connect_test_client(&mut h, requester.as_str(), tau_proto::ClientKind::Ui);
    let cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.prompt_coordination
        .context_discovery
        .pending_rendered_prompts
        .insert(
            agent_id.clone(),
            PendingRenderedPreview {
                requests: vec![PendingRenderedPrompt::Prompt {
                    connection_id: requester.clone(),
                    request_id: "preview-disconnect".to_owned(),
                    role: h.config.selected_role.clone(),
                    enable_agents_md: false,
                }],
                deadline: Instant::now(),
            },
        );

    h.handle_disconnect_at(&requester, Instant::now());

    assert!(
        !h.prompt_coordination
            .context_discovery
            .pending_rendered_prompts
            .contains_key(&agent_id)
    );
    assert!(
        h.runtime_agent_id_for_target_agent(Some(agent_id.as_str()))
            .is_none()
    );
}

/// Replacing a session must cancel previews that are still waiting for
/// extension context, rather than retaining an unanswerable response entry.
#[test]
fn session_switch_cancels_pending_rendered_preview() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.prompt_coordination
        .context_discovery
        .pending_rendered_prompts
        .insert(
            agent_id.clone(),
            PendingRenderedPreview {
                requests: vec![PendingRenderedPrompt::System {
                    connection_id: crate::test_connection_id("preview-switch-requester"),
                    request_id: "preview-switch".to_owned(),
                    role: h.config.selected_role.clone(),
                }],
                deadline: Instant::now(),
            },
        );

    h.switch_session(
        "preview-replacement".parse().expect("session id"),
        tau_proto::SessionStartReason::New,
    )
    .expect("switch session");

    assert!(
        !h.prompt_coordination
            .context_discovery
            .pending_rendered_prompts
            .contains_key(&agent_id)
    );
    assert!(
        h.runtime_agent_id_for_target_agent(Some(agent_id.as_str()))
            .is_none()
    );
}

/// An extension that never completes per-agent context must hit the bounded
/// preview deadline and release both request state and the temporary agent.
#[test]
fn rendered_preview_context_timeout_cleans_up_agent() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    let deadline = Instant::now();
    h.prompt_coordination
        .context_discovery
        .pending_rendered_prompts
        .insert(
            agent_id.clone(),
            PendingRenderedPreview {
                requests: vec![PendingRenderedPrompt::Tools {
                    connection_id: crate::test_connection_id("preview-timeout-requester"),
                    request_id: "preview-timeout".to_owned(),
                    role: h.config.selected_role.clone(),
                }],
                deadline,
            },
        );

    h.process_rendered_preview_deadlines(deadline);

    assert!(
        !h.prompt_coordination
            .context_discovery
            .pending_rendered_prompts
            .contains_key(&agent_id)
    );
    assert!(
        h.runtime_agent_id_for_target_agent(Some(agent_id.as_str()))
            .is_none()
    );
}
