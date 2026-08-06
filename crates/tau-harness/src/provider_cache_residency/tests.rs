use std::cell::Cell;
use std::num::{NonZeroU32, NonZeroU64};
use std::rc::Rc;

use tau_config::settings::ProviderCacheMaxIdle;
use tau_proto::{
    Effort, EstimatedUsdPerMillion, ProviderCacheDeletionAvailability, ProviderCacheKind,
    ProviderCachePolicy, ProviderCachePrivacy, ProviderCacheQuotaAccounting, ThinkingSummary,
    Verbosity,
};

use super::*;

/// Shared manually advanced monotonic test clock.
#[derive(Clone)]
struct FakeClock(
    /// Current synthetic monotonic instant.
    Rc<Cell<Instant>>,
);

impl CacheClock for FakeClock {
    fn now(&self) -> Instant {
        self.0.get()
    }
}

/// Deterministic jitter and incrementing byte source.
struct FixedJitter(
    /// Next fixed jitter value and ID byte.
    u64,
);

impl CacheJitter for FixedJitter {
    fn seconds(&mut self, minimum: u64, maximum: u64) -> u64 {
        self.0.clamp(minimum, maximum)
    }

    fn fill_bytes(&mut self, output: &mut [u8]) {
        output.fill(u8::try_from(self.0).unwrap_or(u8::MAX));
        self.0 = self.0.saturating_add(1);
    }
}

/// Scripted identity entropy for collision tests.
struct SequenceJitter(
    /// Successive complete ID nonces returned to the scheduler.
    VecDeque<[u8; 16]>,
);

impl CacheJitter for SequenceJitter {
    fn seconds(&mut self, minimum: u64, _maximum: u64) -> u64 {
        minimum
    }

    fn fill_bytes(&mut self, output: &mut [u8]) {
        output.copy_from_slice(
            &self
                .0
                .pop_front()
                .expect("test supplies enough identity entropy"),
        );
    }
}

/// Build one fully eligible cache-policy model fixture.
pub(crate) fn model(provider: &str) -> ProviderModelInfo {
    ProviderModelInfo {
        id: format!("{provider}/model").parse().expect("model"),
        display_name: None,
        tags: Vec::new(),
        supported_tool_types: Vec::new(),
        input_modalities: Vec::new(),
        tool_result_modalities: Vec::new(),
        supports_parallel_tool_calls: true,
        default_affinity: 0,
        context_window: 10_000,
        efforts: vec![Effort::Medium],
        verbosities: vec![Verbosity::Medium],
        thinking_summaries: vec![ThinkingSummary::Off],
        supports_compaction: false,
        supports_standalone_compaction: false,
        standalone_compaction_threshold: None,
        cache_policy: Some(ProviderCachePolicy {
            kind: ProviderCacheKind::AutomaticPrefix,
            ttl: ProviderCacheTtl::SlidingKnown {
                seconds: NonZeroU64::new(100).expect("nonzero"),
            },
            renewal: ProviderCacheRenewal::Read,
            output_floor: ProviderCacheOutputFloor::Zero,
            quota: ProviderCacheQuotaAccounting {
                requests: ProviderCacheQuotaCharge::CountsFully,
                read_tokens: ProviderCacheQuotaCharge::Exempt,
                write_tokens: ProviderCacheQuotaCharge::CountsFully,
                output_tokens: ProviderCacheQuotaCharge::Exempt,
            },
            prefix_identity_version: NonZeroU32::new(1).expect("nonzero"),
            privacy: ProviderCachePrivacy {
                storage: ProviderCacheStorageMode::VolatileMemory,
                zero_data_retention: ProviderCacheZeroDataRetentionCompatibility::Compatible,
                data_residency: ProviderCacheDataResidencyEffect::PreservesRoutePolicy,
                manual_deletion: ProviderCacheDeletionAvailability::Unavailable,
            },
        }),
        est_uncached_input_cost_1m_usd: Some(EstimatedUsdPerMillion::from_micro_usd(1_000_000)),
        est_cached_input_cost_1m_usd: Some(EstimatedUsdPerMillion::from_micro_usd(100_000)),
        est_cache_write_input_cost_1m_usd: Some(EstimatedUsdPerMillion::from_micro_usd(1_200_000)),
        est_output_cost_1m_usd: None,
        est_cache_storage_cost_1m_token_hour_usd: None,
    }
}

/// Build one exact-prefix inference prompt fixture.
pub(crate) fn prompt(provider: &str, id: &str) -> AgentPromptCreated {
    AgentPromptCreated {
        agent_prompt_id: AgentPromptId::parse(id).expect("prompt"),
        agent_id: AgentId::parse("agent").expect("agent"),
        session_id: tau_proto::SessionId::parse("session").expect("session"),
        system_prompt: "system".to_owned(),
        context: tau_proto::PromptContext::default(),
        tools: Vec::new(),
        tools_ref: None,
        model: format!("{provider}/model").parse().expect("model"),
        model_params: Default::default(),
        tool_choice: tau_proto::ToolChoice::Auto,
        originator: tau_proto::PromptOriginator::User,
        share_user_cache_key: false,
        ctx_id: None,
        compaction: None,
        operation: tau_proto::PromptOperation::Inference,
    }
}

/// Build response-local cache usage with explicit read/write counters.
pub(crate) fn usage(read: u64, write: u64) -> tau_proto::ProviderTokenUsage {
    tau_proto::ProviderTokenUsage {
        model: None,
        prompt_sent_tokens: read.saturating_add(write),
        prompt_cached_tokens: read,
        prompt_cache_read_ceiling_tokens: None,
        cache: Some(Box::new(tau_proto::ProviderCacheUsage {
            read_tokens: Some(read),
            write_tokens: Some(write),
            ..Default::default()
        })),
        response_received_tokens: 0,
        stats: Default::default(),
    }
}

fn owner() -> (FakeClock, ProviderCacheResidency<FakeClock, FixedJitter>) {
    let clock = FakeClock(Rc::new(Cell::new(Instant::now())));
    (
        clock.clone(),
        ProviderCacheResidency::new(
            ProviderCacheRefresh {
                enabled: true,
                max_idle_seconds: ProviderCacheMaxIdle::new(200).expect("valid test bound"),
            },
            clock,
            FixedJitter(10),
            [7; 32],
        ),
    )
}

fn observe_write_and_read(
    owner: &mut ProviderCacheResidency<FakeClock, FixedJitter>,
    provider: &str,
) {
    let route = tau_proto::ConnectionId::parse(format!("{provider}-connection")).expect("route");
    let model = model(provider);
    let write_prompt = prompt(provider, &format!("ap-{provider}-write"));
    owner.track_prompt(route.clone(), &write_prompt, Some(&model));
    owner.finish_prompt(&write_prompt.agent_prompt_id, true, Some(&usage(0, 10)));
    let read_prompt = prompt(provider, &format!("ap-{provider}-read"));
    owner.track_prompt(route, &read_prompt, Some(&model));
    owner.finish_prompt(&read_prompt.agent_prompt_id, true, Some(&usage(10, 0)));
}

/// Injected monotonic time and fixed jitter produce an exact due deadline.
#[test]
fn deterministic_clock_jitter_and_break_even() {
    let (clock, mut scheduler) = owner();
    observe_write_and_read(&mut scheduler, "one");
    scheduler.open_tool_window();
    assert_eq!(
        scheduler.next_deadline(),
        clock.0.get().checked_add(Duration::from_secs(90))
    );
}

/// The owner deduplicates equal prefixes and enforces global/Provider limits.
#[test]
fn dedup_and_concurrency_are_bounded() {
    let (clock, mut owner) = owner();
    observe_write_and_read(&mut owner, "one");
    observe_write_and_read(&mut owner, "two");
    observe_write_and_read(&mut owner, "three");
    clock.0.set(clock.0.get() + Duration::from_secs(90));
    owner.open_tool_window();
    let admitted = owner.admit();
    assert_eq!(admitted.len(), 2);
    assert!(admitted.iter().all(|refresh| {
        refresh.connection_id.as_str() == format!("{}-connection", refresh.provider)
            && refresh.request.prompt.system_prompt == "system"
            && refresh.request.prompt.context == tau_proto::PromptContext::default()
    }));
    assert!(
        clock.0.get() < owner.next_deadline().expect("capacity release deadline"),
        "a due cohort blocked by active capacity must not busy-loop at its old due time"
    );
}

/// Real prompt priority cancels active refresh and prefix-change state.
#[test]
fn real_prompt_supersedes_refresh() {
    let (clock, mut owner) = owner();
    observe_write_and_read(&mut owner, "one");
    clock.0.set(clock.0.get() + Duration::from_secs(90));
    owner.open_tool_window();
    assert_eq!(owner.admit().len(), 1);
    let cancellations = owner.cancel_real(
        &tau_proto::ProviderName::new("one"),
        &tau_proto::AgentId::parse("agent").expect("agent"),
    );
    owner.track_prompt(
        tau_proto::ConnectionId::parse("one-new").expect("route"),
        &prompt("one", "ap-one-real"),
        Some(&model("one")),
    );
    assert_eq!(cancellations.len(), 1);
    assert_eq!(
        owner.active.len(),
        1,
        "cancel send alone retains slot ownership"
    );
    let active = owner.active.values().next().expect("active attempt");
    assert!(active.cancel_sent);
}

/// Failures, exact stop equality, shutdown, and restart never resurrect work.
#[test]
fn failure_deadline_shutdown_and_restart_are_terminal() {
    let (clock, mut scheduler) = owner();
    let failed = prompt("one", "ap-failed");
    scheduler.track_prompt(
        tau_proto::ConnectionId::parse("one").expect("route"),
        &failed,
        Some(&model("one")),
    );
    scheduler.finish_prompt(&failed.agent_prompt_id, false, Some(&usage(10, 0)));
    assert!(scheduler.scheduled.is_empty());
    observe_write_and_read(&mut scheduler, "one");
    clock.0.set(clock.0.get() + Duration::from_secs(100));
    assert!(scheduler.admit().is_empty());
    let _ = scheduler.clear(tau_proto::ProviderCacheRefreshCancelReason::Shutdown);
    assert!(scheduler.next_deadline().is_none());
    let (_, restarted) = owner();
    assert!(restarted.next_deadline().is_none());
}

/// Active stop service remains visible after window closure and releases the
/// retained slot even when no Provider terminal arrives.
#[test]
fn closed_window_keeps_active_deadline_and_disconnect_releases() {
    let (clock, mut scheduler) = owner();
    observe_write_and_read(&mut scheduler, "one");
    clock.0.set(clock.0.get() + Duration::from_secs(90));
    scheduler.open_tool_window();
    let refresh = scheduler.admit().pop().expect("admitted refresh");
    let cancellations = scheduler.close_window();
    assert_eq!(cancellations.len(), 1);
    assert!(scheduler.next_deadline().is_some());
    scheduler.release_connection(&refresh.connection_id);
    assert!(scheduler.next_deadline().is_none());

    observe_write_and_read(&mut scheduler, "one");
    clock.0.set(clock.0.get() + Duration::from_secs(90));
    scheduler.open_tool_window();
    scheduler.admit();
    scheduler.close_window();
    clock.0.set(clock.0.get() + Duration::from_secs(11));
    let deadline = scheduler.expire_deadlines();
    assert_eq!(deadline.len(), 1);
    assert_eq!(
        deadline[0].request.reason,
        tau_proto::ProviderCacheRefreshCancelReason::Deadline
    );
    assert!(scheduler.active.is_empty());
}

/// Definitive pre-receipt enqueue failure releases exact active ownership.
#[test]
fn enqueue_failure_releases_exact_attempt() {
    let (clock, mut scheduler) = owner();
    observe_write_and_read(&mut scheduler, "one");
    clock.0.set(clock.0.get() + Duration::from_secs(90));
    scheduler.open_tool_window();
    let refresh = scheduler.admit().pop().expect("refresh");
    assert!(scheduler.finish(&refresh.connection_id, &refresh.request.refresh_id));
    assert!(scheduler.active.is_empty());
}

/// Write-only evidence is bounded before any key reaches break-even.
#[test]
fn write_only_evidence_obeys_global_and_provider_bounds() {
    let (_, mut scheduler) = owner();
    for index in 0..130 {
        let id = format!("write-{index}");
        let prompt = prompt("one", &id);
        scheduler.track_prompt(
            tau_proto::ConnectionId::parse(format!("route-{index}")).expect("route"),
            &prompt,
            Some(&model("one")),
        );
        scheduler.finish_prompt(&prompt.agent_prompt_id, true, Some(&usage(0, 10)));
    }
    assert_eq!(scheduler.evidence.len(), 128);

    for index in 0..1_025 {
        let provider = format!("provider-{index}");
        let id = format!("global-{index}");
        let prompt = prompt(&provider, &id);
        scheduler.track_prompt(
            tau_proto::ConnectionId::parse(format!("global-route-{index}")).expect("route"),
            &prompt,
            Some(&model(&provider)),
        );
        scheduler.finish_prompt(&prompt.agent_prompt_id, true, Some(&usage(0, 10)));
    }
    assert_eq!(scheduler.evidence.len(), 1_024);
}

/// Non-evidence and ineligible observations never allocate map or eviction
/// state, including distinct-key adversarial streams.
#[test]
fn unusable_observations_do_not_grow_evidence_or_order() {
    let (_, mut scheduler) = owner();
    for index in 0..200 {
        for (suffix, observation, route_model) in [
            ("zero", usage(0, 0), model("one")),
            ("read-before-write", usage(10, 0), model("one")),
            ("ineligible-write", usage(0, 10), {
                let mut model = model("one");
                model.cache_policy.as_mut().expect("policy").ttl = ProviderCacheTtl::Unknown;
                model
            }),
        ] {
            let id = format!("{suffix}-{index}");
            let prompt = prompt("one", &id);
            scheduler.track_prompt(
                tau_proto::ConnectionId::parse(format!("{suffix}-route-{index}")).expect("route"),
                &prompt,
                Some(&route_model),
            );
            scheduler.finish_prompt(&prompt.agent_prompt_id, true, Some(&observation));
        }
    }
    assert!(scheduler.evidence.is_empty());
    assert!(scheduler.evidence_order.is_empty());
}

/// Evidence invalidation and both eviction limits never release an in-flight
/// lifecycle owner.
#[test]
fn evidence_eviction_retains_active_attempt() {
    let (clock, mut scheduler) = owner();
    observe_write_and_read(&mut scheduler, "active");
    clock.0.set(clock.0.get() + Duration::from_secs(90));
    scheduler.open_tool_window();
    scheduler.admit();
    assert_eq!(scheduler.active.len(), 1);

    for index in 0..130 {
        let prompt = prompt("active", &format!("provider-eviction-{index}"));
        scheduler.track_prompt(
            tau_proto::ConnectionId::parse(format!("provider-eviction-{index}")).expect("route"),
            &prompt,
            Some(&model("active")),
        );
        scheduler.finish_prompt(&prompt.agent_prompt_id, true, Some(&usage(0, 10)));
    }
    assert_eq!(scheduler.active.len(), 1);

    for index in 0..1_025 {
        let provider = format!("global-eviction-{index}");
        let prompt = prompt(&provider, &format!("global-eviction-prompt-{index}"));
        scheduler.track_prompt(
            tau_proto::ConnectionId::parse(format!("global-eviction-route-{index}"))
                .expect("route"),
            &prompt,
            Some(&model(&provider)),
        );
        scheduler.finish_prompt(&prompt.agent_prompt_id, true, Some(&usage(0, 10)));
    }
    assert_eq!(scheduler.active.len(), 1);

    let ineligible = prompt("active", "ineligible-active");
    let mut ineligible_model = model("active");
    ineligible_model.cache_policy.as_mut().expect("policy").ttl = ProviderCacheTtl::Unknown;
    scheduler.track_prompt(
        tau_proto::ConnectionId::parse("active-connection").expect("route"),
        &ineligible,
        Some(&ineligible_model),
    );
    scheduler.finish_prompt(&ineligible.agent_prompt_id, true, Some(&usage(0, 10)));
    assert_eq!(scheduler.active.len(), 1);
    let refresh_id = scheduler
        .active
        .values()
        .next()
        .expect("retained active")
        .refresh_id
        .clone();
    assert!(scheduler.finish(
        &tau_proto::ConnectionId::parse("active-connection").expect("route"),
        &refresh_id
    ));
    assert!(scheduler.active.is_empty());
}

/// Random ID generation retries an active-ID collision deterministically.
#[test]
fn refresh_id_collision_retries() {
    let clock = FakeClock(Rc::new(Cell::new(Instant::now())));
    let mut scheduler = ProviderCacheResidency::new(
        ProviderCacheRefresh {
            enabled: true,
            max_idle_seconds: ProviderCacheMaxIdle::new(200).expect("valid bound"),
        },
        clock,
        SequenceJitter(VecDeque::from([[7; 16], [7; 16], [8; 16]])),
        [1; 32],
    );
    let first = scheduler.random_refresh_id();
    scheduler.install_active_for_test(
        tau_proto::ConnectionId::parse("route").expect("route"),
        first.clone(),
    );
    let second = scheduler.random_refresh_id();
    assert_ne!(first, second);
}

/// Unknown privacy, output, quota, TTL, or pricing contracts fail closed.
#[test]
fn incomplete_or_unsafe_contracts_fail_closed() {
    let mut value = model("one");
    value.cache_policy.as_mut().expect("policy").ttl = ProviderCacheTtl::Unknown;
    assert!(eligible(&value).is_none());
    let mut value = model("one");
    value.cache_policy.as_mut().expect("policy").privacy.storage =
        ProviderCacheStorageMode::ExtendedProviderRetention;
    assert!(eligible(&value).is_none());
    let mut value = model("one");
    value.cache_policy.as_mut().expect("policy").quota.requests = ProviderCacheQuotaCharge::Unknown;
    assert!(eligible(&value).is_none());
    let mut value = model("one");
    value.est_cache_write_input_cost_1m_usd = None;
    assert!(eligible(&value).is_none());
}
