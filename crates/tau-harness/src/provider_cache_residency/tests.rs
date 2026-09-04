use std::cell::Cell;
use std::num::{NonZeroU32, NonZeroU64};
use std::rc::Rc;

use tau_config::settings::ProviderCacheMaxIdle;
use tau_proto::{
    EstimatedUsdPerMillion, NativeReasoningEffort, ProviderCacheDeletionAvailability,
    ProviderCacheKind, ProviderCachePolicy, ProviderCachePrivacy, ProviderCacheQuotaAccounting,
    ThinkingSummary, Verbosity,
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

/// Scripted generic Provider fixture that feeds only cache-policy metadata and
/// privacy-redacted usage into the harness-owned scheduler.
struct ScriptedFakeProvider {
    /// Captured configured Provider connection.
    connection_id: tau_proto::ConnectionId,
    /// Published route metadata used for each scripted completion.
    model: ProviderModelInfo,
    /// Response-local observations returned by successive ordinary prompts.
    completions: VecDeque<tau_proto::ProviderTokenUsage>,
}

impl ScriptedFakeProvider {
    /// Create one generic Provider with a finite ordinary-response script.
    fn new(
        model: ProviderModelInfo,
        completions: impl IntoIterator<Item = tau_proto::ProviderTokenUsage>,
    ) -> Self {
        let provider = model.id.provider.as_str();
        Self {
            connection_id: tau_proto::ConnectionId::parse(format!("{provider}-connection"))
                .expect("fixture connection id"),
            model,
            completions: completions.into_iter().collect(),
        }
    }

    /// Deliver the next scripted ordinary completion through the scheduler's
    /// production observation boundary.
    fn complete<C: CacheClock, J: CacheJitter>(
        &mut self,
        scheduler: &mut ProviderCacheResidency<C, J>,
        prompt_id: &str,
    ) {
        let prompt = prompt(self.model.id.provider.as_str(), prompt_id);
        scheduler.track_prompt(self.connection_id.clone(), &prompt, Some(&self.model));
        let usage = self
            .completions
            .pop_front()
            .expect("fixture supplies a completion for every prompt");
        scheduler.finish_prompt(&prompt.agent_prompt_id, true, Some(&usage));
    }
}

/// One generic Provider policy case that must suppress automatic refresh work.
struct IneligibleContractCase {
    /// Stable label included when an ineligible contract unexpectedly
    /// dispatches.
    name: &'static str,
    /// Published metadata for the exact fake Provider route.
    model: ProviderModelInfo,
}

impl IneligibleContractCase {
    /// Build a safe baseline model with one isolated unsupported policy fact.
    fn new(name: &'static str, mutate: impl FnOnce(&mut ProviderModelInfo)) -> Self {
        let mut model = model(name);
        mutate(&mut model);
        Self { name, model }
    }
}

/// Build one fully eligible cache-policy model fixture.
pub(crate) fn model(provider: &str) -> ProviderModelInfo {
    ProviderModelInfo {
        id: format!("{provider}/model").parse().expect("model"),
        display_name: None,
        tags: Vec::new(),
        hosted_tool_capabilities: Vec::new(),
        supported_tool_types: vec![tau_proto::ToolType::Function],
        input_modalities: Vec::new(),
        tool_result_modalities: Vec::new(),
        supports_parallel_tool_calls: true,
        default_affinity: 0,
        context_window: tau_proto::TokenCount::new(10_000),
        max_input_tokens: None,
        max_output_tokens: None,
        efforts: tau_proto::ReasoningEffortCapability::mapped(vec![NativeReasoningEffort::Medium]),
        verbosities: vec![Verbosity::Medium],
        thinking_summaries: vec![ThinkingSummary::Off],
        supports_compaction: false,
        supports_standalone_compaction: false,
        standalone_compaction_generation_negative: false,
        standalone_compaction_threshold: None,
        standalone_compaction_prefix_budget: None,
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
        hosted_tools: Vec::new(),
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

/// Streaming prefix hashing must retain the exact digest identity produced by
/// the previous contiguous JSON serialization.
#[test]
fn streaming_prefix_hash_matches_contiguous_serialization() {
    let prompt = prompt("one", "prompt-hash");
    let key = [7; 32];
    let expected_bytes = serde_json::to_vec(&(
        &prompt.system_prompt,
        &prompt.context,
        &prompt.tools,
        &prompt.model,
        prompt.model_params,
        prompt.tool_choice,
        &prompt.originator,
        prompt.share_user_cache_key,
    ))
    .expect("serialize prefix");
    let expected = blake3::keyed_hash(&key, &expected_bytes);
    let (_, mut scheduler) = owner();

    scheduler.track_prompt(
        tau_proto::ConnectionId::parse("one-route").expect("route"),
        &prompt,
        Some(&model("one")),
    );

    let tracked = scheduler
        .tracked
        .get(&prompt.agent_prompt_id)
        .expect("tracked prompt");
    assert_eq!(tracked.key.digest, expected.as_bytes()[..16]);
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

/// Generic Provider policy scripts use a fake monotonic clock to prove the
/// discrete two-read break-even point and TTL-minus-margin dispatch deadline
/// without contacting a backend or waiting for wall time.
#[test]
fn cross_backend_policy_script_uses_exact_break_even_and_ttl_margin() {
    let (clock, mut scheduler) = owner();
    let mut model = model("anthropic-proxy");
    model.cache_policy.as_mut().expect("policy").kind = ProviderCacheKind::ExplicitBreakpoint;
    model.est_uncached_input_cost_1m_usd = Some(EstimatedUsdPerMillion::from_micro_usd(3_000_000));
    model.est_cached_input_cost_1m_usd = Some(EstimatedUsdPerMillion::from_micro_usd(300_000));
    model.est_cache_write_input_cost_1m_usd =
        Some(EstimatedUsdPerMillion::from_micro_usd(6_000_000));
    let mut provider = ScriptedFakeProvider::new(model, [usage(0, 10), usage(10, 0), usage(10, 0)]);

    provider.complete(&mut scheduler, "break-even-write");
    provider.complete(&mut scheduler, "break-even-read-one");
    assert!(
        scheduler.scheduled.is_empty(),
        "the one-hour write premium needs two later reads, not one"
    );
    provider.complete(&mut scheduler, "break-even-read-two");
    let scheduled = scheduler
        .scheduled
        .values()
        .next()
        .expect("second read reaches the exact discrete break-even");
    assert_eq!(
        scheduled.due,
        clock.0.get() + Duration::from_secs(90),
        "the fixed ten-second jitter dispatches at TTL minus margin"
    );

    scheduler.open_tool_window();
    clock.0.set(clock.0.get() + Duration::from_secs(90));
    let refresh = scheduler.admit().pop().expect("due refresh");
    assert_eq!(refresh.connection_id, provider.connection_id);
    assert_eq!(refresh.request.stop_after_millis.get(), 10_000);
    assert!(
        scheduler.finish(&provider.connection_id, &refresh.request.refresh_id),
        "the scripted Provider's exact terminal consumes its slot"
    );
}

/// Generic fake Provider contracts with unknown/minimum residency, bounded
/// output, unsafe quota evidence, or named object operations must not turn
/// otherwise qualifying response-local observations into scheduler traffic.
#[test]
fn cross_backend_policy_script_fails_closed_for_unsupported_contracts() {
    let contracts = [
        IneligibleContractCase::new("unknown-ttl", |model| {
            model.cache_policy.as_mut().expect("policy").ttl = ProviderCacheTtl::Unknown;
        }),
        IneligibleContractCase::new("minimum-ttl", |model| {
            model.cache_policy.as_mut().expect("policy").ttl = ProviderCacheTtl::Minimum {
                seconds: NonZeroU64::new(100).expect("positive ttl"),
            };
        }),
        IneligibleContractCase::new("one-output", |model| {
            model.cache_policy.as_mut().expect("policy").output_floor =
                ProviderCacheOutputFloor::One;
        }),
        IneligibleContractCase::new("reasoning-output", |model| {
            model.cache_policy.as_mut().expect("policy").output_floor =
                ProviderCacheOutputFloor::UnboundedReasoning;
        }),
        IneligibleContractCase::new("quota-unknown", |model| {
            model.cache_policy.as_mut().expect("policy").quota.requests =
                ProviderCacheQuotaCharge::Unknown;
        }),
        IneligibleContractCase::new("explicit-object", |model| {
            let object = model.cache_policy.as_mut().expect("policy");
            object.kind = ProviderCacheKind::ExplicitObject;
            object.ttl = ProviderCacheTtl::Fixed {
                seconds: NonZeroU64::new(100).expect("positive ttl"),
            };
            object.renewal = ProviderCacheRenewal::PatchExpiry;
            object.privacy.storage = ProviderCacheStorageMode::NamedProviderObject;
        }),
    ];

    for IneligibleContractCase { name, model } in contracts {
        let (_, mut scheduler) = owner();
        let mut provider =
            ScriptedFakeProvider::new(model, [usage(0, 10), usage(10, 0), usage(10, 0)]);
        provider.complete(&mut scheduler, "unsafe-write");
        provider.complete(&mut scheduler, "unsafe-read");
        provider.complete(&mut scheduler, "unsafe-second-read");
        scheduler.open_tool_window();
        assert!(
            scheduler.admit().is_empty(),
            "{name} must not dispatch an unsupported refresh operation"
        );
        assert!(
            scheduler.evidence.is_empty() && scheduler.scheduled.is_empty(),
            "{name} must reject its isolated policy fact before recording evidence"
        );
    }
}

/// Probabilistic response-local telemetry cannot promote an otherwise
/// ineligible published cache policy into scheduler evidence.
#[test]
fn scripted_provider_probabilistic_usage_cannot_promote_unknown_ttl() {
    let (_, mut scheduler) = owner();
    let mut model = model("probabilistic-telemetry");
    model.cache_policy.as_mut().expect("policy").ttl = ProviderCacheTtl::Unknown;
    let mut probabilistic_read = usage(10, 0);
    probabilistic_read
        .cache
        .as_deref_mut()
        .expect("fixture cache")
        .expiry_confidence = Some(tau_proto::ProviderCacheExpiryConfidence::Probabilistic);
    let mut provider =
        ScriptedFakeProvider::new(model, [usage(0, 10), probabilistic_read, usage(10, 0)]);

    provider.complete(&mut scheduler, "write");
    provider.complete(&mut scheduler, "probabilistic-read");
    provider.complete(&mut scheduler, "second-read");
    scheduler.open_tool_window();
    assert!(
        scheduler.evidence.is_empty()
            && scheduler.scheduled.is_empty()
            && scheduler.admit().is_empty(),
        "probabilistic telemetry cannot override the unknown published TTL"
    );
}

/// Missing usage and missing nested cache counters must not allocate evidence
/// or authorize a refresh even when the configured Provider policy is otherwise
/// eligible.
#[test]
fn scripted_provider_missing_usage_never_creates_evidence() {
    let (_, mut scheduler) = owner();
    let model = model("missing-usage");
    for (prompt_id, usage) in [
        ("absent-usage", None),
        (
            "absent-cache",
            Some(tau_proto::ProviderTokenUsage::default()),
        ),
        (
            "absent-counters",
            Some(tau_proto::ProviderTokenUsage {
                cache: Some(Box::default()),
                ..Default::default()
            }),
        ),
    ] {
        let prompt = prompt("missing-usage", prompt_id);
        scheduler.track_prompt(
            tau_proto::ConnectionId::parse("missing-usage-connection").expect("connection"),
            &prompt,
            Some(&model),
        );
        scheduler.finish_prompt(&prompt.agent_prompt_id, true, usage.as_ref());
        assert!(
            scheduler.evidence.is_empty() && scheduler.scheduled.is_empty(),
            "{prompt_id} must not become automatic refresh evidence"
        );
    }
}

/// A fresh cold write suppresses prior evidence; cancellation retains scheduler
/// ownership until the scripted Provider returns its exact terminal, including
/// after shutdown begins.
#[test]
fn scripted_provider_lifecycle_suppresses_cold_writes_and_retains_shutdown_owner() {
    let (clock, mut scheduler) = owner();
    let mut model = model("generic");
    model.est_uncached_input_cost_1m_usd = Some(EstimatedUsdPerMillion::from_micro_usd(3_000_000));
    model.est_cached_input_cost_1m_usd = Some(EstimatedUsdPerMillion::from_micro_usd(300_000));
    model.est_cache_write_input_cost_1m_usd =
        Some(EstimatedUsdPerMillion::from_micro_usd(6_000_000));
    let mut provider = ScriptedFakeProvider::new(
        model,
        [
            usage(0, 10),
            usage(10, 0),
            usage(0, 10),
            usage(10, 0),
            usage(10, 0),
            usage(10, 0),
        ],
    );

    provider.complete(&mut scheduler, "first-write");
    provider.complete(&mut scheduler, "first-read");
    provider.complete(&mut scheduler, "concurrent-cold-write");
    provider.complete(&mut scheduler, "second-read");
    assert!(
        scheduler.scheduled.is_empty(),
        "a later cold write resets the earlier read evidence"
    );
    provider.complete(&mut scheduler, "third-read");
    assert_eq!(scheduler.scheduled.len(), 1);

    scheduler.open_tool_window();
    clock.0.set(clock.0.get() + Duration::from_secs(90));
    let refresh = scheduler.admit().pop().expect("fresh generation refresh");
    let cancellations = scheduler.cancel_real(
        &tau_proto::ProviderName::new("generic"),
        &tau_proto::AgentId::parse("agent").expect("fixture agent"),
    );
    assert_eq!(cancellations.len(), 1);
    assert!(
        !scheduler.finish(
            &tau_proto::ConnectionId::parse("wrong-provider").expect("fixture connection"),
            &refresh.request.refresh_id,
        ),
        "a raced terminal from another Provider cannot release ownership"
    );
    let shutdown = scheduler.clear(tau_proto::ProviderCacheRefreshCancelReason::Shutdown);
    assert!(
        shutdown.is_empty(),
        "the prior cancellation remains the sole directed request for this attempt"
    );
    assert!(
        scheduler.finish(&provider.connection_id, &refresh.request.refresh_id),
        "shutdown retains ownership until the scripted Provider's exact terminal"
    );
    assert!(scheduler.next_deadline().is_none());
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

/// Observation authorizations retain their zero origin, increment, and
/// saturation behavior without exposing a raw runtime counter.
#[test]
fn observation_generation_preserves_counter_behavior() {
    let mut generation = CacheObservationGeneration::INITIAL;
    assert_eq!(generation.0, 0);

    generation.advance();
    assert_eq!(generation.0, 1);

    generation.0 = u64::MAX;
    generation.advance();
    assert_eq!(generation.0, u64::MAX);
}

/// A write starts at the initial authorization, later reads advance it, and a
/// scheduled attempt holding the superseded authorization cannot dispatch.
#[test]
fn stale_observation_authorization_cannot_dispatch() {
    let (clock, mut scheduler) = owner();
    let route = tau_proto::ConnectionId::parse("one-connection").expect("route");
    let model = model("one");
    let write = prompt("one", "observation-write");
    scheduler.track_prompt(route.clone(), &write, Some(&model));
    scheduler.finish_prompt(&write.agent_prompt_id, true, Some(&usage(0, 10)));
    let evidence = scheduler
        .evidence
        .values()
        .next()
        .expect("write records evidence");
    assert_eq!(evidence.generation, CacheObservationGeneration::INITIAL);

    let first_read = prompt("one", "observation-first-read");
    scheduler.track_prompt(route.clone(), &first_read, Some(&model));
    scheduler.finish_prompt(&first_read.agent_prompt_id, true, Some(&usage(10, 0)));
    let stale_generation = scheduler
        .scheduled
        .values()
        .next()
        .expect("first qualifying read schedules a refresh")
        .generation;
    assert!(
        CacheObservationGeneration::INITIAL < stale_generation,
        "the first qualifying read advances the allocated authorization"
    );

    let second_read = prompt("one", "observation-second-read");
    scheduler.track_prompt(route, &second_read, Some(&model));
    scheduler.finish_prompt(&second_read.agent_prompt_id, true, Some(&usage(10, 0)));
    assert!(
        scheduler
            .evidence
            .values()
            .next()
            .expect("second qualifying read retains evidence")
            .generation
            > stale_generation,
        "every later qualifying read allocates a newer authorization"
    );
    scheduler
        .scheduled
        .values_mut()
        .next()
        .expect("second qualifying read replaces the schedule")
        .generation = stale_generation;

    scheduler.open_tool_window();
    clock.0.set(clock.0.get() + Duration::from_secs(90));
    assert!(
        scheduler.admit().is_empty(),
        "a stale scheduled authorization must not dispatch"
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
