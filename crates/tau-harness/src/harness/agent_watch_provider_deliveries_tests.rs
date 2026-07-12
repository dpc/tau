use super::AgentWatchProviderDeliveryKind;
use super::agent_watch_provider_deliveries::{
    AgentWatchProviderDeliveries, MAX_TRACKED_PROVIDER_STATUS_PROMPTS,
};

fn retrying_transport() -> AgentWatchProviderDeliveryKind {
    AgentWatchProviderDeliveryKind::Retrying(tau_proto::AgentWatchProviderCategory::Transport)
}

/// A delivery bucket must reject older generations without mutation and reset
/// all retained prompt state only when a newer generation arrives.
#[test]
fn provider_status_delivery_generations_are_monotonic_and_reset_forward() {
    let mut deliveries = AgentWatchProviderDeliveries::default();
    let first_prompt = tau_proto::AgentPromptId::from("sp-generation-first");
    assert!(
        deliveries
            .record(7, &first_prompt, retrying_transport())
            .should_deliver
    );

    let stale = deliveries.record(
        6,
        &tau_proto::AgentPromptId::from("sp-generation-stale"),
        retrying_transport(),
    );
    assert!(!stale.should_deliver);
    assert!(stale.stale_generation);
    assert_eq!(deliveries.prompt_count(), 1);
    assert_eq!(deliveries.delivery_key_count(), 1);

    let newer_prompt = tau_proto::AgentPromptId::from("sp-generation-newer");
    let newer = deliveries.record(8, &newer_prompt, retrying_transport());
    assert!(newer.should_deliver);
    assert!(!newer.stale_generation);
    assert_eq!(deliveries.prompt_count(), 1);
    assert_eq!(deliveries.delivery_key_count(), 1);
    assert!(
        deliveries
            .record(8, &first_prompt, retrying_transport())
            .should_deliver,
        "the newer generation must not inherit old-generation dedupe keys"
    );
}

/// Capacity handling must retain all active keys for an untracked terminal,
/// suppress their duplicates, and evict the oldest prompt first only when a
/// new nonterminal prompt needs admission.
#[test]
fn provider_status_delivery_capacity_is_fifo_and_terminals_do_not_consume_it() {
    let mut deliveries = AgentWatchProviderDeliveries::default();
    for index in 0..MAX_TRACKED_PROVIDER_STATUS_PROMPTS {
        assert!(
            deliveries
                .record(
                    3,
                    &tau_proto::AgentPromptId::from(format!("sp-capacity-{index}")),
                    retrying_transport(),
                )
                .should_deliver
        );
    }

    let terminal = deliveries.record(
        3,
        &tau_proto::AgentPromptId::from("sp-untracked-terminal"),
        AgentWatchProviderDeliveryKind::TerminalError(
            tau_proto::ProviderFailureKind::RequestRejected,
        ),
    );
    assert!(terminal.should_deliver);
    assert!(terminal.terminal_retired);
    assert!(!terminal.capacity_evicted);
    assert_eq!(
        deliveries.prompt_count(),
        MAX_TRACKED_PROVIDER_STATUS_PROMPTS
    );
    assert!(
        !deliveries
            .record(
                3,
                &tau_proto::AgentPromptId::from("sp-capacity-0"),
                retrying_transport(),
            )
            .should_deliver,
        "untracked terminal delivery must not evict the oldest active prompt"
    );

    let overflow = deliveries.record(
        3,
        &tau_proto::AgentPromptId::from("sp-capacity-overflow"),
        retrying_transport(),
    );
    assert!(overflow.should_deliver);
    assert!(overflow.capacity_evicted);
    assert!(
        !deliveries
            .record(
                3,
                &tau_proto::AgentPromptId::from("sp-capacity-1"),
                retrying_transport(),
            )
            .should_deliver,
        "the second-oldest prompt must remain after one FIFO eviction"
    );
    assert!(
        deliveries
            .record(
                3,
                &tau_proto::AgentPromptId::from("sp-capacity-0"),
                retrying_transport(),
            )
            .should_deliver,
        "the oldest prompt must be the first capacity eviction"
    );
    assert_eq!(
        deliveries.prompt_count(),
        MAX_TRACKED_PROVIDER_STATUS_PROMPTS
    );
}

/// Thousands of serial prompt identities in one generation must retain exact
/// first/category-change/terminal decisions without allowing either terminal or
/// nonterminal bookkeeping cardinality to grow without bound.
#[test]
fn provider_status_delivery_stays_bounded_across_a_long_high_cardinality_turn() {
    const HIGH_CARDINALITY_PROMPTS: usize = 4_096;

    let mut deliveries = AgentWatchProviderDeliveries::default();
    for index in 0..HIGH_CARDINALITY_PROMPTS {
        let prompt = tau_proto::AgentPromptId::from(format!("sp-terminal-stream-{index}"));
        assert!(
            deliveries
                .record(11, &prompt, retrying_transport())
                .should_deliver
        );
        assert!(
            !deliveries
                .record(11, &prompt, retrying_transport())
                .should_deliver
        );
        assert!(
            deliveries
                .record(
                    11,
                    &prompt,
                    AgentWatchProviderDeliveryKind::Blocked(
                        tau_proto::AgentWatchProviderCategory::Compaction,
                    ),
                )
                .should_deliver
        );
        let terminal = deliveries.record(
            11,
            &prompt,
            AgentWatchProviderDeliveryKind::TerminalError(
                tau_proto::ProviderFailureKind::RequestRejected,
            ),
        );
        assert!(terminal.should_deliver);
        assert!(terminal.terminal_retired);
        assert_eq!(deliveries.prompt_count(), 0);
        assert_eq!(deliveries.delivery_key_count(), 0);
    }

    for index in 0..HIGH_CARDINALITY_PROMPTS {
        assert!(
            deliveries
                .record(
                    11,
                    &tau_proto::AgentPromptId::from(format!("sp-nonterminal-stream-{index}")),
                    retrying_transport(),
                )
                .should_deliver
        );
        assert!(deliveries.prompt_count() <= MAX_TRACKED_PROVIDER_STATUS_PROMPTS);
        assert!(deliveries.delivery_key_count() <= MAX_TRACKED_PROVIDER_STATUS_PROMPTS);
    }
    assert_eq!(
        deliveries.prompt_count(),
        MAX_TRACKED_PROVIDER_STATUS_PROMPTS
    );
    assert_eq!(
        deliveries.delivery_key_count(),
        MAX_TRACKED_PROVIDER_STATUS_PROMPTS
    );
}
