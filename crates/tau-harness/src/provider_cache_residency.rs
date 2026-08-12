//! Harness-owned, process-only Provider cache residency scheduling.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::num::NonZeroU32;
use std::time::{Duration, Instant};

use rand::RngCore as _;
use rand::rngs::StdRng;
use tau_config::settings::ProviderCacheRefresh;
use tau_proto::{
    AgentId, AgentPromptCreated, AgentPromptId, ModelId, ProviderCacheDataResidencyEffect,
    ProviderCacheOutputFloor, ProviderCacheQuotaCharge, ProviderCacheRenewal,
    ProviderCacheStorageMode, ProviderCacheTtl, ProviderCacheZeroDataRetentionCompatibility,
    ProviderModelInfo, ProviderName,
};

#[cfg(test)]
pub(crate) mod tests;

const GLOBAL_CONCURRENCY: usize = 2;
const PROVIDER_CONCURRENCY: usize = 1;

/// Injectable process-monotonic time.
pub(crate) trait CacheClock {
    /// Return the current monotonic instant.
    fn now(&self) -> Instant;
}

/// Production monotonic clock.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RuntimeCacheClock;

impl CacheClock for RuntimeCacheClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Injectable bounded jitter source.
pub(crate) trait CacheJitter {
    /// Sample an inclusive number of seconds.
    fn seconds(&mut self, minimum: u64, maximum: u64) -> u64;
    /// Fill an identifier nonce with process-local random bytes.
    fn fill_bytes(&mut self, output: &mut [u8]);
}

/// Production process-local random jitter.
pub(crate) struct RuntimeCacheJitter {
    /// Independently seeded process RNG for scheduling and correlation IDs.
    rng: StdRng,
}

impl RuntimeCacheJitter {
    /// Create an independently seeded production jitter stream.
    pub(crate) fn new() -> Self {
        use rand::SeedableRng as _;
        Self {
            rng: StdRng::from_entropy(),
        }
    }
}

impl ProviderCacheResidency<RuntimeCacheClock, RuntimeCacheJitter> {
    /// Construct production process-local state with independent secret
    /// entropy.
    pub(crate) fn runtime(config: ProviderCacheRefresh) -> Self {
        let mut key = [0; 32];
        let mut jitter = RuntimeCacheJitter::new();
        jitter.rng.fill_bytes(&mut key);
        Self::new(config, RuntimeCacheClock, jitter, key)
    }
}

impl CacheJitter for RuntimeCacheJitter {
    fn seconds(&mut self, minimum: u64, maximum: u64) -> u64 {
        minimum + self.rng.next_u64() % (maximum - minimum + 1)
    }

    fn fill_bytes(&mut self, output: &mut [u8]) {
        self.rng.fill_bytes(output);
    }
}

/// Process-secret identity of one exact Provider-visible prefix.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct CacheKey {
    /// Exact owning Provider connection generation.
    connection_id: tau_proto::ConnectionId,
    /// Namespace used for per-Provider bounds.
    provider: ProviderName,
    /// Exact selected model route.
    model: ModelId,
    /// Agent cache-bucket owner.
    agent_id: AgentId,
    /// Published prefix-semantics generation.
    prefix_identity_version: u32,
    /// Process-secret digest of all Provider-visible prefix fields.
    digest: [u8; 16],
}

/// One real prompt retained until terminal usage establishes evidence.
struct TrackedPrompt {
    /// Captured route and prefix identity.
    key: CacheKey,
    /// Sensitive full prefix retained only to terminal observation.
    request: tau_proto::AgentPromptPrewarmRequested,
    /// Published economic and privacy contract.
    model: ProviderModelInfo,
}

/// Bounded observation state for one exact prefix generation.
struct Evidence {
    /// Number of qualifying cache reads since the latest observed write.
    reads_after_write: u64,
    /// Whether this key has an observed write in the current generation.
    saw_write: bool,
    /// Monotonic observation generation; each later read may reschedule.
    generation: u64,
}

/// One observation generation awaiting finite-window admission.
struct Scheduled {
    /// Exact sensitive prefix request retained only in process memory.
    request: tau_proto::AgentPromptPrewarmRequested,
    /// Observation generation that authorized this attempt.
    generation: u64,
    /// Monotonic dispatch deadline.
    due: Instant,
    /// Exact economic/idle stop.
    stop: Instant,
}

/// One admitted directed cache-refresh request.
pub(crate) struct CacheRefresh {
    /// Exact Provider namespace used for admission accounting.
    pub(crate) provider: ProviderName,
    /// Exact owning Provider connection.
    pub(crate) connection_id: tau_proto::ConnectionId,
    /// Sensitive directed request.
    pub(crate) request: tau_proto::AgentCacheRefreshRequested,
}

/// One directed cancellation retained against its exact Provider owner.
pub(crate) struct CacheRefreshCancel {
    /// Exact owning Provider connection.
    pub(crate) connection_id: tau_proto::ConnectionId,
    /// Content-free directed cancellation.
    pub(crate) request: tau_proto::AgentCacheRefreshCancelRequested,
}

/// Exact active attempt ownership and fail-safe deadline.
struct ActiveRefresh {
    /// Correlation identity accepted from one exact source.
    refresh_id: tau_proto::ProviderCacheRefreshId,
    /// Stop that releases ownership without a terminal.
    stop: Instant,
    /// Cancellation was sent but the slot remains retained.
    cancel_sent: bool,
}

/// A finite internal opportunity during which refresh work may run.
struct CacheRefreshWindow {
    /// Finite admission deadline; closure prevents new attempts.
    deadline: Instant,
}

/// Single process owner of all cache scheduling state.
pub(crate) struct ProviderCacheResidency<C, J> {
    /// Validated operator policy.
    config: ProviderCacheRefresh,
    /// Process-monotonic clock.
    clock: C,
    /// Jitter and identifier entropy.
    jitter: J,
    /// Process secret for prefix identities.
    digest_key: [u8; 32],
    /// Prompts awaiting terminal usage.
    tracked: HashMap<AgentPromptId, TrackedPrompt>,
    /// Bounded write/read evidence.
    evidence: HashMap<CacheKey, Evidence>,
    /// Deterministic evidence eviction order.
    evidence_order: VecDeque<CacheKey>,
    /// One authorized attempt per observation generation.
    scheduled: BTreeMap<CacheKey, Scheduled>,
    /// Slots retained until terminal, disconnect, stop, or shutdown.
    active: BTreeMap<CacheKey, ActiveRefresh>,
    /// Current finite admission opportunity; never hides active stops.
    window: Option<CacheRefreshWindow>,
}

impl<C: CacheClock, J: CacheJitter> ProviderCacheResidency<C, J> {
    /// Construct empty restart-clean state.
    pub(crate) fn new(
        config: ProviderCacheRefresh,
        clock: C,
        jitter: J,
        digest_key: [u8; 32],
    ) -> Self {
        Self {
            config,
            clock,
            jitter,
            digest_key,
            tracked: HashMap::new(),
            evidence: HashMap::new(),
            evidence_order: VecDeque::new(),
            scheduled: BTreeMap::new(),
            active: BTreeMap::new(),
            window: None,
        }
    }

    /// Capture an exact real prompt and supersede older work for its
    /// route/agent.
    pub(crate) fn track_prompt(
        &mut self,
        connection_id: tau_proto::ConnectionId,
        prompt: &AgentPromptCreated,
        model: Option<&ProviderModelInfo>,
    ) {
        if !self.config.enabled || prompt.operation != tau_proto::PromptOperation::Inference {
            return;
        }
        let Some(model) = model.cloned() else {
            return;
        };
        let Some(version) = model
            .cache_policy
            .map(|policy| policy.prefix_identity_version.get())
        else {
            return;
        };
        let Ok(bytes) = serde_json::to_vec(&(
            &prompt.system_prompt,
            &prompt.context,
            &prompt.tools,
            &prompt.model,
            prompt.model_params,
            prompt.tool_choice,
            &prompt.originator,
            prompt.share_user_cache_key,
        )) else {
            return;
        };
        let hash = blake3::keyed_hash(&self.digest_key, &bytes);
        let mut digest = [0; 16];
        digest.copy_from_slice(&hash.as_bytes()[..16]);
        let key = CacheKey {
            connection_id,
            provider: prompt.model.provider.clone(),
            model: prompt.model.clone(),
            agent_id: prompt.agent_id.clone(),
            prefix_identity_version: version,
            digest,
        };
        self.tracked.insert(
            prompt.agent_prompt_id.clone(),
            TrackedPrompt {
                key,
                request: tau_proto::AgentPromptPrewarmRequested {
                    agent_id: prompt.agent_id.clone(),
                    session_id: prompt.session_id.clone(),
                    system_prompt: prompt.system_prompt.clone(),
                    context: prompt.context.clone(),
                    tools: prompt.tools.clone(),
                    model: Some(prompt.model.clone()),
                    model_params: prompt.model_params,
                    tool_choice: prompt.tool_choice,
                    originator: prompt.originator.clone(),
                    share_user_cache_key: prompt.share_user_cache_key,
                },
                model,
            },
        );
    }

    /// Fold one successful ordinary response's cache read/write evidence.
    pub(crate) fn finish_prompt(
        &mut self,
        prompt_id: &AgentPromptId,
        successful: bool,
        usage: Option<&tau_proto::ProviderTokenUsage>,
    ) {
        let Some(tracked) = self.tracked.remove(prompt_id) else {
            return;
        };
        if !successful {
            return;
        }
        let Some(cache) = usage.and_then(|usage| usage.cache.as_deref()) else {
            return;
        };
        let Some((ttl, break_even)) = eligible(&tracked.model) else {
            self.invalidate_key(&tracked.key);
            return;
        };
        let write = cache.write_tokens.is_some_and(|tokens| 0 < tokens);
        let read = cache.read_tokens.is_some_and(|tokens| 0 < tokens);
        if !write && !read {
            return;
        }
        if read && !write && !self.evidence.contains_key(&tracked.key) {
            return;
        }
        let evidence = self
            .evidence
            .entry(tracked.key.clone())
            .or_insert(Evidence {
                reads_after_write: 0,
                saw_write: false,
                generation: 0,
            });
        if write {
            evidence.saw_write = true;
            evidence.reads_after_write = 0;
        } else if read && evidence.saw_write {
            evidence.reads_after_write = evidence.reads_after_write.saturating_add(1);
            evidence.generation = evidence.generation.saturating_add(1);
        }
        let reads_after_write = evidence.reads_after_write;
        let generation = evidence.generation;
        if write {
            self.record_evidence_key(tracked.key.clone());
        }
        if reads_after_write < break_even {
            return;
        }
        let now = self.clock.now();
        let Some(residency_deadline) = now.checked_add(ttl) else {
            return;
        };
        let Some(idle_deadline) = now.checked_add(self.config.max_idle_seconds.duration()) else {
            return;
        };
        let stop = residency_deadline.min(idle_deadline);
        let horizon = stop.saturating_duration_since(now);
        let maximum_jitter = 30_u64.min(horizon.as_secs() / 10);
        let jitter = if maximum_jitter == 0 {
            Duration::ZERO
        } else {
            Duration::from_secs(self.jitter.seconds(1, maximum_jitter))
        };
        let due = stop.checked_sub(jitter).unwrap_or(now);
        if due < now {
            return;
        }
        self.scheduled.insert(
            tracked.key.clone(),
            Scheduled {
                request: tracked.request,
                generation,
                due,
                stop,
            },
        );
        self.record_evidence_key(tracked.key);
    }

    /// Drop an unresolved prompt without creating refresh evidence.
    pub(crate) fn drop_prompt(&mut self, prompt_id: &AgentPromptId) {
        self.tracked.remove(prompt_id);
    }

    /// Return whether unresolved cache evidence retains `prompt_id`.
    #[cfg(test)]
    pub(crate) fn tracks_prompt(&self, prompt_id: &AgentPromptId) -> bool {
        self.tracked.contains_key(prompt_id)
    }

    /// Return the next due or exact stop deadline.
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        let mut active_providers = BTreeMap::<&ProviderName, usize>::new();
        for key in self.active.keys() {
            *active_providers.entry(&key.provider).or_default() += 1;
        }
        let scheduled = self.window.as_ref().into_iter().flat_map(|_| {
            self.scheduled.iter().flat_map(|(key, entry)| {
                let due = if self.active.len() < GLOBAL_CONCURRENCY
                    && active_providers.get(&key.provider).copied().unwrap_or(0)
                        < PROVIDER_CONCURRENCY
                {
                    entry.due
                } else {
                    entry.stop
                };
                [due, entry.stop]
            })
        });
        scheduled
            .chain(self.active.values().map(|active| active.stop))
            .min()
    }

    /// Open a finite tool-batch opportunity at the earliest candidate stop.
    pub(crate) fn open_tool_window(&mut self) {
        let Some(deadline) = self.scheduled.values().map(|entry| entry.stop).min() else {
            return;
        };
        self.window = Some(CacheRefreshWindow { deadline });
    }

    /// Close the current opportunity and cancel every active attempt.
    pub(crate) fn close_window(&mut self) -> Vec<CacheRefreshCancel> {
        self.window = None;
        self.cancel_active(tau_proto::ProviderCacheRefreshCancelReason::WaitEnded)
    }

    /// Admit due work only during the live finite internal opportunity.
    pub(crate) fn admit(&mut self) -> Vec<CacheRefresh> {
        let now = self.clock.now();
        self.expire(now);
        let Some(wait_deadline) = self.window.as_ref().map(|window| window.deadline) else {
            return Vec::new();
        };
        if wait_deadline <= now {
            return Vec::new();
        }
        let mut providers = BTreeMap::<ProviderName, usize>::new();
        for key in self.active.keys() {
            *providers.entry(key.provider.clone()).or_default() += 1;
        }
        let available = GLOBAL_CONCURRENCY.saturating_sub(self.active.len());
        let mut keys = Vec::new();
        for (key, entry) in &self.scheduled {
            if available <= keys.len() {
                break;
            }
            if entry.due <= now
                && now < entry.stop
                && now < wait_deadline
                && self
                    .evidence
                    .get(key)
                    .is_some_and(|evidence| evidence.generation == entry.generation)
                && providers.get(&key.provider).copied().unwrap_or(0) < PROVIDER_CONCURRENCY
            {
                *providers.entry(key.provider.clone()).or_default() += 1;
                keys.push(key.clone());
            }
        }
        let mut refreshes = Vec::with_capacity(keys.len());
        for key in keys {
            let Some(entry) = self.scheduled.remove(&key) else {
                continue;
            };
            let refresh_id = self.random_refresh_id();
            let stop_after = entry.stop.saturating_duration_since(now);
            let millis = stop_after.as_millis().clamp(1, 30_000);
            let stop_after_millis =
                NonZeroU32::new(u32::try_from(millis).expect("clamped to u32")).expect("nonzero");
            self.active.insert(
                key.clone(),
                ActiveRefresh {
                    refresh_id: refresh_id.clone(),
                    stop: entry.stop,
                    cancel_sent: false,
                },
            );
            refreshes.push(CacheRefresh {
                provider: key.provider.clone(),
                connection_id: key.connection_id.clone(),
                request: tau_proto::AgentCacheRefreshRequested {
                    refresh_id,
                    prompt: entry.request,
                    stop_after_millis,
                },
            });
        }
        refreshes
    }

    /// Cancel all work for real prompt priority.
    pub(crate) fn cancel_real(
        &mut self,
        provider: &ProviderName,
        agent_id: &AgentId,
    ) -> Vec<CacheRefreshCancel> {
        self.scheduled
            .retain(|key, _| &key.provider != provider || &key.agent_id != agent_id);
        self.cancel_matching(
            tau_proto::ProviderCacheRefreshCancelReason::RealPrompt,
            |key| &key.provider == provider && &key.agent_id == agent_id,
        )
    }

    /// Invalidate all state on session, Provider, model, tool, or prompt-policy
    /// change.
    pub(crate) fn clear(
        &mut self,
        reason: tau_proto::ProviderCacheRefreshCancelReason,
    ) -> Vec<CacheRefreshCancel> {
        self.tracked.clear();
        self.evidence.clear();
        self.evidence_order.clear();
        self.scheduled.clear();
        self.window = None;
        self.cancel_active(reason)
    }

    /// Authenticate and consume exactly one terminal report.
    pub(crate) fn finish(
        &mut self,
        connection_id: &tau_proto::ConnectionId,
        refresh_id: &tau_proto::ProviderCacheRefreshId,
    ) -> bool {
        let key = self.active.iter().find_map(|(key, active)| {
            (&key.connection_id == connection_id && &active.refresh_id == refresh_id)
                .then(|| key.clone())
        });
        key.is_some_and(|key| self.active.remove(&key).is_some())
    }

    /// Release every attempt owned by an authenticated disconnected connection.
    pub(crate) fn release_connection(&mut self, connection_id: &tau_proto::ConnectionId) {
        self.active
            .retain(|key, _| &key.connection_id != connection_id);
    }

    /// Cancel and release attempts whose exact process-monotonic stop elapsed.
    pub(crate) fn expire_deadlines(&mut self) -> Vec<CacheRefreshCancel> {
        let now = self.clock.now();
        let expired = self
            .active
            .iter()
            .filter(|(_, active)| active.stop <= now)
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        expired
            .into_iter()
            .filter_map(|key| {
                let active = self.active.remove(&key)?;
                Some(CacheRefreshCancel {
                    connection_id: key.connection_id,
                    request: tau_proto::AgentCacheRefreshCancelRequested {
                        refresh_id: active.refresh_id,
                        reason: tau_proto::ProviderCacheRefreshCancelReason::Deadline,
                    },
                })
            })
            .collect()
    }

    fn invalidate_key(&mut self, key: &CacheKey) {
        self.evidence.remove(key);
        self.evidence_order.retain(|candidate| candidate != key);
        self.scheduled.remove(key);
    }

    fn expire(&mut self, now: Instant) {
        self.scheduled.retain(|_, entry| now < entry.stop);
        if self
            .window
            .as_ref()
            .is_some_and(|window| window.deadline <= now)
        {
            self.window = None;
        }
    }

    fn cancel_active(
        &mut self,
        reason: tau_proto::ProviderCacheRefreshCancelReason,
    ) -> Vec<CacheRefreshCancel> {
        self.cancel_matching(reason, |_| true)
    }

    fn cancel_matching(
        &mut self,
        reason: tau_proto::ProviderCacheRefreshCancelReason,
        matches: impl Fn(&CacheKey) -> bool,
    ) -> Vec<CacheRefreshCancel> {
        let mut cancellations = Vec::new();
        for (key, active) in &mut self.active {
            if matches(key) && !active.cancel_sent {
                active.cancel_sent = true;
                cancellations.push(CacheRefreshCancel {
                    connection_id: key.connection_id.clone(),
                    request: tau_proto::AgentCacheRefreshCancelRequested {
                        refresh_id: active.refresh_id.clone(),
                        reason,
                    },
                });
            }
        }
        cancellations
    }

    fn record_evidence_key(&mut self, key: CacheKey) {
        self.evidence_order.retain(|candidate| candidate != &key);
        self.evidence_order.push_back(key);
        while 1_024 < self.evidence.len() {
            if let Some(oldest) = self.evidence_order.pop_front() {
                self.invalidate_key(&oldest);
            }
        }
        while 128
            < self
                .evidence
                .keys()
                .filter(|candidate| {
                    candidate.provider == self.evidence_order.back().expect("key").provider
                })
                .count()
        {
            let provider = self.evidence_order.back().expect("key").provider.clone();
            let Some(index) = self
                .evidence_order
                .iter()
                .position(|candidate| candidate.provider == provider)
            else {
                break;
            };
            let oldest = self.evidence_order.remove(index).expect("known index");
            self.invalidate_key(&oldest);
        }
    }

    fn random_refresh_id(&mut self) -> tau_proto::ProviderCacheRefreshId {
        loop {
            let mut nonce = [0_u8; 16];
            self.jitter.fill_bytes(&mut nonce);
            let encoded = nonce
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let id = tau_proto::ProviderCacheRefreshId::parse(format!("pcr-{encoded}"))
                .expect("generated refresh id follows validated grammar");
            if self.active.values().all(|active| active.refresh_id != id) {
                return id;
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn install_active_for_test(
        &mut self,
        connection_id: tau_proto::ConnectionId,
        refresh_id: tau_proto::ProviderCacheRefreshId,
    ) {
        self.active.insert(
            CacheKey {
                connection_id,
                provider: ProviderName::new("provider"),
                model: "provider/model".into(),
                agent_id: AgentId::parse("agent").expect("test agent id"),
                prefix_identity_version: 1,
                digest: [0; 16],
            },
            ActiveRefresh {
                refresh_id,
                stop: self.clock.now() + Duration::from_secs(30),
                cancel_sent: false,
            },
        );
    }

    #[cfg(test)]
    /// Make every pending observation generation immediately dispatchable.
    pub(crate) fn force_scheduled_due_for_test(&mut self) {
        let now = self.clock.now();
        for scheduled in self.scheduled.values_mut() {
            scheduled.due = now;
        }
    }

    #[cfg(test)]
    /// Make every active attempt eligible for exact deadline service.
    pub(crate) fn force_active_expired_for_test(&mut self) {
        let now = self.clock.now();
        for active in self.active.values_mut() {
            active.stop = now;
        }
    }
}

fn eligible(model: &ProviderModelInfo) -> Option<(Duration, u64)> {
    let policy = model.cache_policy?;
    if !matches!(
        policy.kind,
        tau_proto::ProviderCacheKind::AutomaticPrefix
            | tau_proto::ProviderCacheKind::ExplicitBreakpoint
    ) || policy.renewal != ProviderCacheRenewal::Read
        || policy.output_floor != ProviderCacheOutputFloor::Zero
        || policy.privacy.storage != ProviderCacheStorageMode::VolatileMemory
        || policy.privacy.zero_data_retention
            != ProviderCacheZeroDataRetentionCompatibility::Compatible
        || policy.privacy.data_residency != ProviderCacheDataResidencyEffect::PreservesRoutePolicy
        || [
            policy.quota.requests,
            policy.quota.read_tokens,
            policy.quota.write_tokens,
            policy.quota.output_tokens,
        ]
        .into_iter()
        .any(|charge| {
            matches!(
                charge,
                ProviderCacheQuotaCharge::Unknown | ProviderCacheQuotaCharge::ProviderSpecific
            )
        })
    {
        return None;
    }
    let ProviderCacheTtl::SlidingKnown { seconds } = policy.ttl else {
        return None;
    };
    let uncached = u128::from(model.est_uncached_input_cost_1m_usd?.as_micro_usd());
    let read = u128::from(model.est_cached_input_cost_1m_usd?.as_micro_usd());
    let write = u128::from(model.est_cache_write_input_cost_1m_usd?.as_micro_usd());
    if uncached <= read || write < read.saturating_mul(2) {
        return None;
    }
    let numerator = write.saturating_sub(uncached);
    let denominator = uncached - read;
    let reads = numerator
        .checked_add(denominator - 1)?
        .checked_div(denominator)?
        .max(1);
    Some((
        Duration::from_secs(seconds.get()),
        u64::try_from(reads).ok()?,
    ))
}
