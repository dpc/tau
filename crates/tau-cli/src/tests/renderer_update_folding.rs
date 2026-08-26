//! Renderer equivalence oracles for dequeue-time provider update folding.

use super::*;

/// Builds one update from explicit message indices to exercise indexed joins.
fn indexed_update(prompt: &str, deltas: &[(u32, &str)]) -> ProviderResponseUpdated {
    ProviderResponseUpdated {
        agent_prompt_id: test_agent_prompt_id(prompt),
        agent_id: agent_id("main"),
        deltas: deltas
            .iter()
            .map(
                |(output_index, text)| tau_proto::ProviderResponseTextDelta::Message {
                    output_index: *output_index,
                    text: (*text).to_owned(),
                    phase: None,
                },
            )
            .collect(),
        compaction: None,
        status: None,
        response_stats: None,
        originator: tau_proto::PromptOriginator::User,
    }
}

/// Renders updates and returns both the visible screen and editor response.
fn render_updates(updates: &[ProviderResponseUpdated]) -> (Vec<String>, Option<String>) {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-fold", "s1",
    )));
    for update in updates {
        renderer.handle(&Event::ProviderResponseUpdated(update.clone()));
    }
    sync(&handle);
    let editor_response = renderer
        .editor_context()
        .lock()
        .expect("editor context")
        .current_response
        .clone();
    (vt.screen_text(80), editor_response)
}

/// Folding preserves final visible/editor state across multiple indices, index
/// gaps, and a middle insertion that changes append into replacement work.
#[test]
fn folded_indexed_deltas_match_sequential_projection() {
    let sequential = vec![
        indexed_update("sp-fold", &[(0, "zero"), (3, "three")]),
        indexed_update("sp-fold", &[(2, "two")]),
        indexed_update("sp-fold", &[(0, "+"), (5, "five")]),
    ];
    let folded = indexed_update(
        "sp-fold",
        &[(0, "zero"), (3, "three"), (2, "two"), (0, "+"), (5, "five")],
    );

    assert_eq!(render_updates(&sequential), render_updates(&[folded]));
}

/// A folded first-visible update retains the late-subscription ellipsis rather
/// than pretending the renderer observed the missing prefix.
#[test]
fn folded_unknown_prompt_retains_ellipsis_prefix() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::ProviderResponseUpdated(indexed_update(
        "sp-unknown",
        &[(0, "hello"), (0, " world")],
    )));
    sync(&handle);

    assert!(vt.screen_contains(80, "…hello world"));
}

/// Folding cannot bypass private standalone-compaction suppression because the
/// combined update still follows the owning prompt's ordinary renderer guard.
#[test]
fn folded_standalone_compaction_output_remains_suppressed() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::AgentPromptStarted(
        standalone_compaction_prompt_started("sp-private"),
    ));
    renderer.handle(&Event::ProviderResponseUpdated(indexed_update(
        "sp-private",
        &[(0, "private"), (0, " output")],
    )));
    sync(&handle);

    assert!(!vt.screen_contains(80, "private output"));
}

/// Folding stats-only stale updates after a terminal does not recreate a live
/// response block or replace the durable final response.
#[test]
fn folded_stale_post_terminal_stats_remain_suppressed() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-stale", "s1",
    )));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-stale",
        vec![assistant_message_item("durable")],
    )));
    let mut folded = main_provider_response_stats_update("sp-stale", 12 * 1024, 0);
    folded
        .response_stats
        .as_mut()
        .expect("response stats")
        .first_semantic_output_elapsed_micros = Some(500_000);
    renderer.handle(&Event::ProviderResponseUpdated(folded));
    sync(&handle);

    assert!(vt.screen_contains(80, "durable"));
    assert!(!vt.screen_contains(80, "12KB"));
}

/// A folded hidden-agent update mutates only the detached owning transcript and
/// becomes visible after selection, without painting into the current agent.
#[test]
fn folded_hidden_agent_output_stays_in_owning_transcript() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("visible".to_owned());
    let mut prompt = agent_prompt_created("sp-hidden", "s1");
    prompt.agent_id = agent_id("hidden");
    renderer.handle(&Event::AgentPromptCreated(prompt));
    let mut folded = indexed_update("sp-hidden", &[(0, "hidden"), (0, " text")]);
    folded.agent_id = agent_id("hidden");
    renderer.handle(&Event::ProviderResponseUpdated(folded));
    sync(&handle);
    assert!(!vt.screen_contains(80, "hidden text"));

    renderer.switch_agent("hidden".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(80, "hidden text"));
}
