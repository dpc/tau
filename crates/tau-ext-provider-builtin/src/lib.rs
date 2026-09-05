//! Built-in provider registry extension.
//!
//! This crate owns Tau's built-in provider process, registration CLI, scoped
//! credential hydration, model publication, and dispatch across built-in
//! provider backends. Individual backend crates own provider-specific wire
//! formats. Component responsibilities and trust boundaries are summarized in
//! `ARCH-tau-ext-provider-builtin`.

use std::collections::hash_map as path_std_collections_hash_map;
use std::io as path_std_io;

mod backend_observation;
mod cache_contract;
mod chat_completions;
mod credential_record;
mod oauth_refresh_rejection;
mod openai_prompt_cache;
mod output_cost_observation;
mod prewarm;
mod provider_settings_validation;
#[cfg(feature = "quota-test-support")]
mod quota_test_support;
mod reasoning_effort_mapping;
mod receipt_observation;
mod report_sink;
mod responses;
mod setup_store;
mod worker_report_sink;

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet, VecDeque};
use std::error::Error;
use std::hash::{Hash, Hasher};
#[cfg(test)]
use std::io::Cursor;
use std::io::{Read, Write};
use std::marker::PhantomData;
#[cfg(test)]
use std::sync::TryLockError;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use backend_observation::{
    chat_completions_backend, codex_backend as backend_descriptor, observed_backend,
    responses_backend,
};
pub use cache_contract::ProviderCacheContract;
pub use chat_completions::{
    ChatCompletionsCompat, ChatCompletionsModel, ChatCompletionsProvider,
    ChatCompletionsReasoningEffort, ChatCompletionsReasoningEffortWire,
    ChatCompletionsReasoningReplay, LocalSummaryCompactionConfig,
    OpenAiPromptCache as ChatCompletionsOpenAiPromptCache, OpenAiPromptCacheBoundary,
    OpenAiPromptCacheMode, OpenAiPromptCacheOptions, OpenAiPromptCacheTtl,
    OpenRouterDiscoveryError, OpenRouterProfile,
};
use chat_completions::{
    PromptAttemptOutcome as ChatCompletionsAttemptOutcome, fetch_openrouter_models,
    models_for_provider as chat_models_for_provider, run_prompt_attempt,
};
use dialoguer::{Confirm, Input, Password, Select};
use oauth_refresh_rejection::{OAuthRefreshRejectionCache, RefreshCredentialsError};
pub use openai_prompt_cache::OpenAiPromptCacheKey;
use output_cost_observation::{
    SamplerObservation, WorkerDrainObservation, WorkerOutputObservation, WorkerQueueState,
};
use prewarm::{PrewarmAbort, PrewarmKey, PrewarmSupervisor};
use provider_settings_validation::{
    ProviderSettingsValidationError, ProviderSettingsValidationReason,
    reject_obsolete_local_summary_fields,
};
#[cfg(feature = "quota-test-support")]
pub use quota_test_support::run_quota_recovery_fixture;
pub use reasoning_effort_mapping::ReasoningEffortMapping;
use receipt_observation::{ReceiptObservation, ReceiptOutcome};
use report_sink::ProviderReportSink;
pub use responses::{
    OpenAiPromptCache as ResponsesOpenAiPromptCache,
    OpenAiPromptCacheBoundary as ResponsesOpenAiPromptCacheBoundary,
    OpenAiPromptCacheMode as ResponsesOpenAiPromptCacheMode,
    OpenAiPromptCacheOptions as ResponsesOpenAiPromptCacheOptions,
    OpenAiPromptCacheTtl as ResponsesOpenAiPromptCacheTtl, ResponsesCompat, ResponsesModel,
    ResponsesProvider,
};
use responses::{
    PromptAttemptOutcome as ResponsesAttemptOutcome,
    models_for_provider as responses_models_for_provider,
    run_prompt_attempt as run_responses_prompt_attempt,
};
use serde::{Deserialize, Serialize};
use tau_client::{
    ClientError, ClientHandle, ClientResult, DispatchOutcome, ExtensionBuilder,
    ExtensionDataClient, ManualExtensionRuntime, ManualRuntimePoll, ManualRuntimeWaker,
    RawEventContext, TauExtension, TauExtensionRunner,
};
use tau_config::provider_settings::{
    ProviderCredential, ProviderCredentialIdentity, ProviderCredentialReference,
    ProviderCredentialSlot, parse_provider_credential,
};
use tau_config::settings::BuiltinComponentIdentity;
#[cfg(test)]
use tau_proto::PeerOutputWriter;
use tau_proto::{
    ClientKind, ContextItem, Event, EventName, HarnessInputMessage, ModelId, ModelName,
    ProviderBackend, ProviderBackendTransport, ProviderCacheMissDiagnostic, ProviderModelInfo,
    ProviderModelsDeclared, ProviderName, ProviderPromptSubmitted, ProviderResponseFinished,
    ProviderResponseStats, ProviderResponseStatusUpdate, ProviderResponseUpdated,
    ProviderStopReason, SecretValue, ServerOffsetMillis, UnixMillis,
};
use tau_provider::local_summary_compaction::ConfigError as SummaryCompactionConfigError;
use tau_provider::retry_policy::{RetryClass, RetryDecision};
use tau_provider_codex::{
    AttemptOutcome as CodexAttemptOutcome, ChatGptRetryIdentity, CodexError, CodexMode,
    CodexRuntime, CompactOutcome, InferenceProfileIdentity, PrewarmOutcome, Prompt as CodexPrompt,
    QuotaProfileIdentity, ResolvedConfig, SemanticProgress as CodexSemanticProgress,
    StreamDeltaEmitter as CodexStreamDeltaEmitter, StreamState as CodexStreamState, StreamUpdate,
    TurnAbort, TurnAbortWaker,
};
pub use tau_provider_responses::Transport as ResponsesTransport;
use worker_report_sink::WorkerReportSink;
#[cfg(test)]
use worker_report_sink::{WorkerReportWaker, prepare_worker_report};

/// `tracing` target for events emitted from this extension.
pub const LOG_TARGET: &str = "provider-builtin";

const EXTENSION_NAME: &str = "tau-ext-provider-builtin";
const CHATGPT_PROVIDER_NAME: &str = "chatgpt";
const DEFAULT_RESPONSES_LITE_COMPATIBILITY: bool = false;
const PROMPT_CREDENTIAL_RPC_TIMEOUT: Duration = Duration::from_secs(30);
/// Bounded control mailbox for the single prompt-credential deadline actor.
const PROMPT_CREDENTIAL_DEADLINE_MAILBOX_CAPACITY: usize = 256;

/// Parsed, immutable provider settings accepted by initial Configure.
type SettingsSnapshot = Arc<Mutex<BuiltinProviderProfiles>>;

#[cfg(test)]
fn test_network_policy() -> tau_provider::OutboundNetworkPolicy {
    tau_provider::OutboundNetworkPolicy::from_environment(BTreeMap::new(), None)
}
/// One built-in provider profile hydrated from settings plus a typed
/// credential.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BuiltinProviderProfile {
    /// ChatGPT/Codex OAuth provider using the Responses backend.
    Chatgpt(ChatGptProfile),
    /// OpenAI-compatible Chat Completions provider.
    ChatCompletions(ChatCompletionsProvider),
    /// OpenRouter provider using a wrapped Chat Completions backend.
    #[serde(rename = "openrouter")]
    OpenRouter(OpenRouterProfile),
    /// Generic public API-key Responses provider with explicit transport.
    Responses(ResponsesProvider),
}

impl BuiltinProviderProfile {
    /// Validate profile-wide invariants after serde has decoded its local
    /// fields.
    fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::ChatCompletions(provider) => provider.validate(),
            Self::OpenRouter(profile) => profile.validate(),
            Self::Responses(provider) => provider.validate_reasoning_effort(),
            Self::Chatgpt(_) => Ok(()),
        }
    }

    /// Validate model-local summary limits consistently across compatible
    /// provider families.
    fn validate_local_summary_compaction(&self) -> Result<(), SummaryCompactionConfigError> {
        match self {
            Self::ChatCompletions(provider) => provider
                .models
                .iter()
                .try_for_each(ChatCompletionsModel::validate_local_summary_compaction),
            Self::OpenRouter(profile) => profile
                .models
                .iter()
                .try_for_each(ChatCompletionsModel::validate_local_summary_compaction),
            Self::Responses(provider) => provider.validate_local_summary_compaction(),
            Self::Chatgpt(_) => Ok(()),
        }
    }
}

/// ChatGPT/Codex provider profile.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatGptProfile {
    /// OAuth credentials used for ChatGPT/Codex Responses calls.
    #[serde(default)]
    pub auth: OpenAiAuth,
    /// Use the legacy Responses Lite contract for audited GPT-5.6 routes.
    ///
    /// This is startup-stable route configuration, not authentication data.
    #[serde(default, skip_serializing_if = "is_false")]
    pub responses_lite_compatibility: bool,
}

impl ChatGptProfile {
    fn responses_mode(&self) -> CodexMode {
        if self.responses_lite_compatibility {
            CodexMode::LiteCompatibility
        } else {
            CodexMode::Standard
        }
    }

    #[cfg(test)]
    fn replace_auth(&mut self, refreshed: OpenAiAuth) {
        self.auth = refreshed;
    }
}

/// Registered built-in providers keyed by filename-derived namespace.
#[derive(Clone, Debug, Default)]
pub struct BuiltinProviderProfiles {
    providers: BTreeMap<ProviderName, BuiltinProviderProfile>,
    /// Explicit authentication selection retained for each parsed profile.
    credentials: BTreeMap<ProviderName, ProviderCredential>,
    /// ChatGPT profiles whose OAuth Secret was positively found absent while
    /// hydrating this prompt-time settings snapshot.
    missing_logins: BTreeSet<ProviderName>,
}

impl BuiltinProviderProfiles {
    /// Clones only one indexed profile and its credential selection.
    fn selected(&self, provider: &ProviderName) -> Self {
        let mut selected = Self::default();
        if let Some(profile) = self.providers.get(provider) {
            selected.providers.insert(provider.clone(), profile.clone());
        }
        if let Some(credential) = self.credentials.get(provider) {
            selected
                .credentials
                .insert(provider.clone(), credential.clone());
        }
        selected
    }

    /// Returns whether credential hydration found this ChatGPT profile logged
    /// out.
    fn missing_login(&self, provider: &ProviderName) -> bool {
        self.missing_logins.contains(provider)
    }

    /// Returns the stored OAuth credential selection for one ChatGPT profile.
    fn chatgpt_credential_reference(
        &self,
        provider: &ProviderName,
    ) -> Option<&ProviderCredentialReference> {
        match self.credentials.get(provider) {
            Some(ProviderCredential::Stored(reference))
                if reference.slot() == ProviderCredentialSlot::OAuth =>
            {
                Some(reference)
            }
            Some(ProviderCredential::Stored(_)) | Some(ProviderCredential::Keyless) | None => None,
        }
    }

    fn startup_responses_modes(&self) -> BTreeMap<ProviderName, CodexMode> {
        self.providers
            .iter()
            .filter_map(|(provider, profile)| match profile {
                BuiltinProviderProfile::Chatgpt(profile) => {
                    Some((provider.clone(), profile.responses_mode()))
                }
                BuiltinProviderProfile::ChatCompletions(_)
                | BuiltinProviderProfile::OpenRouter(_)
                | BuiltinProviderProfile::Responses(_) => None,
            })
            .collect()
    }

    fn apply_startup_responses_modes(&mut self, startup_modes: &BTreeMap<ProviderName, CodexMode>) {
        for (provider, profile) in &mut self.providers {
            let BuiltinProviderProfile::Chatgpt(profile) = profile else {
                continue;
            };
            profile.responses_lite_compatibility = startup_modes
                .get(provider)
                .copied()
                .unwrap_or_default()
                .is_lite_compatibility();
        }
    }

    #[cfg(test)]
    fn resolve_initial_quota_backends<R>(
        &mut self,
        mut resolve: impl FnMut(&ModelId, &mut Self) -> Option<R>,
    ) -> Vec<(ProviderName, R)> {
        let models = self
            .providers
            .iter()
            .filter_map(|(provider, profile)| {
                let BuiltinProviderProfile::Chatgpt(profile) = profile else {
                    return None;
                };
                tau_provider_codex::models_for_provider_mode(provider, profile.responses_mode())
                    .into_iter()
                    .next()
                    .map(|model| model.id)
            })
            .collect::<Vec<_>>();
        models
            .into_iter()
            .filter_map(|model| {
                let backend = resolve(&model, self)?;
                Some((model.provider, backend))
            })
            .collect()
    }
}

/// OAuth credentials for the ChatGPT/Codex Responses provider.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenAiAuth {
    /// ChatGPT access token used as bearer auth for Codex Responses calls.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub access_token: String,
    /// Refresh token used to renew [`Self::access_token`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub refresh_token: String,
    /// Milliseconds since epoch when [`Self::access_token`] expires.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub expires_at_ms: u64,
    /// OpenAI account id sent as `chatgpt-account-id`, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(not(test))]
const RETRY_BASE_DELAY: Duration = Duration::from_secs(10);
#[cfg(test)]
const RETRY_BASE_DELAY: Duration = Duration::from_millis(10);
const RESET_BOUNDARY_JITTER_MAX: Duration = Duration::from_secs(5);
const RETRY_SCHEDULER_MAILBOX_CAPACITY: usize = 1_024;
const PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL: Duration = Duration::from_secs(1);
const QUOTA_FETCH_MIN_INTERVAL: Duration = Duration::from_secs(60);
const QUOTA_REFRESH_INTERVAL: Duration = Duration::from_secs(15 * 60);

#[derive(Clone)]
struct QuotaWindowRecord {
    window: tau_proto::ProviderQuotaWindow,
    updated_sequence: tau_proto::ProviderQuotaSequence,
}

struct QuotaProfileState {
    identity: QuotaProfileIdentity,
    epoch: tau_proto::ProviderQuotaEpoch,
    sequence: tau_proto::ProviderQuotaSequence,
    windows: BTreeMap<tau_proto::ProviderQuotaWindowKey, QuotaWindowRecord>,
    bindings: BTreeMap<ModelId, tau_proto::ProviderQuotaRouteBinding>,
    fetch_in_flight: bool,
    last_fetch_started: Option<Instant>,
    refresh_generation: u64,
    failure_attempt: u8,
}

/// Single-runtime-loop quota epoch, merge, and fetch-coalescing owner.
#[derive(Default)]
struct QuotaCoordinator {
    profiles: BTreeMap<ProviderName, QuotaProfileState>,
    next_epoch: u64,
}

impl QuotaCoordinator {
    fn profile_epoch(&self, provider: &ProviderName) -> Option<tau_proto::ProviderQuotaEpoch> {
        self.profiles
            .get(provider)
            .map(|current| current.epoch.clone())
    }

    fn epoch_matches(
        &self,
        provider: &ProviderName,
        epoch: &tau_proto::ProviderQuotaEpoch,
    ) -> bool {
        self.profiles
            .get(provider)
            .is_some_and(|current| &current.epoch == epoch)
    }

    fn ensure_profile(
        &mut self,
        provider: ProviderName,
        identity: impl Into<QuotaProfileIdentity>,
    ) -> Option<Event> {
        let identity = identity.into();
        if self
            .profiles
            .get(&provider)
            .is_some_and(|current| current.identity == identity)
        {
            return None;
        }
        self.next_epoch = self.next_epoch.saturating_add(1);
        let epoch =
            tau_proto::ProviderQuotaEpoch::parse(format!("q-{}-{}", now_ms(), self.next_epoch))
                .expect("generated quota epoch is valid");
        self.profiles.insert(
            provider.clone(),
            QuotaProfileState {
                identity,
                epoch: epoch.clone(),
                sequence: tau_proto::ProviderQuotaSequence::new(1),
                windows: BTreeMap::new(),
                bindings: BTreeMap::new(),
                fetch_in_flight: false,
                last_fetch_started: None,
                refresh_generation: 0,
                failure_attempt: 0,
            },
        );
        Some(Event::ProviderQuotaReplaceReported(
            tau_proto::ProviderQuotaReplace {
                provider,
                profile_epoch: epoch,
                sequence: tau_proto::ProviderQuotaSequence::new(1),
                establishes_new_epoch: true,
                windows: Vec::new(),
                route_bindings: Vec::new(),
            },
        ))
    }

    fn clear_profile(&mut self, provider: &ProviderName) -> Option<Event> {
        let current = self.profiles.remove(provider)?;
        Some(Event::ProviderQuotaClearReported(
            tau_proto::ProviderQuotaClear {
                provider: provider.clone(),
                profile_epoch: current.epoch,
                sequence: current.sequence.saturating_next(),
            },
        ))
    }

    fn refresh_delay(&self, provider: &ProviderName) -> Duration {
        let now = UnixMillis::new(now_ms());
        self.profiles
            .get(provider)
            .into_iter()
            .flat_map(|current| current.windows.values())
            .filter_map(|record| {
                let (remaining, anchor) = (
                    record.window.remaining_seconds_at_timing_anchor?,
                    record.window.timing_anchor_observed_at_unix_ms?,
                );
                let age = i64::try_from(elapsed_seconds_since(anchor, now)).ok()?;
                u64::try_from(remaining.get().saturating_sub(age)).ok()
            })
            .map(|seconds| Duration::from_secs(seconds).saturating_add(Duration::from_secs(1)))
            .min()
            .unwrap_or(QUOTA_REFRESH_INTERVAL)
            .min(QUOTA_REFRESH_INTERVAL)
            .max(Duration::from_secs(1))
    }

    fn begin_fetch(
        &mut self,
        provider: &ProviderName,
    ) -> Option<(
        tau_proto::ProviderQuotaEpoch,
        tau_proto::ProviderQuotaSequence,
    )> {
        let current = self.profiles.get_mut(provider)?;
        let reset_due = current.windows.values().any(|record| {
            record
                .window
                .remaining_seconds_at_timing_anchor
                .zip(record.window.timing_anchor_observed_at_unix_ms)
                .is_some_and(|(remaining, anchor)| {
                    let age_seconds = elapsed_seconds_since(anchor, UnixMillis::new(now_ms()));
                    u64::try_from(remaining.get())
                        .ok()
                        .is_none_or(|remaining| remaining <= age_seconds)
                })
        });
        if current.fetch_in_flight
            || current.last_fetch_started.is_some_and(|started| {
                started.elapsed() < QUOTA_FETCH_MIN_INTERVAL
                    || (!current.windows.is_empty()
                        && !reset_due
                        && started.elapsed() < QUOTA_REFRESH_INTERVAL)
            })
        {
            return None;
        }
        current.fetch_in_flight = true;
        current.last_fetch_started = Some(Instant::now());
        Some((current.epoch.clone(), current.sequence))
    }

    fn schedule_refresh(
        &mut self,
        provider: &ProviderName,
        epoch: &tau_proto::ProviderQuotaEpoch,
    ) -> Option<u64> {
        let current = self.profiles.get_mut(provider)?;
        if &current.epoch != epoch {
            return None;
        }
        current.refresh_generation = current.refresh_generation.saturating_add(1);
        Some(current.refresh_generation)
    }

    fn refresh_is_current(
        &self,
        provider: &ProviderName,
        epoch: &tau_proto::ProviderQuotaEpoch,
        generation: u64,
    ) -> bool {
        self.profiles.get(provider).is_some_and(|current| {
            &current.epoch == epoch && current.refresh_generation == generation
        })
    }

    fn failure_delay(&self, provider: &ProviderName) -> Duration {
        let attempt = self
            .profiles
            .get(provider)
            .map_or(0, |current| current.failure_attempt.min(4));
        QUOTA_FETCH_MIN_INTERVAL
            .saturating_mul(1_u32 << u32::from(attempt))
            .min(QUOTA_REFRESH_INTERVAL)
    }

    fn fail_fetch(&mut self, provider: &ProviderName, epoch: &tau_proto::ProviderQuotaEpoch) {
        if let Some(current) = self.profiles.get_mut(provider)
            && &current.epoch == epoch
        {
            current.fetch_in_flight = false;
            current.failure_attempt = current.failure_attempt.saturating_add(1);
        }
    }

    fn finish_fetch(
        &mut self,
        provider: ProviderName,
        epoch: tau_proto::ProviderQuotaEpoch,
        fetch_start_sequence: tau_proto::ProviderQuotaSequence,
        snapshot: tau_provider_codex::FullQuotaSnapshot,
        observed_at_unix_ms: UnixMillis,
    ) -> Option<Event> {
        let current = self.profiles.get_mut(&provider)?;
        if current.epoch != epoch {
            return None;
        }
        current.fetch_in_flight = false;
        let mut fetched = BTreeMap::new();
        for observation in snapshot.windows {
            let window = full_quota_window(observation, observed_at_unix_ms)?;
            fetched.insert(window.key.clone(), window);
        }
        let mut candidate = current.windows.clone();
        candidate.retain(|key, record| {
            record.updated_sequence > fetch_start_sequence || fetched.contains_key(key)
        });
        for (key, window) in fetched {
            if candidate
                .get(&key)
                .is_some_and(|record| record.updated_sequence > fetch_start_sequence)
            {
                continue;
            }
            candidate.insert(
                key,
                QuotaWindowRecord {
                    window,
                    updated_sequence: current.sequence.saturating_next(),
                },
            );
        }
        if candidate.len() > tau_proto::MAX_PROVIDER_QUOTA_WINDOWS {
            current.failure_attempt = current.failure_attempt.saturating_add(1);
            return None;
        }
        current.sequence = current.sequence.saturating_next();
        current.failure_attempt = 0;
        let sequence = current.sequence;
        current.windows = candidate;
        Some(Event::ProviderQuotaReplaceReported(
            tau_proto::ProviderQuotaReplace {
                provider,
                profile_epoch: epoch,
                sequence,
                establishes_new_epoch: false,
                windows: current
                    .windows
                    .values()
                    .map(|record| record.window.clone())
                    .collect(),
                route_bindings: current.bindings.values().cloned().collect(),
            },
        ))
    }

    fn merge_rolling(
        &mut self,
        model: ModelId,
        profile_identity: impl Into<QuotaProfileIdentity>,
        observation: tau_provider_codex::RollingQuotaObservation,
        observed_at_unix_ms: UnixMillis,
    ) -> Option<Event> {
        let profile_identity = profile_identity.into();
        let provider = model.provider.clone();
        let current = self.profiles.get_mut(&provider)?;
        if current.identity != profile_identity {
            return None;
        }
        let incoming_keys = observation
            .windows
            .iter()
            .map(|window| tau_proto::ProviderQuotaWindowKey {
                limit_id: window.limit_id.clone(),
                window_id: window.window_id.clone(),
            })
            .collect::<HashSet<_>>();
        let resulting_window_count = current
            .windows
            .keys()
            .filter(|key| !incoming_keys.contains(*key))
            .count()
            .saturating_add(incoming_keys.len());
        let adding_binding =
            observation.active_limit_id.is_some() && !current.bindings.contains_key(&model);
        if observation.windows.len() > tau_proto::MAX_PROVIDER_QUOTA_WINDOWS
            || incoming_keys.len() != observation.windows.len()
            || resulting_window_count > tau_proto::MAX_PROVIDER_QUOTA_WINDOWS
            || (adding_binding && current.bindings.len() >= tau_proto::MAX_PROVIDER_QUOTA_BINDINGS)
            || observation.active_limit_id.is_some() != observation.binding_provenance.is_some()
        {
            return None;
        }
        current.sequence = current.sequence.saturating_next();
        let sequence = current.sequence;
        let mut windows = Vec::new();
        for sparse in observation.windows {
            let key = tau_proto::ProviderQuotaWindowKey {
                limit_id: sparse.limit_id.clone(),
                window_id: sparse.window_id.clone(),
            };
            let previous = current.windows.get(&key).map(|record| &record.window);
            let Some(window) = merge_sparse_quota_window(previous, sparse, observed_at_unix_ms)
            else {
                continue;
            };
            current.windows.insert(
                key,
                QuotaWindowRecord {
                    window: window.clone(),
                    updated_sequence: sequence,
                },
            );
            windows.push(window);
        }
        let mut route_bindings = Vec::new();
        if let (Some(limit_id), Some(provenance)) =
            (observation.active_limit_id, observation.binding_provenance)
        {
            let binding = tau_proto::ProviderQuotaRouteBinding {
                model: model.clone(),
                limit_ids: vec![limit_id],
                observed_at_unix_ms,
                provenance,
            };
            current.bindings.insert(model, binding.clone());
            route_bindings.push(binding);
        }
        if windows.is_empty() && route_bindings.is_empty() {
            return None;
        }
        Some(Event::ProviderQuotaPatchReported(
            tau_proto::ProviderQuotaPatch {
                provider,
                profile_epoch: current.epoch.clone(),
                sequence,
                windows,
                removed_window_keys: Vec::new(),
                route_bindings,
            },
        ))
    }
}

/// Wrap one provider quota observation in explicitly transient publication
/// metadata.
fn quota_report_message(event: Event) -> HarnessInputMessage {
    // ast-grep-ignore: debug-assert-expression-must-not-mutate
    debug_assert!(matches!(
        event,
        Event::ProviderQuotaReplaceReported(_)
            | Event::ProviderQuotaPatchReported(_)
            | Event::ProviderQuotaClearReported(_)
    ));
    HarnessInputMessage::emit_with_persist(event, false)
}

fn send_cache_refresh_terminal(
    handle: &ClientHandle,
    refresh_id: tau_proto::ProviderCacheRefreshId,
    status: tau_proto::ProviderCacheRefreshStatus,
) -> ClientResult<()> {
    handle.send(HarnessInputMessage::emit_transient(
        Event::ProviderCacheRefreshFinishedReported(tau_proto::ProviderCacheRefreshFinished {
            refresh_id,
            status,
        }),
    ))
}

fn quota_profile_identity(config: &ResolvedConfig) -> QuotaProfileIdentity {
    config.profile_identity()
}

fn responses_profile_identity(config: &ResolvedConfig) -> InferenceProfileIdentity {
    config.inference_identity()
}

/// Process-local correlation identity for one resolved provider backend
/// generation.
///
/// Its opaque hash combines the backend and credential state used by prompt
/// workers, cooldowns, and OAuth recovery. It never crosses a protocol or
/// persistence boundary.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct BackendProfileIdentity {
    /// Opaque hash of the resolved backend generation.
    hash: u64,
}

#[cfg(test)]
impl BackendProfileIdentity {
    /// Constructs a deterministic backend identity for focused state tests.
    fn from_test_value(value: u64) -> Self {
        Self { hash: value }
    }
}

fn backend_profile_identity(backend: &PromptBackend) -> Option<BackendProfileIdentity> {
    let mut hasher = path_std_collections_hash_map::DefaultHasher::new();
    match backend {
        PromptBackend::Unavailable { .. } => return None,
        PromptBackend::Responses(config) => {
            responses_profile_identity(config).hash(&mut hasher);
        }
        PromptBackend::ChatCompletions { provider, .. } => {
            "chat_completions".hash(&mut hasher);
            provider.base_url.hash(&mut hasher);
            provider.api_key.hash(&mut hasher);
        }
        PromptBackend::PublicResponses { provider, .. } => {
            "responses".hash(&mut hasher);
            provider.base_url.hash(&mut hasher);
            provider.api_key.hash(&mut hasher);
        }
    }
    Some(BackendProfileIdentity {
        hash: hasher.finish(),
    })
}

fn automatic_retry_identity_matches(
    pinned: Option<&ChatGptRetryIdentity>,
    next: &PromptBackend,
) -> bool {
    match (pinned, next) {
        (Some(pinned), PromptBackend::Responses(next)) => {
            next.matches_chatgpt_retry_identity(pinned)
        }
        (Some(_), PromptBackend::Unavailable { .. }) => true,
        (Some(_), _) => false,
        (None, _) => true,
    }
}

/// Converts two Unix-millisecond observations into the elapsed whole seconds.
fn elapsed_seconds_since(anchor: UnixMillis, now: UnixMillis) -> u64 {
    now.get().saturating_sub(anchor.get()).div_ceil(1_000)
}

fn full_quota_window(
    observation: tau_provider_codex::QuotaWindowObservation,
    observed_at_unix_ms: UnixMillis,
) -> Option<tau_proto::ProviderQuotaWindow> {
    let window_seconds = observation.window_seconds?;
    let server_offset_ms = match (
        observation.reset_at_unix_seconds,
        observation.remaining_seconds,
    ) {
        (Some(reset), Some(remaining)) => i128::from(reset.get())
            .checked_sub(i128::from(remaining.get()))?
            .checked_mul(1_000)?
            .checked_sub(i128::from(observed_at_unix_ms.get()))?
            .try_into()
            .ok(),
        _ => None,
    };
    Some(tau_proto::ProviderQuotaWindow {
        key: tau_proto::ProviderQuotaWindowKey {
            limit_id: observation.limit_id,
            window_id: observation.window_id,
        },
        used_basis_points: observation.used_basis_points,
        usage_observed_at_unix_ms: observed_at_unix_ms,
        window_seconds,
        reset_at_unix_seconds: observation.reset_at_unix_seconds,
        remaining_seconds_at_timing_anchor: observation.remaining_seconds,
        timing_anchor_observed_at_unix_ms: observation
            .remaining_seconds
            .map(|_| observed_at_unix_ms),
        server_offset_ms: server_offset_ms.map(ServerOffsetMillis::new),
        server_offset_observed_at_unix_ms: server_offset_ms.map(|_| observed_at_unix_ms),
    })
}

fn merge_sparse_quota_window(
    previous: Option<&tau_proto::ProviderQuotaWindow>,
    sparse: tau_provider_codex::QuotaWindowObservation,
    observed_at_unix_ms: UnixMillis,
) -> Option<tau_proto::ProviderQuotaWindow> {
    let duration_changed = previous.is_some_and(|previous| {
        sparse
            .window_seconds
            .is_some_and(|seconds| seconds != previous.window_seconds)
    });
    let window_seconds = sparse
        .window_seconds
        .or_else(|| previous.map(|window| window.window_seconds))?;
    let reset_at_unix_seconds = sparse
        .reset_at_unix_seconds
        .or_else(|| previous.and_then(|window| window.reset_at_unix_seconds));
    let unsafe_usage_decrease = previous.is_some_and(|previous| {
        sparse.used_basis_points.saturating_add(100) < previous.used_basis_points
    });
    let reset_transition_trusted = previous.is_none_or(|previous| {
        let (Some(old), Some(new)) = (previous.reset_at_unix_seconds, sparse.reset_at_unix_seconds)
        else {
            return true;
        };
        if old.get().abs_diff(new.get()) <= 60 {
            return true;
        }
        if new < old {
            return false;
        }
        let old_remaining = previous
            .remaining_seconds_at_timing_anchor
            .zip(previous.timing_anchor_observed_at_unix_ms)
            .map(|(remaining, anchor)| {
                let age_seconds = i64::try_from(elapsed_seconds_since(anchor, observed_at_unix_ms))
                    .unwrap_or(i64::MAX);
                remaining.get().saturating_sub(age_seconds)
            });
        old_remaining.is_some_and(|remaining| remaining <= 5 * 60)
    });
    let timing_trusted = !duration_changed && !unsafe_usage_decrease && reset_transition_trusted;
    let retain_relative = timing_trusted && sparse.reset_at_unix_seconds.is_none();
    Some(tau_proto::ProviderQuotaWindow {
        key: tau_proto::ProviderQuotaWindowKey {
            limit_id: sparse.limit_id,
            window_id: sparse.window_id,
        },
        used_basis_points: sparse.used_basis_points,
        usage_observed_at_unix_ms: observed_at_unix_ms,
        window_seconds,
        reset_at_unix_seconds,
        remaining_seconds_at_timing_anchor: retain_relative
            .then(|| previous.and_then(|window| window.remaining_seconds_at_timing_anchor))
            .flatten(),
        timing_anchor_observed_at_unix_ms: retain_relative
            .then(|| previous.and_then(|window| window.timing_anchor_observed_at_unix_ms))
            .flatten(),
        server_offset_ms: timing_trusted
            .then(|| previous.and_then(|window| window.server_offset_ms))
            .flatten(),
        server_offset_observed_at_unix_ms: timing_trusted
            .then(|| previous.and_then(|window| window.server_offset_observed_at_unix_ms))
            .flatten(),
    })
}

/// Default number of provider prompts allowed to execute concurrently.
const DEFAULT_PROMPT_CONCURRENCY: usize = 8;

/// Environment override for prompt execution concurrency.
const PROMPT_CONCURRENCY_ENV: &str = "TAU_BUILTIN_PROVIDER_PROMPT_CONCURRENCY";

/// Runs setup commands for registered built-in providers.
pub fn run_provider_cli(args: &[String]) -> Result<(), Box<dyn Error>> {
    if matches!(args.first().map(String::as_str), Some("login")) && args.len() != 2 {
        return Err("tau provider login requires exactly one NAME".into());
    }
    let (extension_instance, args) = provider_cli_target(args)?;
    let network = Arc::new(tau_provider::OutboundNetworkPolicy::from_env());
    match args.first().map(String::as_str).unwrap_or("help") {
        "add" => cmd_add(&args[1..], &network, &extension_instance)?,
        "login" => cmd_login(&args[1..], &network, &extension_instance)?,
        "remove" | "delete" => cmd_remove(&args[1..], &extension_instance)?,
        "rename" => cmd_rename(&args[1..], &extension_instance)?,
        "list" | "status" => cmd_list(&args[1..], &extension_instance)?,
        "show" => cmd_show(&args[1..], &extension_instance)?,
        "help" | "--help" | "-h" => println!("{PROVIDER_CLI_HELP}"),
        other => return Err(format!("unknown provider subcommand: {other}").into()),
    }
    Ok(())
}

fn provider_cli_target(
    args: &[String],
) -> Result<(tau_proto::ExtensionName, Vec<String>), Box<dyn Error>> {
    let mut remaining = args.to_vec();
    let requested = if remaining.first().map(String::as_str) == Some("--extension") {
        if remaining.len() < 2 {
            return Err("--extension requires an exact configured instance name".into());
        }
        let value = remaining.remove(1);
        remaining.remove(0);
        Some(value)
    } else {
        None
    };
    let settings = tau_config::settings::load_harness_settings()?;
    let mut candidates = settings
        .extensions
        .iter()
        .filter_map(|(name, entry)| provider_cli_entry_is_builtin(name, entry).then_some(name))
        .cloned()
        .collect::<Vec<_>>();
    if !settings.extensions.contains_key("provider-builtin") {
        candidates.push("provider-builtin".to_owned());
    }
    candidates.sort();
    candidates.dedup();
    let extension = match requested {
        Some(extension) if candidates.iter().any(|candidate| candidate == &extension) => extension,
        Some(extension) => {
            return Err(format!(
                "extension instance '{extension}' is missing, disabled, or is not the built-in provider component with role 'provider'"
            )
            .into());
        }
        None if candidates
            .iter()
            .any(|candidate| candidate == "provider-builtin") =>
        {
            "provider-builtin".to_owned()
        }
        None => return Err("the default provider-builtin extension is disabled".into()),
    };
    let extension = tau_proto::ExtensionName::parse(extension)
        .map_err(|error| format!("invalid provider extension instance: {error}"))?;
    Ok((extension, remaining))
}

fn provider_cli_entry_is_builtin(name: &str, entry: &tau_config::settings::ExtensionEntry) -> bool {
    if entry.enable == Some(false) || entry.role.as_deref().is_some_and(|role| role != "provider") {
        return false;
    }
    let component = entry
        .command
        .is_none()
        .then(|| {
            entry
                .suffix
                .as_deref()
                .and_then(BuiltinComponentIdentity::from_tau_owned_suffix)
        })
        .flatten();
    if name == "provider-builtin" && entry.suffix.is_none() {
        return entry.command.is_none();
    }
    component == Some(BuiltinComponentIdentity::Provider)
        && (name == "provider-builtin" || entry.role.as_deref() == Some("provider"))
}

const PROVIDER_CLI_HELP: &str = "\
Usage: tau provider [--extension INSTANCE] <subcommand>

Subcommands:
  add [--state|--config [--output -]] [KIND]
                                 Add or replace a provider profile (default: state)
  login <name>                   Authenticate an existing provider profile
  remove [--state|--config] <name>
                                  Remove a provider profile
  rename <old> <new>             Rename a provider profile without changing credentials
  list [--state|--config|--all]  List provider profiles with their source
  show <name>                    Show a credential-free profile and source path

Provider kinds:
  chatgpt           ChatGPT / Codex
  chat-completions  OpenAI-compatible Chat Completions
  responses         OpenAI Responses API
  openrouter        OpenRouter";

/// One canonical provider-kind choice accepted by the setup command.
///
/// The token is the only non-interactive spelling.  The label is deliberately
/// human-oriented because the same table drives the interactive picker.
struct ProviderKindDescriptor {
    /// Canonical machine token accepted after `tau provider add`.
    token: &'static str,
    /// Human-readable picker label.
    label: &'static str,
}

/// The complete, canonical provider-kind catalog.
const PROVIDER_KINDS: [ProviderKindDescriptor; 4] = [
    ProviderKindDescriptor {
        token: "chatgpt",
        label: "ChatGPT / Codex",
    },
    ProviderKindDescriptor {
        token: "chat-completions",
        label: "OpenAI-compatible Chat Completions",
    },
    ProviderKindDescriptor {
        token: "responses",
        label: "OpenAI Responses API",
    },
    ProviderKindDescriptor {
        token: "openrouter",
        label: "OpenRouter",
    },
];

fn cmd_add(
    args: &[String],
    network: &tau_provider::OutboundNetworkPolicy,
    extension_instance: &tau_proto::ExtensionName,
) -> Result<(), Box<dyn Error>> {
    let mut requested_target = None;
    let mut stdout = false;
    let mut kind_args = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--state" => {
                if requested_target
                    .replace(setup_store::ProfileTarget::State)
                    .is_some()
                {
                    return Err("provider add accepts exactly one source flag".into());
                }
            }
            "--config" => {
                if requested_target
                    .replace(setup_store::ProfileTarget::Config)
                    .is_some()
                {
                    return Err("provider add accepts exactly one source flag".into());
                }
            }
            "--output" if args.get(index + 1).map(String::as_str) == Some("-") => {
                if stdout {
                    return Err("provider add accepts --output - only once".into());
                }
                stdout = true;
                index += 1;
            }
            argument if argument.starts_with('-') => {
                return Err(format!("unknown provider add option '{argument}'").into());
            }
            _ => kind_args.push(args[index].clone()),
        }
        index += 1;
    }
    let implicit_state_target = requested_target.is_none() && !stdout;
    let target = match (requested_target, stdout) {
        (Some(setup_store::ProfileTarget::Config), true) => setup_store::ProfileTarget::Stdout,
        (None, false) | (Some(setup_store::ProfileTarget::State), false) => {
            setup_store::ProfileTarget::State
        }
        (Some(setup_store::ProfileTarget::Config), false) => setup_store::ProfileTarget::Config,
        (_, true) => return Err("--output - requires --config".into()),
        (Some(setup_store::ProfileTarget::Stdout), _) => unreachable!(),
    };
    let kind = match kind_args.as_slice() {
        [] => {
            let labels = PROVIDER_KINDS
                .iter()
                .map(|kind| kind.label)
                .collect::<Vec<_>>();
            PROVIDER_KINDS[Select::new()
                .with_prompt("Provider kind")
                .items(&labels)
                .default(0)
                .interact()?]
            .token
        }
        [kind] if PROVIDER_KINDS.iter().any(|known| known.token == kind) => kind,
        [kind] => {
            let valid = PROVIDER_KINDS
                .iter()
                .map(|known| known.token)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!("unknown provider kind '{kind}'; valid kinds: {valid}").into());
        }
        _ => return Err("tau provider add accepts at most one KIND".into()),
    };
    match kind {
        "chatgpt" => cmd_add_chatgpt(network, extension_instance, target, implicit_state_target)?,
        "chat-completions" => cmd_add_chat_completions(extension_instance, target)?,
        "responses" => cmd_add_responses(extension_instance, target)?,
        "openrouter" => cmd_add_openrouter(network, extension_instance, target)?,
        _ => unreachable!("provider kind came from the closed descriptor table"),
    }
    Ok(())
}

fn cmd_add_responses(
    extension_instance: &tau_proto::ExtensionName,
    target: setup_store::ProfileTarget,
) -> Result<(), Box<dyn Error>> {
    let name = prompt_provider_name("responses")?;
    let base_url: String = Input::new()
        .with_prompt("Base URL")
        .default("https://api.openai.com/v1".to_owned())
        .interact_text()?;
    let api_key_source = prompt_api_key(extension_instance, true)?;
    let api_key = api_key_source.value();
    let models_input: String = Input::new()
        .with_prompt("Models (comma-separated)")
        .interact_text()?;
    let models = parse_responses_model_list(&models_input)?;
    let transport_options = match recommended_responses_transport(&base_url) {
        ResponsesTransport::Sse => ["sse", "websocket"],
        ResponsesTransport::Websocket => ["websocket", "sse"],
    };
    let transport = Select::new()
        .with_prompt("Transport")
        .items(&transport_options)
        .default(0)
        .interact()?;
    let transport = match transport_options[transport] {
        "sse" => ResponsesTransport::Sse,
        "websocket" => ResponsesTransport::Websocket,
        _ => unreachable!("transport came from the closed picker choices"),
    };
    save_profile(
        extension_instance,
        &name,
        &BuiltinProviderProfile::Responses(ResponsesProvider {
            base_url,
            api_key,
            models,
            tags: Vec::new(),
            max_output_tokens: 8192,
            transport,
            compat: responses::ResponsesCompat::default(),
        }),
        ProviderSetupInput::ApiKey(api_key_source),
        target,
    )?;
    Ok(())
}

fn recommended_responses_transport(base_url: &str) -> ResponsesTransport {
    if base_url.trim_end_matches('/') == "https://api.openai.com/v1" {
        ResponsesTransport::Websocket
    } else {
        ResponsesTransport::Sse
    }
}

fn cmd_add_chatgpt(
    network: &tau_provider::OutboundNetworkPolicy,
    extension_instance: &tau_proto::ExtensionName,
    target: setup_store::ProfileTarget,
    offer_existing_login: bool,
) -> Result<(), Box<dyn Error>> {
    use std::io::IsTerminal as _;

    let store = setup_store::SetupStore::open_default()?;
    cmd_add_chatgpt_in(
        network,
        extension_instance,
        target,
        offer_existing_login,
        &store,
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal(),
    )
}

fn cmd_add_chatgpt_in(
    network: &tau_provider::OutboundNetworkPolicy,
    extension_instance: &tau_proto::ExtensionName,
    target: setup_store::ProfileTarget,
    offer_existing_login: bool,
    store: &setup_store::SetupStore,
    interactive: bool,
) -> Result<(), Box<dyn Error>> {
    if offer_existing_login && !interactive {
        let default_name = ProviderName::new(CHATGPT_PROVIDER_NAME);
        if offer_login_for_existing_config_profile_in(
            network,
            extension_instance,
            &default_name,
            store,
            false,
        )? {
            return Ok(());
        }
    }
    let name = prompt_provider_name("chatgpt")?;
    if offer_existing_login
        && offer_login_for_existing_config_profile_in(
            network,
            extension_instance,
            &name,
            store,
            true,
        )?
    {
        return Ok(());
    }
    let auth = run_openai_codex_login(network)?;
    let responses_lite_compatibility = Confirm::new()
        .with_prompt("Use legacy Responses Lite compatibility for GPT-5.6?")
        .default(DEFAULT_RESPONSES_LITE_COMPATIBILITY)
        .interact()?;
    save_profile(
        extension_instance,
        &name,
        &BuiltinProviderProfile::Chatgpt(ChatGptProfile {
            auth,
            responses_lite_compatibility,
        }),
        ProviderSetupInput::ProfileOAuth,
        target,
    )?;
    Ok(())
}

fn offer_login_for_existing_config_profile_in(
    network: &tau_provider::OutboundNetworkPolicy,
    extension_instance: &tau_proto::ExtensionName,
    name: &ProviderName,
    store: &setup_store::SetupStore,
    interactive: bool,
) -> Result<bool, Box<dyn Error>> {
    let snapshot = store.snapshot(extension_instance)?;
    let matching = snapshot
        .profiles
        .iter()
        .filter(|profile| profile.provider == *name)
        .collect::<Vec<_>>();
    let profile = match matching.as_slice() {
        [] => return Ok(false),
        [profile] => *profile,
        _ => {
            return Err(
                format!("provider profile '{name}' is duplicated across config and state").into(),
            );
        }
    };
    if profile.source != setup_store::ProfileSource::Config {
        return Ok(false);
    }
    let (parsed, credential) =
        parse_settings_profile(name, &profile.contents).map_err(|reason| {
            format!(
                "provider profile '{name}' is invalid: {reason} (source=config, path={})",
                profile.path.display()
            )
        })?;
    if !matches!(parsed, BuiltinProviderProfile::Chatgpt(_)) {
        return Err(format!("provider '{name}' already exists in config").into());
    }
    let ProviderCredential::Stored(reference) = credential else {
        return Err(format!("provider '{name}' is keyless and cannot use ChatGPT").into());
    };
    let needs_login = !snapshot
        .credentials
        .get(&(reference.identity().clone(), ProviderCredentialSlot::OAuth))
        .and_then(|bytes| {
            serde_json::from_slice::<credential_record::ChatGptOAuthCredential>(bytes).ok()
        })
        .is_some_and(|record| record.is_unexpired(now_ms()));
    if !needs_login {
        return Err(format!(
            "provider '{name}' already exists in config and is authenticated; use `{}` to refresh its credentials",
            provider_login_command(extension_instance, name)
        )
        .into());
    }
    if !interactive {
        return Err(format!(
            "provider '{name}' already exists in config and needs authentication; run `{}`",
            provider_login_command(extension_instance, name)
        )
        .into());
    }
    let login = Confirm::new()
        .with_prompt(format!(
            "Provider '{name}' already exists in config and needs authentication. Log in now?"
        ))
        .default(true)
        .interact()?;
    if !login {
        return Err(format!(
            "provider authentication cancelled; run `{}` when ready",
            provider_login_command(extension_instance, name)
        )
        .into());
    }
    login_profile(network, extension_instance, store, name)?;
    Ok(true)
}

fn provider_login_command(
    extension_instance: &tau_proto::ExtensionName,
    name: &ProviderName,
) -> String {
    if extension_instance.as_str() == "provider-builtin" {
        format!("tau provider login {name}")
    } else {
        format!("tau provider --extension {extension_instance} login {name}")
    }
}

fn cmd_login(
    args: &[String],
    network: &tau_provider::OutboundNetworkPolicy,
    extension_instance: &tau_proto::ExtensionName,
) -> Result<(), Box<dyn Error>> {
    let [name] = args else {
        return Err("tau provider login requires exactly one NAME".into());
    };
    let name = ProviderName::try_new(name.clone())
        .map_err(|error| format!("invalid provider namespace '{name}': {error}"))?;
    let store = setup_store::SetupStore::open_default()?;
    login_profile(network, extension_instance, &store, &name)
}

/// Authenticates one existing profile and publishes only its host-local Secret
/// record; the exact source and settings bytes must remain unchanged.
fn login_profile(
    network: &tau_provider::OutboundNetworkPolicy,
    extension_instance: &tau_proto::ExtensionName,
    store: &setup_store::SetupStore,
    name: &ProviderName,
) -> Result<(), Box<dyn Error>> {
    use credential_record::{ApiKeyCredential, ChatGptOAuthCredential};

    let snapshot = store.snapshot(extension_instance)?;
    let matching = snapshot
        .profiles
        .into_iter()
        .filter(|profile| profile.provider == *name)
        .collect::<Vec<_>>();
    let profile = match matching.as_slice() {
        [] => return Err(format!("provider profile '{name}' is not configured").into()),
        [profile] => profile,
        _ => {
            return Err(
                format!("provider profile '{name}' is duplicated across config and state").into(),
            );
        }
    };
    let (parsed, credential) =
        parse_settings_profile(name, &profile.contents).map_err(|reason| {
            format!(
                "provider profile '{name}' is invalid: {reason} (source={}, path={})",
                profile.source.label(),
                profile.path.display()
            )
        })?;
    let ProviderCredential::Stored(reference) = credential else {
        return Err(
            format!("provider profile '{name}' is keyless and does not require login").into(),
        );
    };
    let (secret, named_source) = match parsed {
        BuiltinProviderProfile::Chatgpt(_) => {
            let auth = run_openai_codex_login(network)?;
            (
                setup_store::SecretWrite {
                    path: reference.path().clone(),
                    contents: setup_store::SecretBytes::new(serde_json::to_vec(
                        &ChatGptOAuthCredential::from(auth),
                    )?),
                },
                None,
            )
        }
        BuiltinProviderProfile::ChatCompletions(_)
        | BuiltinProviderProfile::OpenRouter(_)
        | BuiltinProviderProfile::Responses(_) => {
            let named_source = reference
                .named_source()
                .map(|source_name| configured_named_secret(extension_instance, source_name))
                .transpose()?;
            let value = if named_source.is_some() {
                String::new()
            } else {
                Password::new().with_prompt("API key").interact()?
            };
            (
                setup_store::SecretWrite {
                    path: reference.path().clone(),
                    contents: setup_store::SecretBytes::new(serde_json::to_vec(
                        &ApiKeyCredential::new(value),
                    )?),
                },
                named_source,
            )
        }
    };
    store.publish_credential(
        extension_instance,
        name,
        profile.source,
        &profile.contents,
        &secret,
        named_source.as_ref(),
    )?;
    eprintln!(
        "Authenticated provider '{name}' for extension '{extension_instance}'; settings were unchanged."
    );
    Ok(())
}

fn configured_named_secret(
    extension_instance: &tau_proto::ExtensionName,
    source_name: &str,
) -> Result<setup_store::NamedSecretSource, Box<dyn Error>> {
    let Some((name, declaration)) = configured_secrets(extension_instance)?
        .into_iter()
        .find(|(name, _)| name == source_name)
    else {
        return Err(format!("configured named secret '{source_name}' is not declared").into());
    };
    Ok(setup_store::NamedSecretSource { name, declaration })
}

fn cmd_add_chat_completions(
    extension_instance: &tau_proto::ExtensionName,
    target: setup_store::ProfileTarget,
) -> Result<(), Box<dyn Error>> {
    let name = prompt_provider_name("local")?;
    let base_url: String = Input::new()
        .with_prompt("Base URL")
        .default("https://api.openai.com/v1".to_owned())
        .interact_text()?;
    let api_key_source = prompt_api_key(extension_instance, true)?;
    let api_key = api_key_source.value();
    let models_input: String = Input::new()
        .with_prompt("Models (comma-separated)")
        .default("gpt-4o,gpt-4o-mini".to_owned())
        .interact_text()?;
    let models = parse_chat_model_list(&models_input)?;
    let profile = ChatCompletionsProvider {
        base_url,
        api_key,
        models,
        max_output_tokens: tau_provider_chat_completions::DEFAULT_MAX_OUTPUT_TOKENS,
        extra_body: BTreeMap::new(),
        tags: Vec::new(),
        compat: chat_completions_add_compat(),
    };
    save_profile(
        extension_instance,
        &name,
        &BuiltinProviderProfile::ChatCompletions(profile),
        ProviderSetupInput::ApiKey(api_key_source),
        target,
    )?;
    Ok(())
}

fn chat_completions_add_compat() -> ChatCompletionsCompat {
    ChatCompletionsCompat {
        max_completion_tokens: false,
        ..ChatCompletionsCompat::openai_defaults()
    }
}

fn cmd_add_openrouter(
    network: &tau_provider::OutboundNetworkPolicy,
    extension_instance: &tau_proto::ExtensionName,
    target: setup_store::ProfileTarget,
) -> Result<(), Box<dyn Error>> {
    let name = prompt_provider_name("openrouter")?;
    let api_key_source = prompt_api_key(extension_instance, false)?;
    let api_key = api_key_source.value();
    let models_input: String = Input::new()
        .with_prompt("Models (comma-separated, or press enter to fetch from OpenRouter)")
        .allow_empty(true)
        .interact_text()?;
    let models = if models_input.trim().is_empty() {
        eprintln!("Fetching models from OpenRouter...");
        fetch_openrouter_models(&api_key, network)?
    } else {
        parse_chat_model_list(&models_input)?
    };
    let profile = OpenRouterProfile { api_key, models };
    save_profile(
        extension_instance,
        &name,
        &BuiltinProviderProfile::OpenRouter(profile),
        ProviderSetupInput::ApiKey(api_key_source),
        target,
    )?;
    Ok(())
}

fn cmd_remove(
    args: &[String],
    extension_instance: &tau_proto::ExtensionName,
) -> Result<(), Box<dyn Error>> {
    let mut source = None;
    let mut name_arg = None;
    for argument in args {
        match argument.as_str() {
            "--config" => {
                if source.replace(setup_store::ProfileSource::Config).is_some() {
                    return Err("provider remove accepts exactly one source flag".into());
                }
            }
            "--state" => {
                if source.replace(setup_store::ProfileSource::State).is_some() {
                    return Err("provider remove accepts exactly one source flag".into());
                }
            }
            value if value.starts_with('-') => {
                return Err(format!("unknown provider remove option '{value}'").into());
            }
            value if name_arg.is_none() => name_arg = Some(value),
            _ => return Err("tau provider remove accepts one NAME".into()),
        }
    }
    let name = match name_arg {
        Some(name) => ProviderName::try_new(name.trim().to_owned())
            .map_err(|error| format!("invalid provider namespace '{name}': {error}"))?,
        None => prompt_provider_name(CHATGPT_PROVIDER_NAME)?,
    };
    if setup_store::SetupStore::open_default()?.remove_from(extension_instance, &name, source)? {
        eprintln!("Removed provider profile '{name}'.");
    } else {
        eprintln!("Provider profile '{name}' was not configured.");
    }
    Ok(())
}

/// Renames one profile file while retaining its opaque credential identity.
fn cmd_rename(
    args: &[String],
    extension_instance: &tau_proto::ExtensionName,
) -> Result<(), Box<dyn Error>> {
    let [old, new] = args else {
        return Err("tau provider rename requires OLD and NEW".into());
    };
    let old = ProviderName::try_new(old.clone())
        .map_err(|error| format!("invalid old provider namespace '{old}': {error}"))?;
    let new = ProviderName::try_new(new.clone())
        .map_err(|error| format!("invalid new provider namespace '{new}': {error}"))?;
    let source =
        setup_store::SetupStore::open_default()?.rename_profile(extension_instance, &old, &new)?;
    eprintln!(
        "Renamed {source_label} provider profile '{old}' to '{new}'; credentials were unchanged.",
        source_label = source.label()
    );
    Ok(())
}

fn cmd_list(
    args: &[String],
    extension_instance: &tau_proto::ExtensionName,
) -> Result<(), Box<dyn Error>> {
    let store = setup_store::SetupStore::open_default()?;
    let stdout = std::io::stdout();
    cmd_list_from_store(args, extension_instance, &store, &mut stdout.lock())
}

fn cmd_list_from_store(
    args: &[String],
    extension_instance: &tau_proto::ExtensionName,
    store: &setup_store::SetupStore,
    output: &mut impl Write,
) -> Result<(), Box<dyn Error>> {
    let source_filter = match args {
        [] => None,
        [flag] if flag == "--all" => None,
        [flag] if flag == "--config" => Some(setup_store::ProfileSource::Config),
        [flag] if flag == "--state" => Some(setup_store::ProfileSource::State),
        _ => return Err("tau provider list accepts one of --all, --config, or --state".into()),
    };
    let setup_store::SetupSnapshot {
        profiles,
        credentials,
    } = store.snapshot(extension_instance)?;
    let mut displayed = 0_usize;
    for profile in profiles
        .into_iter()
        .filter(|profile| source_filter.is_none_or(|wanted| wanted == profile.source))
    {
        let (parsed, credential) = parse_settings_profile(&profile.provider, &profile.contents)
            .map_err(|reason| {
                format!(
                    "provider profile '{}' is invalid: {reason} (source={}, path={})",
                    profile.provider,
                    profile.source.label(),
                    profile.path.display()
                )
            })?;
        displayed += 1;
        let name = &profile.provider;
        let source = profile.source.label();
        match parsed {
            BuiltinProviderProfile::Chatgpt(parsed) => {
                let ProviderCredential::Stored(reference) = &credential else {
                    return Err("ChatGPT profile has no stored credential".into());
                };
                let status = match credentials
                    .get(&(reference.identity().clone(), ProviderCredentialSlot::OAuth))
                    .and_then(|bytes| {
                        serde_json::from_slice::<credential_record::ChatGptOAuthCredential>(bytes)
                            .ok()
                    }) {
                    Some(record) if record.is_unexpired(now_ms()) => "logged-in",
                    Some(_) => "expired",
                    _ => "not-configured",
                };
                let remediation = match status {
                    "expired" | "not-configured" => {
                        format!(
                            "\tlogin: {}",
                            provider_login_command(extension_instance, name)
                        )
                    }
                    _ => String::new(),
                };
                let mode = if parsed.responses_lite_compatibility {
                    "responses-lite-compatibility"
                } else {
                    "responses-standard"
                };
                writeln!(
                    output,
                    "{extension_instance}\t{name}\tchatgpt\t{status}\t{mode}\t{source}{remediation}"
                )?;
            }
            BuiltinProviderProfile::ChatCompletions(parsed) => {
                let auth_status = setup_api_key_status(&credentials, &credential);
                let models = parsed
                    .models
                    .iter()
                    .map(|model| model.id.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                writeln!(
                    output,
                    "{extension_instance}\t{name}\tchat_completions\t{}\t{models}\t{auth_status}\t{source}",
                    parsed.base_url
                )?;
            }
            BuiltinProviderProfile::OpenRouter(parsed) => {
                let auth_status = setup_api_key_status(&credentials, &credential);
                let models = parsed
                    .models
                    .iter()
                    .map(|model| model.id.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                writeln!(
                    output,
                    "{extension_instance}\t{name}\topenrouter\thttps://openrouter.ai/api/v1\t{models}\t{auth_status}\t{source}"
                )?;
            }
            BuiltinProviderProfile::Responses(parsed) => {
                let auth_status = setup_api_key_status(&credentials, &credential);
                let models = parsed
                    .models
                    .iter()
                    .map(|model| model.id.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                writeln!(
                    output,
                    "{extension_instance}\t{name}\tresponses\t{}\t{models}\t{auth_status}\t{source}",
                    parsed.base_url
                )?;
            }
        }
    }
    if displayed == 0 {
        writeln!(output, "No providers configured.")?;
    }
    Ok(())
}

fn cmd_show(
    args: &[String],
    extension_instance: &tau_proto::ExtensionName,
) -> Result<(), Box<dyn Error>> {
    let store = setup_store::SetupStore::open_default()?;
    let stdout = std::io::stdout();
    cmd_show_from_store(args, extension_instance, &store, &mut stdout.lock())
}

fn cmd_show_from_store(
    args: &[String],
    extension_instance: &tau_proto::ExtensionName,
    store: &setup_store::SetupStore,
    output: &mut impl Write,
) -> Result<(), Box<dyn Error>> {
    let [name] = args else {
        return Err("tau provider show requires exactly one NAME".into());
    };
    let name = ProviderName::try_new(name.clone())
        .map_err(|error| format!("invalid provider namespace '{name}': {error}"))?;
    let snapshot = store.snapshot(extension_instance)?;
    let Some(profile) = snapshot
        .profiles
        .into_iter()
        .find(|profile| profile.provider == name)
    else {
        return Err(format!("provider profile '{name}' is not configured").into());
    };
    let value: serde_json::Value = serde_json::from_slice(&profile.contents)?;
    writeln!(
        output,
        "source: {}\npath: {}\n{}",
        profile.source.label(),
        profile.path.display(),
        serde_json::to_string_pretty(&value)?
    )?;
    Ok(())
}

fn setup_api_key_status(
    credentials: &std::collections::BTreeMap<
        (ProviderCredentialIdentity, ProviderCredentialSlot),
        Vec<u8>,
    >,
    credential: &ProviderCredential,
) -> &'static str {
    let ProviderCredential::Stored(reference) = credential else {
        return "no-api-key";
    };
    match credentials
        .get(&(reference.identity().clone(), ProviderCredentialSlot::ApiKey))
        .and_then(|bytes| serde_json::from_slice::<credential_record::ApiKeyCredential>(bytes).ok())
    {
        Some(record) if record.has_value() => "api-key",
        _ => "no-api-key",
    }
}

fn prompt_provider_name(default: &str) -> Result<ProviderName, Box<dyn Error>> {
    let name: String = Input::new()
        .with_prompt("Provider namespace")
        .default(default.to_owned())
        .interact_text()?;
    ProviderName::try_new(name.trim().to_owned())
        .map_err(|error| format!("invalid provider namespace '{name}': {error}").into())
}

fn parse_chat_model_list(input: &str) -> Result<Vec<ChatCompletionsModel>, Box<dyn Error>> {
    let mut models = Vec::new();
    for raw in input.split(',') {
        let model = raw.trim();
        if model.is_empty() {
            continue;
        }
        models.push(ChatCompletionsModel {
            id: ModelName::try_new(model.to_owned())?,
            display_name: None,
            context_window: tau_proto::TokenCount::new(128_000),
            max_input_tokens: None,
            max_output_tokens: None,
            compat: None,
            tags: Vec::new(),
            hosted_tool_capabilities: Vec::new(),
            supported_tool_types: vec![tau_proto::ToolType::Function],
            input_modalities: Vec::new(),
            tool_result_modalities: Vec::new(),
            supports_parallel_tool_calls: true,
            local_summary_compaction: None,
            cache_contract: None,
            est_uncached_input_cost_1m_usd: None,
            est_cached_input_cost_1m_usd: None,
            est_cache_write_input_cost_1m_usd: None,
            est_output_cost_1m_usd: None,
            est_cache_storage_cost_1m_token_hour_usd: None,
        });
    }
    if models.is_empty() {
        return Err("at least one model is required".into());
    }
    Ok(models)
}

fn parse_responses_model_list(input: &str) -> Result<Vec<ResponsesModel>, Box<dyn Error>> {
    let models = input
        .split(',')
        .filter_map(|raw| {
            let id = raw.trim();
            (!id.is_empty()).then(|| {
                ModelName::try_new(id.to_owned()).map(|id| ResponsesModel {
                    id,
                    reasoning_effort: None,
                    compat: None,
                    display_name: None,
                    context_window: tau_proto::TokenCount::new(128_000),
                    max_input_tokens: None,
                    max_output_tokens: None,
                    tags: Vec::new(),
                    supports_parallel_tool_calls: true,
                    local_summary_compaction: None,
                    cache_contract: None,
                    est_uncached_input_cost_1m_usd: None,
                    est_cached_input_cost_1m_usd: None,
                    est_cache_write_input_cost_1m_usd: None,
                    est_output_cost_1m_usd: None,
                    est_cache_storage_cost_1m_token_hour_usd: None,
                })
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if models.is_empty() {
        return Err("at least one model is required".into());
    }
    Ok(models)
}

/// Identifies whether an API-key record is direct user input, an intentionally
/// empty keyless profile, or a restart-refreshable configured named secret.
enum ApiKeySource {
    /// A masked terminal prompt supplied the value.
    Direct(SecretValue),
    /// The profile is genuinely keyless.
    Keyless,
    /// Trusted setup materialized this configured secret name.
    ConfiguredNamed {
        /// Exact configured source name serialized into provider settings.
        name: String,
        /// Declaration captured from the targeted extension configuration.
        declaration: tau_config::settings::ExtensionSecretEntry,
    },
    /// The profile explicitly defers binding this source until persistent
    /// startup.
    DeferredNamed {
        /// Exact source name serialized into provider settings.
        name: String,
    },
}

/// Exhaustive credential input accepted by provider profile setup.
enum ProviderSetupInput {
    /// Serialize the OAuth credential already present in a ChatGPT profile.
    ProfileOAuth,
    /// Apply one explicit API-key authority selection to an API-key profile.
    ApiKey(ApiKeySource),
}

impl ApiKeySource {
    /// Return the setup-time value or the empty materialization placeholder.
    fn value(&self) -> String {
        match self {
            Self::Direct(value) => value.expose_secret().to_owned(),
            Self::Keyless | Self::ConfiguredNamed { .. } | Self::DeferredNamed { .. } => {
                String::new()
            }
        }
    }
}

/// Prompt for an API-key authority without ever echoing a secret value.
fn prompt_api_key(
    extension_instance: &tau_proto::ExtensionName,
    allow_keyless: bool,
) -> Result<ApiKeySource, Box<dyn Error>> {
    let named_secrets = configured_secrets(extension_instance)?;
    let mut choices = vec!["Enter API key now"];
    choices.push("Use named secret");
    if allow_keyless {
        choices.push("No API key");
    }
    let selected = choices[Select::new()
        .with_prompt("API key source")
        .items(&choices)
        .default(0)
        .interact()?];
    match selected {
        "Enter API key now" => Ok(ApiKeySource::Direct(SecretValue::new(
            Password::new().with_prompt("API key").interact()?,
        ))),
        "Use named secret" => {
            const ENTER_DEFERRED: &str = "Enter secret name for deferred binding…";
            if named_secrets.is_empty() {
                return prompt_deferred_secret_name();
            }
            let mut names = named_secrets
                .iter()
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>();
            names.push(ENTER_DEFERRED.to_owned());
            let index = Select::new()
                .with_prompt("Configured named secret")
                .items(&names)
                .default(0)
                .interact()?;
            if index == named_secrets.len() {
                prompt_deferred_secret_name()
            } else {
                let (name, declaration) = named_secrets[index].clone();
                Ok(ApiKeySource::ConfiguredNamed { name, declaration })
            }
        }
        "No API key" => Ok(ApiKeySource::Keyless),
        _ => unreachable!("API-key source came from the offered picker choices"),
    }
}

/// Prompt for one valid source name to bind only at persistent startup.
fn prompt_deferred_secret_name() -> Result<ApiKeySource, Box<dyn Error>> {
    let name = Input::<String>::new()
        .with_prompt("Secret name for deferred binding")
        .validate_with(|name: &String| validate_deferred_secret_name(name))
        .interact_text()?;
    Ok(ApiKeySource::DeferredNamed { name })
}

/// Validate the opaque source name retained by an explicit deferred binding.
fn validate_deferred_secret_name(name: &str) -> Result<(), String> {
    tau_config::secret_sources::validate_secret_name(name)
        .map_err(|_| "use letters, digits, '.', '_', or '-' (but not '.' or '..')".to_owned())?;
    Ok(())
}

/// Return secret declarations for the exact targeted provider instance.
fn configured_secrets(
    extension_instance: &tau_proto::ExtensionName,
) -> Result<Vec<(String, tau_config::settings::ExtensionSecretEntry)>, Box<dyn Error>> {
    let settings = tau_config::settings::load_harness_settings()?;
    let mut declarations = settings
        .extensions
        .get(extension_instance.as_str())
        .and_then(|entry| entry.secrets.as_ref())
        .map(|secrets| {
            secrets
                .iter()
                .map(|(name, declaration)| (name.clone(), declaration.clone()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    declarations.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(declarations)
}

fn save_profile(
    extension_instance: &tau_proto::ExtensionName,
    name: &ProviderName,
    profile: &BuiltinProviderProfile,
    credential_input: ProviderSetupInput,
    target: setup_store::ProfileTarget,
) -> Result<(), Box<dyn Error>> {
    let ProviderSetupPayload {
        settings,
        credential,
    } = provider_setup_payload(name, profile, credential_input)?;
    let publication = match &credential {
        setup_store::CredentialSetup::Stored { .. } => CredentialPublication::Published,
        setup_store::CredentialSetup::DeferredNamed { .. } => CredentialPublication::Deferred,
        setup_store::CredentialSetup::Keyless => CredentialPublication::Keyless,
    };
    let settings_output = settings.clone();
    let settings_path = setup_store::SetupStore::open_default()?.apply_to(
        &setup_store::ProviderSetupPlan {
            extension_instance: extension_instance.clone(),
            provider: name.clone(),
            settings,
            credential,
        },
        target,
    )?;
    if target == setup_store::ProfileTarget::Stdout {
        write_dotfiles_profile(
            &settings_output,
            publication,
            &mut std::io::stdout().lock(),
            &mut std::io::stderr().lock(),
        )?;
    } else if let Some(settings_path) = settings_path {
        eprintln!(
            "Provider '{name}' registered for extension '{extension_instance}' in {}. Settings: {}",
            match target {
                setup_store::ProfileTarget::State => "state",
                setup_store::ProfileTarget::Config => "config",
                setup_store::ProfileTarget::Stdout => unreachable!(),
            },
            settings_path.display()
        );
    }
    eprintln!("Restart Tau for settings changes to take effect.");
    Ok(())
}

fn write_dotfiles_profile(
    settings: &[u8],
    publication: CredentialPublication,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> path_std_io::Result<()> {
    stdout.write_all(settings)?;
    stdout.write_all(b"\n")?;
    match publication {
        CredentialPublication::Published => writeln!(
            stderr,
            "Published the host-local credential; deploy this config profile before restart."
        )?,
        CredentialPublication::Deferred => writeln!(
            stderr,
            "No host-local credential was written; declare and provide the named secret before restarting Tau."
        )?,
        CredentialPublication::Keyless => writeln!(
            stderr,
            "This keyless profile needs no host-local credential; deploy it before restart."
        )?,
    }
    Ok(())
}

/// User-visible credential publication effect of provider setup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CredentialPublication {
    /// Setup published a complete host-local typed record.
    Published,
    /// Setup wrote only an explicitly deferred named-source binding.
    Deferred,
    /// The profile explicitly needs no credential.
    Keyless,
}

/// Credential-free settings and an exhaustive credential publication plan.
struct ProviderSetupPayload {
    /// Credential-free provider settings.
    settings: Vec<u8>,
    /// Exhaustive keyless or stored credential publication selection.
    credential: setup_store::CredentialSetup,
}

fn provider_setup_payload(
    _name: &ProviderName,
    profile: &BuiltinProviderProfile,
    credential_input: ProviderSetupInput,
) -> Result<ProviderSetupPayload, Box<dyn Error>> {
    use credential_record::ChatGptOAuthCredential;

    let mut settings = serde_json::to_value(profile)?;
    let object = settings
        .as_object_mut()
        .ok_or("provider settings must serialize as an object")?;
    let api_key_source = match (profile, credential_input) {
        (BuiltinProviderProfile::Chatgpt(_), ProviderSetupInput::ProfileOAuth) => None,
        (
            BuiltinProviderProfile::ChatCompletions(_)
            | BuiltinProviderProfile::OpenRouter(_)
            | BuiltinProviderProfile::Responses(_),
            ProviderSetupInput::ApiKey(source),
        ) => Some(source),
        (BuiltinProviderProfile::Chatgpt(_), ProviderSetupInput::ApiKey(_)) => {
            return Err("ChatGPT setup requires profile OAuth credentials".into());
        }
        (
            BuiltinProviderProfile::ChatCompletions(_)
            | BuiltinProviderProfile::OpenRouter(_)
            | BuiltinProviderProfile::Responses(_),
            ProviderSetupInput::ProfileOAuth,
        ) => return Err("API-key provider setup requires an API-key authority".into()),
    };
    let (slot, secret) = match profile {
        BuiltinProviderProfile::Chatgpt(profile) => {
            object.remove("auth");
            (
                ProviderCredentialSlot::OAuth,
                Some(serde_json::to_vec(&ChatGptOAuthCredential::from(
                    profile.auth.clone(),
                ))?),
            )
        }
        BuiltinProviderProfile::ChatCompletions(profile) => {
            object.remove("api_key");
            object.remove("api_key_secret");
            (
                ProviderCredentialSlot::ApiKey,
                api_key_record(profile.api_key.clone(), api_key_source.as_ref())?,
            )
        }
        BuiltinProviderProfile::OpenRouter(profile) => {
            object.remove("api_key");
            object.remove("api_key_secret");
            (
                ProviderCredentialSlot::ApiKey,
                api_key_record(profile.api_key.clone(), api_key_source.as_ref())?,
            )
        }
        BuiltinProviderProfile::Responses(profile) => {
            object.remove("api_key");
            object.remove("api_key_secret");
            (
                ProviderCredentialSlot::ApiKey,
                api_key_record(profile.api_key.clone(), api_key_source.as_ref())?,
            )
        }
    };
    let keyless = matches!(
        (profile, api_key_source.as_ref()),
        (
            BuiltinProviderProfile::ChatCompletions(_) | BuiltinProviderProfile::Responses(_),
            Some(ApiKeySource::Keyless)
        )
    );
    if matches!(
        (profile, api_key_source.as_ref()),
        (
            BuiltinProviderProfile::OpenRouter(_),
            Some(ApiKeySource::Keyless)
        )
    ) {
        return Err("OpenRouter requires an API key".into());
    }
    let reference = (!keyless)
        .then(|| {
            ProviderCredentialReference::new(
                ProviderCredentialIdentity::random(),
                slot,
                match api_key_source.as_ref() {
                    Some(
                        ApiKeySource::ConfiguredNamed { name, .. }
                        | ApiKeySource::DeferredNamed { name },
                    ) => Some(name.as_str()),
                    None | Some(ApiKeySource::Direct(_) | ApiKeySource::Keyless) => None,
                },
            )
        })
        .transpose()?;
    object.insert(
        "credential".to_owned(),
        reference.as_ref().map_or_else(
            || serde_json::json!({"kind": "none"}),
            ProviderCredentialReference::to_value,
        ),
    );
    Ok(ProviderSetupPayload {
        settings: serde_json::to_vec_pretty(&settings)?,
        credential: match reference {
            None => setup_store::CredentialSetup::Keyless,
            Some(reference)
                if matches!(api_key_source, Some(ApiKeySource::DeferredNamed { .. })) =>
            {
                setup_store::CredentialSetup::DeferredNamed {
                    path: reference.path().clone(),
                }
            }
            Some(reference) => setup_store::CredentialSetup::Stored {
                secret: setup_store::SecretWrite {
                    path: reference.path().clone(),
                    contents: setup_store::SecretBytes::new(
                        secret.ok_or("stored credential setup requires credential bytes")?,
                    ),
                },
                named_source: match api_key_source {
                    Some(ApiKeySource::ConfiguredNamed { name, declaration }) => {
                        Some(setup_store::NamedSecretSource { name, declaration })
                    }
                    None
                    | Some(
                        ApiKeySource::Direct(_)
                        | ApiKeySource::Keyless
                        | ApiKeySource::DeferredNamed { .. },
                    ) => None,
                },
            },
        },
    })
}

/// Serialize an API-key record only for setup paths that publish it now.
fn api_key_record(
    value: String,
    source: Option<&ApiKeySource>,
) -> Result<Option<Vec<u8>>, serde_json::Error> {
    if matches!(source, Some(ApiKeySource::DeferredNamed { .. })) {
        Ok(None)
    } else {
        serde_json::to_vec(&credential_record::ApiKeyCredential::new(value)).map(Some)
    }
}

fn try_load_settings_profiles(
    files: Vec<(ProviderName, Vec<u8>)>,
) -> Result<BuiltinProviderProfiles, ProviderSettingsValidationError> {
    let mut profiles = BuiltinProviderProfiles::default();
    for (name, contents) in files {
        insert_settings_profile(&mut profiles, name, &contents)?;
    }
    Ok(profiles)
}

fn insert_settings_profile(
    profiles: &mut BuiltinProviderProfiles,
    name: ProviderName,
    contents: &[u8],
) -> Result<(), ProviderSettingsValidationError> {
    let (profile, credential) = parse_settings_profile(&name, contents).map_err(|reason| {
        ProviderSettingsValidationError {
            provider: name.clone(),
            reason,
        }
    })?;
    profiles.credentials.insert(name.clone(), credential);
    profiles.providers.insert(name, profile);
    Ok(())
}

fn validate_configure_settings(
    settings: &BTreeMap<String, Vec<u8>>,
) -> ClientResult<BuiltinProviderProfiles> {
    let mut files = Vec::with_capacity(settings.len());
    for (file_name, contents) in settings {
        let Some(stem) = file_name.strip_suffix(".json") else {
            return Err(ClientError::handler(
                "provider settings snapshot contains an invalid filename",
            ));
        };
        let name = ProviderName::try_new(stem.to_owned()).map_err(|_| {
            ClientError::handler("provider settings snapshot contains an invalid filename")
        })?;
        files.push((name, contents.clone()));
    }
    try_load_settings_profiles(files).map_err(|error| ClientError::handler(error.to_string()))
}

fn parse_settings_profile(
    name: &ProviderName,
    contents: &[u8],
) -> Result<(BuiltinProviderProfile, ProviderCredential), ProviderSettingsValidationReason> {
    let mut value: serde_json::Value = serde_json::from_slice(contents)
        .map_err(|_| ProviderSettingsValidationReason::InvalidJson)?;
    let object = value
        .as_object_mut()
        .ok_or(ProviderSettingsValidationReason::NotObject)?;
    if object.contains_key("auth")
        || object.contains_key("api_key")
        || object.contains_key("api_key_secret")
    {
        return Err(ProviderSettingsValidationReason::CredentialFieldsPresent);
    }
    reject_obsolete_local_summary_fields(object)?;
    let credential = parse_provider_credential(name, object)
        .map_err(|_| ProviderSettingsValidationReason::InvalidCredentialReference)?;
    object
        .remove("credential")
        .expect("validated reference must be present");
    match &credential {
        ProviderCredential::Stored(reference)
            if reference.slot() == ProviderCredentialSlot::OAuth =>
        {
            object.insert("auth".to_owned(), serde_json::json!({}));
        }
        ProviderCredential::Stored(_) | ProviderCredential::Keyless => {
            object.insert("api_key".to_owned(), serde_json::json!(""));
        }
    }
    let profile: BuiltinProviderProfile = serde_json::from_value(value)
        .map_err(|_| ProviderSettingsValidationReason::InvalidProfile)?;
    profile
        .validate_local_summary_compaction()
        .map_err(|error| match error {
            SummaryCompactionConfigError::MaxOutputTokensExceedContextWindow => {
                ProviderSettingsValidationReason::LocalSummaryOutputTokensExceedContextWindow
            }
            SummaryCompactionConfigError::MaxOutputBytesExceedNarrativeLimit => {
                ProviderSettingsValidationReason::LocalSummaryOutputBytesExceedNarrativeLimit
            }
        })?;
    profile
        .validate()
        .map_err(|_| ProviderSettingsValidationReason::InvalidProfile)?;
    let profile_matches_kind = match (&profile, &credential) {
        (BuiltinProviderProfile::Chatgpt(_), ProviderCredential::Stored(reference)) => {
            reference.slot() == ProviderCredentialSlot::OAuth
        }
        (
            BuiltinProviderProfile::ChatCompletions(_)
            | BuiltinProviderProfile::OpenRouter(_)
            | BuiltinProviderProfile::Responses(_),
            ProviderCredential::Stored(reference),
        ) => reference.slot() == ProviderCredentialSlot::ApiKey,
        (
            BuiltinProviderProfile::ChatCompletions(_) | BuiltinProviderProfile::Responses(_),
            ProviderCredential::Keyless,
        ) => true,
        (
            BuiltinProviderProfile::Chatgpt(_) | BuiltinProviderProfile::OpenRouter(_),
            ProviderCredential::Keyless,
        ) => false,
    };
    if !profile_matches_kind {
        return Err(ProviderSettingsValidationReason::CredentialKindMismatch);
    }
    Ok((profile, credential))
}

fn hydrate_profile_credentials(
    client: &ExtensionDataClient,
    profiles: &mut BuiltinProviderProfiles,
) -> BTreeMap<ProviderName, CredentialObservation> {
    hydrate_profile_credentials_with(profiles, |path| {
        client.request(
            tau_proto::ExtensionDataScope::Secret,
            tau_proto::ExtensionDataRequestOp::ReadFile { path },
        )
    })
}

/// Secret-free evidence used to detect a changed credential generation.
#[derive(Clone, Debug, Eq, PartialEq)]
enum CredentialObservation {
    /// Secret storage did not return credential bytes for this profile.
    Unavailable,
    /// Secret storage returned this opaque content generation.
    Contents(blake3::Hash),
}

/// Hydrates the supplied profiles with credential records returned by Secret
/// storage.
fn hydrate_profile_credentials_with(
    profiles: &mut BuiltinProviderProfiles,
    mut read_secret: impl FnMut(
        tau_proto::ExtensionDataPath,
    ) -> Result<
        tau_proto::ExtensionDataValue,
        tau_client::ExtensionDataRpcError,
    >,
) -> BTreeMap<ProviderName, CredentialObservation> {
    let mut observations = BTreeMap::new();
    let names = profiles.providers.keys().cloned().collect::<Vec<_>>();
    for name in names {
        let Some(credential) = profiles.credentials.get(&name).cloned() else {
            profiles.providers.remove(&name);
            continue;
        };
        let ProviderCredential::Stored(ref reference) = credential else {
            continue;
        };
        let path = reference.path().clone();
        let result = read_secret(path);
        let tau_proto::ExtensionDataValue::ReadFile { contents } = (match result {
            Ok(value) => value,
            Err(error) => {
                observations.insert(name.clone(), CredentialObservation::Unavailable);
                if credential_error_means_logged_out(
                    profiles.providers.get(&name),
                    &credential,
                    &error,
                ) {
                    profiles.missing_logins.insert(name.clone());
                }
                tracing::warn!(
                    target: LOG_TARGET,
                    credential_error = credential_hydration_error_category(&error),
                    "skipping provider with unavailable credential"
                );
                profiles.providers.remove(&name);
                continue;
            }
        }) else {
            observations.insert(name.clone(), CredentialObservation::Unavailable);
            tracing::warn!(
                target: LOG_TARGET,
                "skipping provider after unexpected credential result"
            );
            profiles.providers.remove(&name);
            continue;
        };
        observations.insert(
            name.clone(),
            CredentialObservation::Contents(blake3::hash(&contents)),
        );
        let valid = match profiles.providers.get_mut(&name) {
            Some(BuiltinProviderProfile::Chatgpt(profile)) => {
                serde_json::from_slice::<credential_record::ChatGptOAuthCredential>(&contents)
                    .map_err(|_| ())
                    .map(credential_record::ChatGptOAuthCredential::into_auth)
                    .map(|auth| profile.auth = auth)
            }
            Some(BuiltinProviderProfile::ChatCompletions(profile)) => {
                serde_json::from_slice::<credential_record::ApiKeyCredential>(&contents)
                    .map_err(|_| ())
                    .map(credential_record::ApiKeyCredential::into_value)
                    .and_then(|value| (!value.trim().is_empty()).then_some(value).ok_or(()))
                    .map(|value| profile.api_key = value)
            }
            Some(BuiltinProviderProfile::OpenRouter(profile)) => {
                serde_json::from_slice::<credential_record::ApiKeyCredential>(&contents)
                    .map_err(|_| ())
                    .map(credential_record::ApiKeyCredential::into_value)
                    .and_then(|value| (!value.trim().is_empty()).then_some(value).ok_or(()))
                    .map(|value| profile.api_key = value)
            }
            Some(BuiltinProviderProfile::Responses(profile)) => {
                serde_json::from_slice::<credential_record::ApiKeyCredential>(&contents)
                    .map_err(|_| ())
                    .map(credential_record::ApiKeyCredential::into_value)
                    .and_then(|value| (!value.trim().is_empty()).then_some(value).ok_or(()))
                    .map(|value| profile.api_key = value)
            }
            None => continue,
        };
        if valid.is_err() {
            tracing::warn!(
                target: LOG_TARGET,
                "skipping provider with invalid version-zero credential"
            );
            profiles.providers.remove(&name);
        }
    }
    observations
}

/// Returns the closed diagnostic category for a failed Secret hydration read.
fn credential_hydration_error_category(error: &tau_client::ExtensionDataRpcError) -> &'static str {
    match error {
        tau_client::ExtensionDataRpcError::Harness {
            kind: tau_proto::ExtensionDataErrorKind::NotFound,
            ..
        } => "not_found",
        tau_client::ExtensionDataRpcError::Harness { .. } => "harness",
        tau_client::ExtensionDataRpcError::Client(_) => "client",
        tau_client::ExtensionDataRpcError::Timeout => "timeout",
        tau_client::ExtensionDataRpcError::InputClosed => "input_closed",
        tau_client::ExtensionDataRpcError::Disconnect(_) => "disconnected",
    }
}

/// Returns whether Secret hydration safely proves a ChatGPT profile needs
/// login.
///
/// The harness owns the `NotFound` classification. This intentionally ignores
/// its descriptive message, which can include secret-storage implementation
/// details.
fn credential_error_means_logged_out(
    profile: Option<&BuiltinProviderProfile>,
    credential: &ProviderCredential,
    error: &tau_client::ExtensionDataRpcError,
) -> bool {
    matches!(profile, Some(BuiltinProviderProfile::Chatgpt(_)))
        && matches!(
            credential,
            ProviderCredential::Stored(reference)
                if reference.slot() == ProviderCredentialSlot::OAuth
        )
        && matches!(
            error,
            tau_client::ExtensionDataRpcError::Harness {
                kind: tau_proto::ExtensionDataErrorKind::NotFound,
                ..
            }
        )
}

fn run_openai_codex_login(
    network: &tau_provider::OutboundNetworkPolicy,
) -> Result<OpenAiAuth, Box<dyn Error>> {
    let (auth_url, expected_state, verifier) = tau_provider_codex::oauth::openai_codex_auth_url();

    eprintln!("\nOpen this URL in your browser:\n");
    eprintln!("{auth_url}");
    tau_term_screen::write_osc8_hyperlink(&mut std::io::stderr(), "Or click here.", &auth_url)?;
    eprintln!();
    eprintln!();
    eprintln!("After logging in, you'll be redirected to a page that won't load.");
    eprintln!("Copy the full URL from your browser's address bar and paste it here:\n");

    std::io::stdout().flush()?;
    let redirect_input: String = Input::new().with_prompt("Redirect URL").interact_text()?;

    let (code, state) = tau_provider_codex::oauth::parse_redirect_url(&redirect_input)
        .map_err(|e| path_std_io::Error::new(path_std_io::ErrorKind::InvalidInput, e))?;

    if state != expected_state {
        return Err("state mismatch — possible CSRF attack or stale URL".into());
    }

    eprintln!("Exchanging code for tokens...");
    let tokens = tau_provider_codex::oauth::openai_codex_exchange(&code, &verifier, network)?;

    eprintln!("Login successful!");
    Ok(OpenAiAuth {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_at_ms: tokens.expires_at_ms,
        account_id: tokens.account_id,
    })
}

/// Runs the extension on stdin/stdout.
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    tau_client::init_logging_for(LOG_TARGET);
    run(std::io::stdin(), std::io::stdout())
}

/// Runs the extension over arbitrary reader/writer streams.
///
/// The reader is moved to a background thread so retry-backoff sleeps can wake
/// early when the harness disconnects or sends a targeted cancel.
pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let settings_snapshot = Arc::new(Mutex::new(BuiltinProviderProfiles::default()));
    let profile_snapshot = Arc::clone(&settings_snapshot);
    run_inner_with_configured_settings(
        reader,
        writer,
        BuiltinProviderProfiles::default(),
        move |selected| {
            let profiles = profile_snapshot
                .lock()
                .expect("lock provider settings snapshot");
            selected.map_or_else(|| profiles.clone(), |provider| profiles.selected(provider))
        },
        settings_snapshot,
    )
}

#[cfg(test)]
fn run_with_auth<R, W>(reader: R, writer: W, auth: OpenAiAuth) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let profiles = profiles_with_chatgpt_auth(auth);
    let prompt_profiles = profiles.clone();
    run_inner(reader, writer, profiles, move |_| prompt_profiles.clone())
}

#[cfg(test)]
fn profiles_with_chatgpt_auth(auth: OpenAiAuth) -> BuiltinProviderProfiles {
    let provider = ProviderName::new(CHATGPT_PROVIDER_NAME);
    let mut providers = BTreeMap::new();
    providers.insert(
        provider.clone(),
        BuiltinProviderProfile::Chatgpt(ChatGptProfile {
            auth,
            responses_lite_compatibility: false,
        }),
    );
    BuiltinProviderProfiles {
        providers,
        credentials: BTreeMap::from([(
            provider,
            ProviderCredential::Stored(
                ProviderCredentialReference::new(
                    ProviderCredentialIdentity::random(),
                    ProviderCredentialSlot::OAuth,
                    None,
                )
                .expect("OAuth credential reference"),
            ),
        )]),
        missing_logins: Default::default(),
    }
}

#[cfg(test)]
fn run_inner<R, W, F>(
    reader: R,
    writer: W,
    startup_profiles: BuiltinProviderProfiles,
    load_prompt_profiles: F,
) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
    F: FnMut(Option<&ProviderName>) -> BuiltinProviderProfiles + 'static,
{
    run_inner_with_prompt_executor(
        reader,
        writer,
        startup_profiles,
        load_prompt_profiles,
        prompt_concurrency_limit(),
        production_prompt_executor(),
    )
}

/// Runs the production extension with its startup-stable settings snapshot.
fn run_inner_with_configured_settings<R, W, F>(
    reader: R,
    writer: W,
    startup_profiles: BuiltinProviderProfiles,
    load_prompt_profiles: F,
    settings_snapshot: SettingsSnapshot,
) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
    F: FnMut(Option<&ProviderName>) -> BuiltinProviderProfiles + 'static,
{
    run_inner_with_executors_and_clock_with_settings(
        reader,
        writer,
        load_prompt_profiles,
        prompt_concurrency_limit(),
        RuntimeExecutors {
            prompt: production_prompt_executor(),
            prewarm: production_prewarm_executor(),
            retry_clock: Arc::new(SystemRetryClock),
        },
        RuntimeStartup {
            profiles: startup_profiles,
            settings_snapshot,
            publish_models_after_configure: true,
        },
    )
}

#[cfg(any(test, feature = "quota-test-support"))]
fn run_inner_with_prompt_executor<R, W, F>(
    reader: R,
    writer: W,
    startup_profiles: BuiltinProviderProfiles,
    load_prompt_profiles: F,
    prompt_concurrency_limit: usize,
    prompt_executor: PromptExecutor,
) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
    F: FnMut(Option<&ProviderName>) -> BuiltinProviderProfiles + 'static,
{
    run_inner_with_executors(
        reader,
        writer,
        startup_profiles,
        load_prompt_profiles,
        prompt_concurrency_limit,
        prompt_executor,
        production_prewarm_executor(),
    )
}

#[cfg(any(test, feature = "quota-test-support"))]
fn run_inner_with_executors<R, W, F>(
    reader: R,
    writer: W,
    startup_profiles: BuiltinProviderProfiles,
    load_prompt_profiles: F,
    prompt_concurrency_limit: usize,
    prompt_executor: PromptExecutor,
    prewarm_executor: PrewarmExecutor,
) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
    F: FnMut(Option<&ProviderName>) -> BuiltinProviderProfiles + 'static,
{
    run_inner_with_executors_and_clock_with_settings(
        reader,
        writer,
        load_prompt_profiles,
        prompt_concurrency_limit,
        RuntimeExecutors {
            prompt: prompt_executor,
            prewarm: prewarm_executor,
            retry_clock: Arc::new(SystemRetryClock),
        },
        RuntimeStartup {
            profiles: startup_profiles,
            settings_snapshot: Arc::new(Mutex::new(BuiltinProviderProfiles::default())),
            publish_models_after_configure: false,
        },
    )
}

/// Effectful executors and retry clock injected into the provider runtime.
struct RuntimeExecutors {
    /// Runs one finite prompt attempt.
    prompt: PromptExecutor,
    /// Runs one finite transport prewarm.
    prewarm: PrewarmExecutor,
    /// Supplies retry policy's monotonic time.
    retry_clock: Arc<dyn RetryClock>,
}

/// Startup profiles and settings supplied to one provider runtime.
struct RuntimeStartup {
    /// Profiles used by test-only static model publication.
    profiles: BuiltinProviderProfiles,
    /// Parsed, immutable settings accepted by initial Configure.
    settings_snapshot: SettingsSnapshot,
    /// Whether production must defer model publication until Configure
    /// resolution.
    publish_models_after_configure: bool,
}

#[cfg(test)]
fn run_inner_with_executors_and_clock<R, W, F>(
    reader: R,
    writer: W,
    startup_profiles: BuiltinProviderProfiles,
    load_prompt_profiles: F,
    prompt_concurrency_limit: usize,
    executors: RuntimeExecutors,
) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
    F: FnMut(Option<&ProviderName>) -> BuiltinProviderProfiles + 'static,
{
    run_inner_with_executors_and_clock_with_settings(
        reader,
        writer,
        load_prompt_profiles,
        prompt_concurrency_limit,
        executors,
        RuntimeStartup {
            profiles: startup_profiles,
            settings_snapshot: Arc::new(Mutex::new(BuiltinProviderProfiles::default())),
            publish_models_after_configure: false,
        },
    )
}

/// Runs the extension with injected executors and settings.
fn run_inner_with_executors_and_clock_with_settings<R, W, F>(
    reader: R,
    writer: W,
    load_prompt_profiles: F,
    prompt_concurrency_limit: usize,
    executors: RuntimeExecutors,
    startup: RuntimeStartup,
) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
    F: FnMut(Option<&ProviderName>) -> BuiltinProviderProfiles + 'static,
{
    let (worker_tx, worker_rx) = mpsc::channel::<WorkerMessage>();
    let startup_responses_modes = startup.profiles.startup_responses_modes();
    let network = Arc::new(tau_provider::OutboundNetworkPolicy::from_env());
    let runtime = ProviderRuntime {
        load_prompt_profiles,
        startup_responses_modes,
        prompt_concurrency_limit,
        prompt_executor: executors.prompt,
        prewarm_executor: executors.prewarm,
        worker_tx,
        worker_rx,
        worker_waker: None,
        retry_scheduler: None,
        credential_admission: PromptCredentialAdmissionState::default(),
        retry_clock: executors.retry_clock,
        shared_cooldowns: BTreeMap::new(),
        shared_cooldown_generation: 0,
        codex_runtime: Arc::new(CodexRuntime::new(network)),
        prewarm_supervisor: PrewarmSupervisor::default(),
        provider_profile_identities: BTreeMap::new(),
        prewarm_profile_identities: BTreeMap::new(),
        cancellation: Arc::new(CancellationState::default()),
        prompt_queue: VecDeque::new(),
        active_prompts: 0,
        input_closed: false,
        cancel_generation: 0,
        quota: QuotaCoordinator::default(),
        oauth_refresh_rejections: OAuthRefreshRejectionCache::default(),
        unavailable_compact_identities: HashSet::new(),
        compact_profile_identities: HashMap::new(),
        extension_data_client: None,
        declared_credential_observations: None,
        declared_models: None,
        diagnostics: ProviderDiagnosticsState {
            output_queue: WorkerQueueState::enabled(),
            ..ProviderDiagnosticsState::default()
        },
    };
    let install_extension_data_client = startup.publish_models_after_configure;
    let mut runtime = TauExtensionRunner::new(ProviderExtension::<F>::new(
        startup.settings_snapshot,
        (!startup.publish_models_after_configure).then_some(startup.profiles),
    ))
    .start_manual_loop_with_extension_data_state(
        reader,
        writer,
        move |_handle, extension_data_client| {
            let mut runtime = runtime;
            if install_extension_data_client {
                runtime.extension_data_client = Some(extension_data_client);
            }
            runtime
        },
    )?;
    let worker_waker = runtime.waker();
    runtime.state_mut().set_worker_waker(worker_waker);
    let handle = runtime.handle();
    #[cfg(not(test))]
    tau_provider::debug_capture_writer::initialize_provider_debug_capture_transport(handle.clone());
    runtime.state_mut().initialize_quota(&handle)?;
    run_provider_loop(runtime)
}

/// Tau-client declaration for the built-in provider peer.
struct ProviderExtension<F> {
    /// Credential-free settings captured from the initial Configure frame.
    settings_snapshot: SettingsSnapshot,
    /// Models declared statically by test-only, non-secret profile loaders.
    startup_profiles: Option<BuiltinProviderProfiles>,
    /// Marker tying the declaration to the runtime state's profile loader type.
    _load_prompt_profiles: PhantomData<fn() -> F>,
}

impl<F> ProviderExtension<F> {
    /// Creates a provider declaration retaining credential-free settings.
    fn new(
        settings_snapshot: SettingsSnapshot,
        startup_profiles: Option<BuiltinProviderProfiles>,
    ) -> Self {
        Self {
            settings_snapshot,
            startup_profiles,
            _load_prompt_profiles: PhantomData,
        }
    }
}

impl<F> TauExtension for ProviderExtension<F>
where
    F: FnMut(Option<&ProviderName>) -> BuiltinProviderProfiles + 'static,
{
    type State = ProviderRuntime<F>;

    fn name(&self) -> &'static str {
        EXTENSION_NAME
    }

    fn kind(&self) -> ClientKind {
        ClientKind::Provider
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        let settings_snapshot = self.settings_snapshot;
        let startup_profiles = self.startup_profiles;
        let publish_models_after_configure = startup_profiles.is_none();
        let mut configured = false;
        // No past effectful provider events requested: provider work starts from
        // fresh live state. Harness session directory announcements are
        // current-state facts, so replay catch-up is allowed for diagnostics
        // policy only.
        builder
            .configure_raw(move |cx| {
                if configured {
                    return Ok(());
                }
                configured = true;
                let profiles = validate_configure_settings(&cx.configure.settings_files)?;
                *settings_snapshot
                    .lock()
                    .expect("lock provider settings snapshot") = profiles.clone();
                let provider_count = profiles.providers.len();
                if !publish_models_after_configure {
                    let model_count = models_for_profiles(&profiles).len();
                    tracing::info!(
                        target: LOG_TARGET,
                        providers = provider_count,
                        models = model_count,
                        "provider configured"
                    );
                    return Ok(());
                }
                cx.state
                    .set_startup_responses_modes(profiles.startup_responses_modes());
                let usable_profiles = cx.state.load_all_profiles(&cx.handle)?;
                let model_count = models_for_profiles(&usable_profiles).len();
                tracing::info!(
                    target: LOG_TARGET,
                    providers = provider_count,
                    models = model_count,
                    "provider configured"
                );
                Ok(())
            })
            .on_raw_live(
                tau_proto::EventSelector::Exact(EventName::AGENT_PROMPT_PREWARM_REQUESTED),
                handle_provider_delivery::<F>,
            )
            .on_raw_live(
                tau_proto::EventSelector::Exact(EventName::AGENT_CACHE_REFRESH_REQUESTED),
                handle_provider_delivery::<F>,
            )
            .on_raw_live(
                tau_proto::EventSelector::Exact(EventName::AGENT_CACHE_REFRESH_CANCEL_REQUESTED),
                handle_provider_delivery::<F>,
            )
            .on_raw_restore(
                tau_proto::EventSelector::Exact(EventName::HARNESS_SESSION_DIR),
                handle_provider_delivery::<F>,
            )
            .on_raw_live(
                tau_proto::EventSelector::Exact(EventName::HARNESS_SESSION_DIR),
                handle_provider_delivery::<F>,
            )
            .on_raw_live(
                tau_proto::EventSelector::Exact(EventName::UI_CANCEL_PROMPT),
                handle_provider_delivery::<F>,
            )
            .on_raw_live(
                tau_proto::EventSelector::Exact(EventName::SESSION_SHUTDOWN),
                handle_provider_delivery::<F>,
            )
            .on_raw_routed_live(
                tau_proto::EventSelector::Exact(EventName::UI_RETRY_PROMPT),
                handle_provider_delivery::<F>,
            )
            .on_raw_routed_live(
                tau_proto::EventSelector::Exact(EventName::AGENT_PROMPT_CREATED),
                handle_provider_delivery::<F>,
            )
            .ready_message("builtin provider ready");
        if let Some(profiles) = startup_profiles {
            builder.startup_transient_event(Event::ProviderModelsDeclared(
                ProviderModelsDeclared {
                    models: models_for_profiles(&profiles),
                },
            ));
        }
    }
}

fn handle_provider_delivery<F>(cx: RawEventContext<'_, ProviderRuntime<F>>) -> ClientResult<()>
where
    F: FnMut(Option<&ProviderName>) -> BuiltinProviderProfiles + 'static,
{
    if matches!(cx.event(), Event::AgentPromptCreated(_))
        && let Some(observation) = cx.state.diagnostics.receipt.current_input.as_mut()
    {
        observation.handler_started();
    }
    let event = cx.event().clone();
    if matches!(event, Event::AgentPromptCreated(_))
        && let Some(observation) = cx.state.diagnostics.receipt.current_input.as_mut()
    {
        observation.handler_materialized();
    }
    cx.state.handle_event(event, &cx.handle())
}

fn run_provider_loop<F>(
    runtime: ManualExtensionRuntime<ProviderRuntime<F>>,
) -> Result<(), Box<dyn Error>>
where
    F: FnMut(Option<&ProviderName>) -> BuiltinProviderProfiles + 'static,
{
    runtime.select_local_input_observation_path(
        run_provider_loop_inner::<F, true>,
        run_provider_loop_inner::<F, false>,
    )
}

/// Runs the provider loop on the startup-selected observation path.
fn run_provider_loop_inner<F, const OBSERVE_RECEIPT: bool>(
    mut runtime: ManualExtensionRuntime<ProviderRuntime<F>>,
) -> Result<(), Box<dyn Error>>
where
    F: FnMut(Option<&ProviderName>) -> BuiltinProviderProfiles + 'static,
{
    loop {
        let handle = runtime.handle();
        runtime
            .state_mut()
            .drain_workers_and_start_prompts(&handle)?;
        if runtime.state().is_finished() {
            runtime.finish()?;
            return Ok(());
        }

        let mut handled_input = false;
        if !runtime.state().input_closed {
            loop {
                match runtime.try_recv() {
                    Ok(ManualRuntimePoll::Message(frame)) => {
                        handled_input = true;
                        if OBSERVE_RECEIPT
                            && matches!(
                                &frame,
                                tau_proto::HarnessOutputMessage::Deliver(delivery)
                                    if matches!(delivery.event(), Event::AgentPromptCreated(_))
                            )
                            && let Some(observation) = runtime.take_local_input_observation()
                        {
                            runtime.state_mut().diagnostics.receipt.current_input =
                                Some(ReceiptObservation::new(observation));
                        }
                        if let tau_proto::HarnessOutputMessage::ExtensionDataResult(result) = frame
                        {
                            runtime
                                .state_mut()
                                .handle_extension_data_result(*result, &handle)?;
                            runtime
                                .state_mut()
                                .drain_workers_and_start_prompts(&handle)?;
                            continue;
                        }
                        match runtime.dispatch_one(frame)? {
                            DispatchOutcome::Continue => {}
                            DispatchOutcome::Disconnect(_) => {
                                runtime.state_mut().cancellation.shutdown();
                                runtime.state_mut().cancel_all_prewarms();
                                let _state = runtime.finish_detached();
                                return Ok(());
                            }
                            DispatchOutcome::StopRequested => {
                                runtime.state_mut().begin_input_shutdown();
                                runtime.finish()?;
                                return Ok(());
                            }
                        }
                        // Preserve source causality before observing a later
                        // queued EOF/shutdown frame. In particular, an
                        // immediately ready keyless admission owns its
                        // submitted transition before input closure.
                        runtime
                            .state_mut()
                            .drain_workers_and_start_prompts(&handle)?;
                    }
                    Ok(ManualRuntimePoll::InputClosed) => {
                        handled_input = true;
                        runtime.state_mut().begin_input_shutdown();
                        break;
                    }
                    Ok(ManualRuntimePoll::Empty) => break,
                    Err(error) => {
                        handled_input = true;
                        tracing::warn!(target: LOG_TARGET, "provider input reader failed: {error}");
                        runtime.state_mut().begin_input_shutdown();
                        break;
                    }
                }
            }
        }

        if !runtime.state().input_closed && runtime.state_mut().advance_initial_quota(&handle)? {
            handled_input = true;
        }

        if !handled_input {
            runtime.wait_for_wake();
        }
    }
}

/// Main-loop-owned state for asynchronous prompt credential admission.
#[derive(Default)]
struct PromptCredentialAdmissionState {
    /// Single bounded timer actor shared by every prompt credential RPC.
    deadlines: Option<PromptCredentialDeadlineScheduler>,
    /// Prompt admissions retained in source order until credential work
    /// completes.
    admissions: VecDeque<PendingPromptAdmission>,
    /// Exact Secret generations currently undergoing prompt-owned OAuth
    /// refresh.
    oauth_refreshes: HashMap<PromptOAuthRefreshKey, PromptOAuthRefresh>,
    /// Correlated Secret CAS/reload continuations for prompt-owned refreshes.
    oauth_rpcs: HashMap<String, PromptOAuthRpc>,
    /// Credential-hydrated startup snapshot retained for initial quota work.
    startup_quota_profiles: Option<BuiltinProviderProfiles>,
    /// Whether the post-Ready initial quota pass consumed its startup snapshot.
    initial_quota_started: bool,
}

/// Enabled-only receipt observation ownership and test network control.
#[derive(Default)]
struct ReceiptObservationState {
    /// Observation currently crossing prompt event dispatch.
    current_input: Option<ReceiptObservation>,
    /// Prevents test fixtures from starting real OAuth network work.
    #[cfg(test)]
    suppress_oauth_worker: bool,
}

/// Provider-owned private diagnostics policy and ephemeral observation state.
#[derive(Default)]
struct ProviderDiagnosticsState {
    /// Per-session provider request capture decision.
    session_debug_allowed: BTreeMap<tau_proto::SessionId, bool>,
    /// Private receipt observation owner.
    receipt: ReceiptObservationState,
    /// Enabled-only worker output queue observation state.
    output_queue: Option<Arc<WorkerQueueState>>,
}

/// Live provider event loop state after the Tau extension handshake completes.
struct ProviderRuntime<F> {
    /// Clones either the complete validated settings snapshot or one indexed
    /// provider for runtime auth/model resolution.
    load_prompt_profiles: F,
    /// Per-profile Responses mode captured at process startup.
    startup_responses_modes: BTreeMap<ProviderName, CodexMode>,
    /// Maximum number of prompt workers that may run at once.
    prompt_concurrency_limit: usize,
    /// Starts provider backend execution for one prompt job.
    prompt_executor: PromptExecutor,
    /// Runs one finite prewarm attempt outside the provider main loop.
    prewarm_executor: PrewarmExecutor,
    /// Sender used by prompt workers to return frames and completion notices.
    worker_tx: Sender<WorkerMessage>,
    /// Receiver used by the runtime loop to drain worker output.
    worker_rx: Receiver<WorkerMessage>,
    /// Wake handle signaled after workers enqueue output or completion.
    worker_waker: Option<ManualRuntimeWaker>,
    /// Single timer scheduler shared by every delayed logical prompt.
    retry_scheduler: Option<RetryScheduler>,
    /// Main-loop-owned prompt credential admission and deadline state.
    credential_admission: PromptCredentialAdmissionState,
    /// Monotonic clock used by retry policy and scheduler admission.
    retry_clock: Arc<dyn RetryClock>,
    /// Account/profile cooldowns, keyed without credentials or account ids.
    shared_cooldowns: BTreeMap<ProviderName, SharedCooldown>,
    /// Monotonic generation allocated whenever shared cooldown evidence
    /// changes.
    shared_cooldown_generation: u64,
    /// Shared ChatGPT backend runtime for prewarm and prompt execution.
    codex_runtime: Arc<CodexRuntime>,
    /// Main-loop ownership and cancellation for prewarm workers.
    prewarm_supervisor: PrewarmSupervisor,
    /// Last resolved inference identity for every configured provider
    /// namespace.
    provider_profile_identities: BTreeMap<ProviderName, Option<BackendProfileIdentity>>,
    /// Last Responses identity used to supervise prewarm transport state.
    prewarm_profile_identities: BTreeMap<ProviderName, InferenceProfileIdentity>,
    /// Cooperative cancellation state shared with prompt workers.
    cancellation: Arc<CancellationState>,
    /// Prompt jobs accepted while all worker slots were occupied.
    prompt_queue: VecDeque<PromptJob>,
    /// Number of prompt workers currently running.
    active_prompts: usize,
    /// True after the harness input stream disconnects or reaches EOF.
    input_closed: bool,
    /// Generation used to reject worker output and retry outcomes created
    /// before broadcast cancellation.
    cancel_generation: u64,
    /// Single-loop owner for ephemeral provider quota state.
    quota: QuotaCoordinator,
    /// Permanent refresh rejections scoped to exact credential generations.
    oauth_refresh_rejections: OAuthRefreshRejectionCache,
    /// Provider namespaces downgraded after a generic compact-route 404.
    unavailable_compact_identities: HashSet<InferenceProfileIdentity>,
    /// Latest resolved compaction generation for each provider namespace.
    compact_profile_identities: HashMap<ProviderName, InferenceProfileIdentity>,
    /// Runtime Secret-scope RPC client, installed after startup transport
    /// setup.
    extension_data_client: Option<ExtensionDataClient>,
    /// Credential generations observed when publishing the latest model
    /// snapshot.
    declared_credential_observations: Option<BTreeMap<ProviderName, CredentialObservation>>,
    /// Latest complete replacement model snapshot published by this runtime.
    declared_models: Option<Vec<ProviderModelInfo>>,
    /// Private diagnostics policy and observation state.
    diagnostics: ProviderDiagnosticsState,
}

impl<F> ProviderRuntime<F>
where
    F: FnMut(Option<&ProviderName>) -> BuiltinProviderProfiles + 'static,
{
    /// Loads every profile for startup and complete declaration work.
    fn load_all_profiles(
        &mut self,
        handle: &ClientHandle,
    ) -> ClientResult<BuiltinProviderProfiles> {
        let mut profiles = (self.load_prompt_profiles)(None);
        profiles.apply_startup_responses_modes(&self.startup_responses_modes);
        if let Some(client) = &self.extension_data_client {
            let observations = hydrate_profile_credentials(client, &mut profiles);
            self.publish_models_if_changed(&profiles, observations, handle)?;
            if !self.credential_admission.initial_quota_started {
                self.credential_admission.startup_quota_profiles = Some(profiles.clone());
            }
        }
        Ok(profiles)
    }

    /// Loads and hydrates only the selected provider profile at a prompt-like
    /// credential boundary.
    fn load_selected_profile(
        &mut self,
        provider: &ProviderName,
        handle: &ClientHandle,
    ) -> ClientResult<BuiltinProviderProfiles> {
        let mut profiles = (self.load_prompt_profiles)(Some(provider));
        profiles.apply_startup_responses_modes(&self.startup_responses_modes);
        if let Some(client) = &self.extension_data_client {
            let observations = hydrate_profile_credentials(client, &mut profiles);
            self.publish_selected_models_if_changed(provider, &profiles, observations, handle)?;
        }
        Ok(profiles)
    }

    /// Replaces one provider's contribution inside the latest complete model
    /// declaration after selected credential hydration.
    fn publish_selected_models_if_changed(
        &mut self,
        provider: &ProviderName,
        profiles: &BuiltinProviderProfiles,
        observations: BTreeMap<ProviderName, CredentialObservation>,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        let Some(previous_models) = self.declared_models.as_ref() else {
            return self.publish_models_if_changed(profiles, observations, handle);
        };
        let models = replace_provider_models(previous_models, provider, profiles);

        let mut complete_observations = self
            .declared_credential_observations
            .clone()
            .unwrap_or_default();
        complete_observations.remove(provider);
        complete_observations.extend(observations);
        self.publish_models_if_changed_from_models(models, complete_observations, handle)
    }

    /// Publishes one replacement declaration after credential or route
    /// usability changes.
    fn publish_models_if_changed(
        &mut self,
        profiles: &BuiltinProviderProfiles,
        observations: BTreeMap<ProviderName, CredentialObservation>,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        self.publish_models_if_changed_from_models(
            models_for_profiles(profiles),
            observations,
            handle,
        )
    }

    /// Publishes a replacement from complete model and credential snapshots.
    fn publish_models_if_changed_from_models(
        &mut self,
        mut models: Vec<ProviderModelInfo>,
        observations: BTreeMap<ProviderName, CredentialObservation>,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        if let Some(previous) = &self.declared_credential_observations {
            let superseded_negative_identities = reconcile_compact_state_after_credential_changes(
                previous,
                &observations,
                &mut self.compact_profile_identities,
                &mut self.unavailable_compact_identities,
            );
            for identity in superseded_negative_identities {
                self.codex_runtime.retire_compact_identity(identity);
            }
        }
        apply_compact_route_downgrades(
            &mut models,
            &self.compact_profile_identities,
            &self.unavailable_compact_identities,
        );
        if !declaration_needs_publication(
            self.declared_models.as_ref(),
            self.declared_credential_observations.as_ref(),
            &models,
            &observations,
        ) {
            return Ok(());
        }
        self.emit_model_declaration(models, handle)?;
        self.declared_credential_observations = Some(observations);
        Ok(())
    }

    /// Emits and remembers one complete model declaration.
    fn emit_model_declaration(
        &mut self,
        models: Vec<ProviderModelInfo>,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        handle.emit_transient(Event::ProviderModelsDeclared(ProviderModelsDeclared {
            models: models.clone(),
        }))?;
        self.declared_models = Some(models);
        Ok(())
    }

    /// Captures ChatGPT route modes from the profiles resolved at extension
    /// startup.
    fn set_startup_responses_modes(&mut self, modes: BTreeMap<ProviderName, CodexMode>) {
        self.startup_responses_modes = modes;
    }

    fn set_worker_waker(&mut self, waker: ManualRuntimeWaker) {
        self.retry_scheduler = Some(RetryScheduler::start(
            self.worker_tx.clone(),
            waker.clone(),
            Arc::clone(&self.retry_clock),
        ));
        self.credential_admission.deadlines = Some(PromptCredentialDeadlineScheduler::start(
            self.worker_tx.clone(),
            waker.clone(),
        ));
        self.worker_waker = Some(waker);
    }

    /// Rehydrates and republishes after a ChatGPT resolver may rotate
    /// credentials.
    fn observe_all_oauth_resolutions(
        &mut self,
        observes_oauth_refresh: bool,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        observe_oauth_resolution_with(observes_oauth_refresh, || {
            self.load_all_profiles(handle).map(|_| ())
        })
    }

    /// Rehydrates one provider after its resolver may rotate or adopt an OAuth
    /// credential generation.
    fn observe_selected_oauth_resolution(
        &mut self,
        provider: &ProviderName,
        observes_oauth_refresh: bool,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        observe_oauth_resolution_with(observes_oauth_refresh, || {
            self.load_selected_profile(provider, handle).map(|_| ())
        })
    }

    #[cfg(not(test))]
    fn initialize_quota(&mut self, _handle: &ClientHandle) -> ClientResult<()> {
        // The reactive loop advances this snapshot one provider per round after
        // draining input, so quota orchestration cannot sit ahead of inference.
        self.credential_admission.initial_quota_started = true;
        Ok(())
    }

    #[cfg(test)]
    fn initialize_quota(&mut self, _handle: &ClientHandle) -> ClientResult<()> {
        // Existing runtime tests use stateful profile loaders to model exact
        // prompt-time reload counts. Quota acquisition and reconciliation are
        // covered through the injected parser/coordinator seams instead of
        // introducing an unrelated startup load into those tests.
        Ok(())
    }

    /// Starts best-effort initial quota work for at most one provider.
    ///
    /// Returns whether more startup profiles remain for a later reactive round.
    #[cfg(not(test))]
    fn advance_initial_quota(&mut self, handle: &ClientHandle) -> ClientResult<bool> {
        let Some(mut selected) =
            take_next_initial_quota_profile(&mut self.credential_admission.startup_quota_profiles)
        else {
            return Ok(false);
        };
        if let Some(model) = models_for_profiles(&selected).into_iter().next()
            && let Some(PromptBackend::Responses(config)) = resolve_prompt_backend_without_refresh(
                &model.id,
                &mut selected,
                &mut self.oauth_refresh_rejections,
            )
        {
            let _ = self.ensure_quota_profile(&model.id.provider, &config, handle)?;
        }
        Ok(self.credential_admission.startup_quota_profiles.is_some())
    }

    #[cfg(test)]
    fn advance_initial_quota(&mut self, _handle: &ClientHandle) -> ClientResult<bool> {
        Ok(false)
    }

    fn ensure_quota_profile(
        &mut self,
        provider: &ProviderName,
        config: &ResolvedConfig,
        handle: &ClientHandle,
    ) -> ClientResult<bool> {
        self.reconcile_prewarm_profile(provider, config);
        let identity = quota_profile_identity(config);
        if let Some(event) = self.quota.ensure_profile(provider.clone(), identity) {
            handle.send(quota_report_message(event))?;
        }
        Ok(self.start_quota_fetch_if_due(provider, config))
    }

    fn start_quota_fetch_if_due(
        &mut self,
        provider: &ProviderName,
        config: &ResolvedConfig,
    ) -> bool {
        let Some((profile_epoch, fetch_start_sequence)) = self.quota.begin_fetch(provider) else {
            return false;
        };
        let Some(waker) = self.worker_waker.clone() else {
            self.quota.fail_fetch(provider, &profile_epoch);
            return false;
        };
        let tx = self.worker_tx.clone();
        let provider = provider.clone();
        let runtime = Arc::clone(&self.codex_runtime);
        let config = config.clone();
        thread::spawn(move || {
            let result = runtime.fetch_usage(&config);
            let _ = send_worker_message(
                &tx,
                &waker,
                WorkerMessage::QuotaFetchFinished {
                    provider,
                    profile_epoch,
                    fetch_start_sequence,
                    observed_at_unix_ms: UnixMillis::new(now_ms()),
                    result,
                },
            );
        });
        true
    }

    fn resolve_backend_with_quota(
        &mut self,
        model: &ModelId,
        profiles: &mut BuiltinProviderProfiles,
        handle: &ClientHandle,
    ) -> ClientResult<PromptBackend> {
        let observes_oauth_refresh = profiles
            .chatgpt_credential_reference(&model.provider)
            .is_some();
        let backend = resolve_prompt_backend(
            model,
            profiles,
            &mut self.oauth_refresh_rejections,
            self.codex_runtime.network(),
            self.extension_data_client.as_ref(),
        )
        .unwrap_or_else(|| PromptBackend::Unavailable {
            login_required: profiles
                .missing_login(&model.provider)
                .then(|| model.provider.clone()),
        });
        // Resolution can refresh or adopt a winning OAuth credential generation.
        // Rehydrate immediately so the declaration reflects that observed write.
        self.observe_selected_oauth_resolution(&model.provider, observes_oauth_refresh, handle)?;
        self.reconcile_provider_profile(&model.provider, backend_profile_identity(&backend));
        if let PromptBackend::Responses(config) = &backend {
            let identity = config.inference_identity();
            let changed = self
                .compact_profile_identities
                .insert(model.provider.clone(), identity)
                .is_some_and(|previous| previous != identity);
            if changed {
                let mut models = self.declared_models.as_ref().map_or_else(
                    || models_for_profiles(profiles),
                    |previous| replace_provider_models(previous, &model.provider, profiles),
                );
                apply_compact_route_downgrades(
                    &mut models,
                    &self.compact_profile_identities,
                    &self.unavailable_compact_identities,
                );
                self.emit_model_declaration(models, handle)?;
            }
            let _ = self.ensure_quota_profile(&model.provider, config, handle)?;
        } else {
            self.clear_prewarm_profile(&model.provider);
            if let Some(event) = self.quota.clear_profile(&model.provider) {
                handle.send(quota_report_message(event))?;
            }
        }
        Ok(backend)
    }

    /// Resolves a prompt whose OAuth state machine already completed, without
    /// re-entering blocking Secret or network refresh work.
    fn resolve_admitted_backend_with_quota(
        &mut self,
        model: &ModelId,
        profiles: &mut BuiltinProviderProfiles,
        handle: &ClientHandle,
    ) -> ClientResult<PromptBackend> {
        let backend = resolve_prompt_backend_without_refresh(
            model,
            profiles,
            &mut self.oauth_refresh_rejections,
        )
        .unwrap_or_else(|| PromptBackend::Unavailable {
            login_required: profiles
                .missing_login(&model.provider)
                .then(|| model.provider.clone()),
        });
        self.reconcile_provider_profile(&model.provider, backend_profile_identity(&backend));
        if let PromptBackend::Responses(config) = &backend {
            let identity = config.inference_identity();
            let changed = self
                .compact_profile_identities
                .insert(model.provider.clone(), identity)
                .is_some_and(|previous| previous != identity);
            if changed {
                let mut models = self.declared_models.as_ref().map_or_else(
                    || models_for_profiles(profiles),
                    |previous| replace_provider_models(previous, &model.provider, profiles),
                );
                apply_compact_route_downgrades(
                    &mut models,
                    &self.compact_profile_identities,
                    &self.unavailable_compact_identities,
                );
                self.emit_model_declaration(models, handle)?;
            }
            let _ = self.ensure_quota_profile(&model.provider, config, handle)?;
        } else {
            self.clear_prewarm_profile(&model.provider);
            if let Some(event) = self.quota.clear_profile(&model.provider) {
                handle.send(quota_report_message(event))?;
            }
        }
        Ok(backend)
    }

    fn schedule_quota_refresh(
        &mut self,
        provider: ProviderName,
        profile_epoch: tau_proto::ProviderQuotaEpoch,
        delay: Duration,
    ) {
        let Some(refresh_generation) = self.quota.schedule_refresh(&provider, &profile_epoch)
        else {
            return;
        };
        let Some(waker) = self.worker_waker.clone() else {
            return;
        };
        let tx = self.worker_tx.clone();
        thread::spawn(move || {
            thread::sleep(delay);
            let _ = send_worker_message(
                &tx,
                &waker,
                WorkerMessage::QuotaRefreshDue {
                    provider,
                    profile_epoch,
                    refresh_generation,
                },
            );
        });
    }

    fn drain_workers_and_start_prompts(&mut self, handle: &ClientHandle) -> ClientResult<()> {
        self.drain_worker_messages(handle)?;
        self.drain_prompt_admissions(handle)?;
        if !self.input_closed {
            self.park_cooled_queued_prompts(handle)?;
        }
        if let Some(prompt_worker_context) = with_queued_prompt_start_capacity(
            self.active_prompts,
            self.prompt_concurrency_limit,
            self.prompt_queue.len(),
            || self.prompt_worker_context(),
        ) {
            start_queued_prompts(
                &mut self.prompt_queue,
                &mut self.active_prompts,
                self.prompt_concurrency_limit,
                &prompt_worker_context,
                handle,
            )?;
        }
        Ok(())
    }

    fn is_finished(&self) -> bool {
        self.input_closed
            && self.active_prompts == 0
            && self.prewarm_supervisor.is_empty()
            && self.prompt_queue.is_empty()
            && self.credential_admission.admissions.is_empty()
            && self.credential_admission.oauth_refreshes.is_empty()
            && self
                .retry_scheduler
                .as_ref()
                .is_none_or(RetryScheduler::is_empty)
    }

    fn begin_input_shutdown(&mut self) {
        self.input_closed = true;
        self.cancel_all_prewarms();
        self.cancellation.shutdown();
        // No late Secret reply may resurrect an admission after input shutdown.
        for admission in &mut self.credential_admission.admissions {
            finish_receipt_canceled(&mut admission.receipt_observation);
        }
        self.credential_admission.admissions.clear();
        self.credential_admission.oauth_refreshes.clear();
        self.credential_admission.oauth_rpcs.clear();
        if let Some(deadlines) = &self.credential_admission.deadlines {
            deadlines.cancel_all();
        }
        if let Some(scheduler) = &self.retry_scheduler {
            scheduler.cancel_all();
        }
    }

    fn handle_event(&mut self, event: Event, handle: &ClientHandle) -> ClientResult<()> {
        match event {
            Event::HarnessSessionDir(session_dir) => self.record_session_debug_policy(session_dir),
            Event::AgentPromptPrewarmRequested(prewarm) => self.prewarm_backend(prewarm, handle)?,
            Event::AgentCacheRefreshRequested(refresh) => {
                self.cache_refresh_backend(refresh, handle)?
            }
            Event::AgentCacheRefreshCancelRequested(cancel) => {
                self.prewarm_supervisor.cancel_refresh(&cancel.refresh_id);
            }
            Event::AgentPromptCreated(prompt) => self.handle_prompt_created(prompt, handle)?,
            Event::UiCancelPrompt(cancel) => self.handle_cancel_prompt(cancel, handle)?,
            Event::UiRetryPrompt(retry) => self.handle_retry_prompt(retry)?,
            Event::SessionShutdown(_) => self.handle_session_shutdown(handle)?,
            _ => {}
        }
        Ok(())
    }

    fn record_session_debug_policy(&mut self, session_dir: tau_proto::HarnessSessionDir) {
        self.diagnostics.session_debug_allowed.insert(
            session_dir.session_id,
            !matches!(session_dir.status, tau_proto::SessionDirStatus::Ephemeral),
        );
    }

    fn prewarm_backend(
        &mut self,
        prewarm: tau_proto::AgentPromptPrewarmRequested,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        let requested_provider = prewarm.model.as_ref().map(|model| model.provider.clone());
        let mut profiles = match requested_provider.as_ref() {
            Some(provider) => self.load_selected_profile(provider, handle)?,
            None => BuiltinProviderProfiles::default(),
        };
        let observes_oauth_refresh = requested_provider
            .as_ref()
            .is_some_and(|provider| profiles.chatgpt_credential_reference(provider).is_some());
        let resolved = resolve_prewarm_backend(
            &prewarm,
            &mut profiles,
            &mut self.oauth_refresh_rejections,
            self.codex_runtime.network(),
            self.extension_data_client.as_ref(),
        );
        if let Some(provider) = requested_provider.as_ref() {
            self.observe_selected_oauth_resolution(provider, observes_oauth_refresh, handle)?;
        }
        let Some((model, config)) = resolved else {
            if let Some(provider) = requested_provider {
                self.clear_prewarm_profile(&provider);
            }
            return Ok(());
        };
        if self.shared_cooldowns.contains_key(&model.provider) {
            tracing::debug!(
                target: LOG_TARGET,
                provider = %model.provider,
                "skipping prompt prewarm during Provider cooldown",
            );
            return Ok(());
        }
        self.reconcile_provider_profile(
            &model.provider,
            backend_profile_identity(&PromptBackend::Responses(config.clone())),
        );
        self.reconcile_prewarm_profile(&model.provider, &config);
        let key = PrewarmKey {
            provider: model.provider,
            agent_id: prewarm.agent_id.clone(),
            refresh_id: None,
        };
        let Some((generation, abort)) = self.prewarm_supervisor.begin(key.clone()) else {
            tracing::debug!(
                target: LOG_TARGET,
                session_id = %prewarm.session_id,
                "skipping prompt prewarm: duplicate or supervisor capacity reached",
            );
            return Ok(());
        };
        let debug_provider_requests = debug_provider_requests_for(
            &prewarm.session_id,
            &self.diagnostics.session_debug_allowed,
        );
        let executor = self.prewarm_executor.clone();
        let runtime = self.codex_runtime.clone();
        let tx = self.worker_tx.clone();
        let waker = self
            .worker_waker
            .as_ref()
            .expect("provider runtime worker waker is installed before dispatch")
            .clone();
        thread::spawn(move || {
            let _ = executor(PrewarmExecution {
                runtime,
                config,
                request: prewarm,
                debug_provider_requests,
                abort,
            });
            let _ = send_worker_message(
                &tx,
                &waker,
                WorkerMessage::PrewarmDone {
                    key,
                    generation,
                    terminal: None,
                },
            );
        });
        Ok(())
    }

    fn cache_refresh_backend(
        &mut self,
        refresh: tau_proto::AgentCacheRefreshRequested,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        let refresh_id = refresh.refresh_id.clone();
        let requested_provider = refresh
            .prompt
            .model
            .as_ref()
            .map(|model| model.provider.clone());
        let mut profiles = match requested_provider.as_ref() {
            Some(provider) => self.load_selected_profile(provider, handle)?,
            None => BuiltinProviderProfiles::default(),
        };
        let observes_oauth_refresh = requested_provider
            .as_ref()
            .is_some_and(|provider| profiles.chatgpt_credential_reference(provider).is_some());
        let resolved = resolve_prewarm_backend(
            &refresh.prompt,
            &mut profiles,
            &mut self.oauth_refresh_rejections,
            self.codex_runtime.network(),
            self.extension_data_client.as_ref(),
        );
        if let Some(provider) = requested_provider.as_ref() {
            self.observe_selected_oauth_resolution(provider, observes_oauth_refresh, handle)?;
        }
        let Some((model, config)) = resolved else {
            return send_cache_refresh_terminal(
                handle,
                refresh_id,
                tau_proto::ProviderCacheRefreshStatus::Unsupported,
            );
        };
        if self.shared_cooldowns.contains_key(&model.provider) {
            return send_cache_refresh_terminal(
                handle,
                refresh_id,
                tau_proto::ProviderCacheRefreshStatus::Failed,
            );
        }
        self.reconcile_provider_profile(
            &model.provider,
            backend_profile_identity(&PromptBackend::Responses(config.clone())),
        );
        self.reconcile_prewarm_profile(&model.provider, &config);
        let key = PrewarmKey {
            provider: model.provider,
            agent_id: refresh.prompt.agent_id.clone(),
            refresh_id: Some(refresh_id.clone()),
        };
        let Some((generation, abort)) = self.prewarm_supervisor.begin(key.clone()) else {
            return send_cache_refresh_terminal(
                handle,
                refresh_id,
                tau_proto::ProviderCacheRefreshStatus::Failed,
            );
        };
        let deadline =
            Instant::now() + Duration::from_millis(u64::from(refresh.stop_after_millis.get()));
        let deadline_abort = abort.clone();
        thread::spawn(move || {
            if let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
                thread::sleep(remaining);
            }
            deadline_abort.cancel();
        });
        let debug_provider_requests = debug_provider_requests_for(
            &refresh.prompt.session_id,
            &self.diagnostics.session_debug_allowed,
        );
        let executor = self.prewarm_executor.clone();
        let runtime = self.codex_runtime.clone();
        let tx = self.worker_tx.clone();
        let waker = self
            .worker_waker
            .as_ref()
            .expect("provider runtime worker waker is installed before dispatch")
            .clone();
        thread::spawn(move || {
            let status = executor(PrewarmExecution {
                runtime,
                config,
                request: refresh.prompt,
                debug_provider_requests,
                abort,
            });
            let status = if deadline <= Instant::now() {
                tau_proto::ProviderCacheRefreshStatus::DeadlineExceeded
            } else {
                status
            };
            let _ = send_worker_message(
                &tx,
                &waker,
                WorkerMessage::PrewarmDone {
                    key,
                    generation,
                    terminal: Some((refresh_id, status)),
                },
            );
        });
        Ok(())
    }

    fn reconcile_provider_profile(
        &mut self,
        provider: &ProviderName,
        identity: Option<BackendProfileIdentity>,
    ) {
        let (changed, removed_cooldown) = reconcile_inference_identity(
            &mut self.provider_profile_identities,
            &mut self.shared_cooldowns,
            provider,
            identity,
        );
        if changed {
            if let Some(cooldown) = removed_cooldown
                && let Some(scheduler) = &self.retry_scheduler
            {
                scheduler.release_cooldown(
                    provider.clone(),
                    cooldown.generation,
                    self.retry_clock.now(),
                );
            }
            self.prewarm_supervisor.cancel_provider(provider);
            if let Err(error) = self.codex_runtime.invalidate_profile_websockets(provider) {
                tracing::debug!(
                    target: LOG_TARGET,
                    "failed to invalidate websocket pool after profile change: {error}",
                );
            }
        }
    }

    fn reconcile_prewarm_profile(&mut self, provider: &ProviderName, config: &ResolvedConfig) {
        let identity = responses_profile_identity(config);
        let changed = self
            .prewarm_profile_identities
            .insert(provider.clone(), identity)
            .is_some_and(|previous| previous != identity);
        if changed {
            self.prewarm_supervisor.cancel_provider(provider);
            if let Err(error) = self.codex_runtime.invalidate_profile_websockets(provider) {
                tracing::debug!(
                    target: LOG_TARGET,
                    "failed to invalidate websocket pool after profile change: {error}",
                );
            }
        }
    }

    fn clear_prewarm_profile(&mut self, provider: &ProviderName) {
        if self.prewarm_profile_identities.remove(provider).is_some() {
            self.prewarm_supervisor.cancel_provider(provider);
            if let Err(error) = self.codex_runtime.invalidate_profile_websockets(provider) {
                tracing::debug!(
                    target: LOG_TARGET,
                    "failed to invalidate websocket pool after profile removal: {error}",
                );
            }
        }
    }

    /// Invalidates one provider profile's cooldown and advances only its parked
    /// logical prompts with stable anti-herd jitter.
    fn release_shared_cooldown(&mut self, provider: &ProviderName) {
        if let Some(cooldown) = self.shared_cooldowns.remove(provider)
            && let Some(scheduler) = &self.retry_scheduler
        {
            scheduler.release_cooldown(
                provider.clone(),
                cooldown.generation,
                self.retry_clock.now(),
            );
        }
    }

    fn cancel_all_prewarms(&mut self) {
        self.prewarm_supervisor.cancel_all();
    }

    fn handle_prompt_created(
        &mut self,
        prompt: tau_proto::AgentPromptCreated,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        let agent_prompt_id = prompt.agent_prompt_id.clone();
        let prompt = materialize_prompt(prompt);
        if self.cancellation.take_canceled(&agent_prompt_id) {
            return self.finish_canceled_prompt(&agent_prompt_id, &prompt, handle);
        }
        trace_provider_prompt(&prompt, &agent_prompt_id);
        self.start_or_reject_prompt(agent_prompt_id, prompt, handle)
    }

    fn finish_canceled_prompt(
        &mut self,
        agent_prompt_id: &tau_proto::AgentPromptId,
        prompt: &tau_proto::AgentPromptCreated,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        let mut frame_writer = handle_report_sink(handle);
        finish_canceled(agent_prompt_id, prompt, &mut frame_writer)
            .map_err(|error| ClientError::handler(error.to_string()))?;
        Ok(())
    }

    /// Finish a canceled retry job with any backend observed by an earlier
    /// finite attempt in the same logical turn.
    fn finish_canceled_job(&mut self, job: &PromptJob, handle: &ClientHandle) -> ClientResult<()> {
        let mut frame_writer = handle_report_sink(handle);
        emit_canceled_correlated(
            &job.agent_prompt_id,
            &job.prompt,
            &mut frame_writer,
            job.observed_backend.clone(),
            tau_proto::ProviderAttempt::ONE,
        )
        .map_err(|error| ClientError::handler(error.to_string()))
    }

    fn finish_identity_changed_prompt(
        &mut self,
        job: &PromptJob,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        let mut finished = simple_finished(
            job.agent_prompt_id.clone(),
            job.prompt.agent_id.clone(),
            job.prompt.originator.clone(),
            "ChatGPT identity changed; automatic retry refused",
        );
        finished.backend = job.observed_backend.clone();
        handle.send(HarnessInputMessage::emit_transient(
            Event::ProviderResponseFinishedReported(finished),
        ))
    }

    /// Finishes one canceled credential admission and closes any correlated
    /// manual-retry request that had already left the scheduler.
    fn finish_canceled_admission(
        &mut self,
        admission: &PendingPromptAdmission,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        match &admission.kind {
            PendingPromptAdmissionKind::Initial {
                agent_prompt_id,
                prompt,
            } => self.finish_canceled_prompt(agent_prompt_id, prompt, handle)?,
            PendingPromptAdmissionKind::RetryDue(job)
            | PendingPromptAdmissionKind::Manual { job, .. } => {
                self.finish_canceled_job(job, handle)?;
            }
        }
        if let PendingPromptAdmissionKind::Manual { job, request_id } = &admission.kind {
            handle.emit_transient(Event::ProviderRetryPromptResultReported(
                tau_proto::ProviderRetryPromptResult {
                    request_id: request_id.clone(),
                    agent_prompt_id: job.agent_prompt_id.clone(),
                    status: tau_proto::RetryPromptStatus::NotParked,
                },
            ))?;
        }
        Ok(())
    }

    fn start_or_reject_prompt(
        &mut self,
        agent_prompt_id: tau_proto::AgentPromptId,
        prompt: tau_proto::AgentPromptCreated,
        _handle: &ClientHandle,
    ) -> ClientResult<()> {
        self.prewarm_supervisor.cancel_key(&PrewarmKey {
            provider: prompt.model.provider.clone(),
            agent_id: prompt.agent_id.clone(),
            refresh_id: None,
        });
        let mut receipt_observation = self.diagnostics.receipt.current_input.take();
        if let Some(observation) = receipt_observation.as_mut() {
            observation.handler_dispatched();
        }
        let settings_started_at = receipt_observation.as_ref().map(|_| Instant::now());
        let mut profiles = (self.load_prompt_profiles)(Some(&prompt.model.provider));
        profiles.apply_startup_responses_modes(&self.startup_responses_modes);
        if let (Some(observation), Some(started_at)) =
            (receipt_observation.as_mut(), settings_started_at)
        {
            observation.settings_cloned(started_at.elapsed(), profiles.providers.len());
        }
        if self.extension_data_client.is_none() {
            return self.start_resolved_prompt(
                agent_prompt_id,
                prompt,
                &mut profiles,
                receipt_observation,
                _handle,
            );
        }
        let request_id = if let (Some(ProviderCredential::Stored(reference)), Some(client)) = (
            profiles.credentials.get(&prompt.model.provider),
            self.extension_data_client.as_ref(),
        ) {
            client.start_request(
                tau_proto::ExtensionDataScope::Secret,
                tau_proto::ExtensionDataRequestOp::ReadFile {
                    path: reference.path().clone(),
                },
            )?
        } else {
            String::new()
        };
        let ready = request_id.is_empty();
        if !ready {
            if let Some(observation) = receipt_observation.as_mut() {
                observation.secret_started();
            }
            self.arm_prompt_credential_timeout(request_id.clone());
        }
        self.credential_admission
            .admissions
            .push_back(PendingPromptAdmission {
                kind: PendingPromptAdmissionKind::Initial {
                    agent_prompt_id,
                    prompt,
                },
                profiles,
                request_id: (!request_id.is_empty()).then_some(request_id),
                observations: ready.then(BTreeMap::new),
                oauth_refresh: None,
                oauth_forced: false,
                receipt_observation,
            });
        Ok(())
    }

    /// Starts one credential-eligible prompt after preserving admission order.
    fn start_resolved_prompt(
        &mut self,
        agent_prompt_id: tau_proto::AgentPromptId,
        prompt: tau_proto::AgentPromptCreated,
        profiles: &mut BuiltinProviderProfiles,
        mut receipt_observation: Option<ReceiptObservation>,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        let quota_started_at = receipt_observation.as_ref().map(|_| Instant::now());
        let backend = self.resolve_admitted_backend_with_quota(&prompt.model, profiles, handle)?;
        if let (Some(observation), Some(started_at)) =
            (receipt_observation.as_mut(), quota_started_at)
        {
            observation.quota_resolved(started_at.elapsed());
        }
        let profile_identity = backend_profile_identity(&backend);
        let pinned_chatgpt_identity = match &backend {
            PromptBackend::Responses(config) => Some(config.chatgpt_retry_identity()),
            _ => None,
        };
        let mut frame_writer = handle_report_sink(handle);
        write_prompt_submitted(&agent_prompt_id, &prompt.originator, &mut frame_writer)
            .map_err(|error| ClientError::handler(error.to_string()))?;
        let mut job = PromptJob {
            agent_prompt_id,
            debug_provider_requests: debug_provider_requests_for(
                &prompt.session_id,
                &self.diagnostics.session_debug_allowed,
            ),
            prompt,
            backend,
            pinned_chatgpt_identity,
            profile_identity,
            retry_state: PromptRetryState::default(),
            observed_backend: None,
            cancel_generation: self.cancel_generation,
            manual_cooldown_bypass: false,
            cooldown_probe: None,
            receipt_observation,
        };
        if let Some(cooldown) = self
            .shared_cooldowns
            .get(&job.prompt.model.provider)
            .copied()
            .filter(|cooldown| cooldown.not_before > self.retry_clock.now())
        {
            let now = self.retry_clock.now();
            let due = cooldown_due_for_job(cooldown.not_before, &job);
            emit_retry_status(&job, cooldown.class, due, now, None, handle)?;
            if let Some(observation) = job.receipt_observation.as_mut() {
                let depth = self
                    .retry_scheduler
                    .as_ref()
                    .expect("retry scheduler starts with the runtime waker")
                    .delayed_count
                    .load(AtomicOrdering::Relaxed)
                    .saturating_add(1);
                observation.cooldown_queued(depth);
            }
            self.retry_scheduler
                .as_ref()
                .expect("retry scheduler starts with the runtime waker")
                .schedule(
                    job,
                    now,
                    Some(CooldownConstraint {
                        generation: cooldown.generation,
                        boundary: cooldown.not_before,
                    }),
                );
        } else {
            self.enqueue_or_start_prompt(job);
        }
        Ok(())
    }

    /// Starts the selected credential read for a delayed prompt. Each retry
    /// owns a fresh read rather than inheriting the original prompt's
    /// credential generation.
    fn start_retry_credential_read(
        &mut self,
        mut kind: PendingPromptAdmissionKind,
    ) -> ClientResult<Option<PendingPromptAdmissionKind>> {
        let provider = kind.prompt().model.provider.clone();
        let profiles = load_fresh_retry_profiles(
            &mut self.load_prompt_profiles,
            &self.startup_responses_modes,
            &provider,
        );
        let Some(client) = self.extension_data_client.as_ref() else {
            return Ok(Some(kind));
        };
        let mut receipt_observation = kind.take_receipt_observation();
        let request_id = if let Some(ProviderCredential::Stored(reference)) =
            profiles.credentials.get(&provider)
        {
            Some(client.start_request(
                tau_proto::ExtensionDataScope::Secret,
                tau_proto::ExtensionDataRequestOp::ReadFile {
                    path: reference.path().clone(),
                },
            )?)
        } else {
            None
        };
        let ready = request_id.is_none();
        if let Some(request_id) = &request_id {
            if let Some(observation) = receipt_observation.as_mut() {
                observation.secret_started();
            }
            self.arm_prompt_credential_timeout(request_id.clone());
        }
        self.credential_admission
            .admissions
            .push_back(PendingPromptAdmission {
                kind,
                profiles,
                request_id,
                observations: ready.then(BTreeMap::new),
                oauth_refresh: None,
                oauth_forced: false,
                receipt_observation,
            });
        Ok(None)
    }

    /// Arms the finite deadline for one already-sent credential RPC.
    fn arm_prompt_credential_timeout(&self, request_id: String) {
        self.credential_admission
            .deadlines
            .as_ref()
            .expect("credential deadline scheduler starts with the runtime waker")
            .schedule(request_id, Instant::now() + PROMPT_CREDENTIAL_RPC_TIMEOUT);
    }

    fn enqueue_or_start_prompt(&mut self, job: PromptJob) {
        if self.active_prompts >= self.prompt_concurrency_limit {
            let mut job = job;
            if let Some(observation) = job.receipt_observation.as_mut() {
                observation.queued(self.prompt_queue.len().saturating_add(1));
            }
            self.prompt_queue.push_back(job);
            return;
        }
        let prompt_worker_context = self.prompt_worker_context();
        start_prompt_job(job, &mut self.active_prompts, &prompt_worker_context);
    }

    /// Records one asynchronous Secret reply for later FIFO admission.
    ///
    /// Unknown replies belong to another caller-owned extension-data operation
    /// or an invalidated deadline and are deliberately ignored.
    fn handle_extension_data_result(
        &mut self,
        result: tau_proto::ExtensionDataResult,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        if let Some(deadlines) = &self.credential_admission.deadlines {
            deadlines.cancel(result.request_id.clone());
        }
        if let Some(continuation) = self
            .credential_admission
            .oauth_rpcs
            .remove(&result.request_id)
        {
            return self.handle_prompt_oauth_rpc_result(continuation, result.result, handle);
        }
        let Some(admission) = self
            .credential_admission
            .admissions
            .iter_mut()
            .find(|admission| admission.request_id.as_deref() == Some(&result.request_id))
        else {
            return Ok(());
        };
        let payload = result.result;
        if let Some(observation) = admission.receipt_observation.as_mut() {
            let bytes = match &payload {
                tau_proto::ExtensionDataResultPayload::Ok {
                    value: tau_proto::ExtensionDataValue::ReadFile { contents },
                } => u64::try_from(contents.len()).unwrap_or(u64::MAX),
                _ => 0,
            };
            observation.secret_finished(bytes);
        }
        let observations =
            hydrate_profile_credentials_with(&mut admission.profiles, |_| match payload.clone() {
                tau_proto::ExtensionDataResultPayload::Ok { value } => Ok(value),
                tau_proto::ExtensionDataResultPayload::Error { kind, message } => {
                    Err(tau_client::ExtensionDataRpcError::Harness { kind, message })
                }
            });
        admission.observations = Some(observations);
        self.stage_prompt_oauth_refresh(&result.request_id);
        if let Some(admission) = self
            .credential_admission
            .admissions
            .iter_mut()
            .find(|admission| admission.request_id.as_deref() == Some(&result.request_id))
        {
            // Consume the correlation only after OAuth staging used it to find
            // the admission. Duplicate/late harness replies are then no-ops.
            admission.request_id = None;
        }
        Ok(())
    }

    /// Advances one prompt OAuth CAS/reload continuation.
    fn handle_prompt_oauth_rpc_result(
        &mut self,
        continuation: PromptOAuthRpc,
        result: tau_proto::ExtensionDataResultPayload,
        _handle: &ClientHandle,
    ) -> ClientResult<()> {
        let continuation_key = match &continuation {
            PromptOAuthRpc::CompareAndSwap { key } | PromptOAuthRpc::Reload { key } => key,
        };
        let response_bytes = match &result {
            tau_proto::ExtensionDataResultPayload::Ok {
                value: tau_proto::ExtensionDataValue::ReadFile { contents },
            } => u64::try_from(contents.len()).unwrap_or(u64::MAX),
            _ => 0,
        };
        for admission in &mut self.credential_admission.admissions {
            if admission.oauth_refresh.as_ref() == Some(continuation_key)
                && let Some(observation) = admission.receipt_observation.as_mut()
            {
                observation.secret_finished(response_bytes);
            }
        }
        if let Some(refresh) = self
            .credential_admission
            .oauth_refreshes
            .get_mut(continuation_key)
        {
            refresh.secret_in_flight = false;
        }
        match continuation {
            PromptOAuthRpc::CompareAndSwap { key } => {
                if prompt_oauth_cas_requires_reload(&result) {
                    self.start_prompt_oauth_rpc(
                        tau_proto::ExtensionDataRequestOp::ReadFile {
                            path: key.path.clone(),
                        },
                        PromptOAuthRpc::Reload { key },
                    )
                } else {
                    self.fail_prompt_oauth_refresh(&key, None);
                    Ok(())
                }
            }
            PromptOAuthRpc::Reload { key } => {
                let matching = self
                    .credential_admission
                    .admissions
                    .iter()
                    .enumerate()
                    .filter(|(_, admission)| admission.oauth_refresh.as_ref() == Some(&key))
                    .map(|(index, admission)| {
                        (index, admission.kind.prompt().model.provider.clone())
                    })
                    .collect::<Vec<_>>();
                let (authoritative, observation) = match result {
                    tau_proto::ExtensionDataResultPayload::Ok {
                        value: tau_proto::ExtensionDataValue::ReadFile { contents },
                    } => (
                        serde_json::from_slice::<credential_record::ChatGptOAuthCredential>(
                            &contents,
                        )
                        .ok()
                        .map(credential_record::ChatGptOAuthCredential::into_auth),
                        Some(CredentialObservation::Contents(blake3::hash(&contents))),
                    ),
                    tau_proto::ExtensionDataResultPayload::Ok { .. }
                    | tau_proto::ExtensionDataResultPayload::Error { .. } => (None, None),
                };
                if let Some(authoritative) = authoritative {
                    self.complete_prompt_oauth_refresh(&key, authoritative);
                } else {
                    self.fail_prompt_oauth_refresh(&key, None);
                }
                for (index, provider) in matching {
                    if let Some(observation) = observation.clone()
                        && let Some(admission) = self.credential_admission.admissions.get_mut(index)
                    {
                        admission.observations = Some(BTreeMap::from([(provider, observation)]));
                    }
                }
                Ok(())
            }
        }
    }

    /// Starts or joins the exact-generation OAuth refresh needed by one
    /// hydrated prompt admission.
    fn stage_prompt_oauth_refresh(&mut self, request_id: &str) {
        let Some(index) = self
            .credential_admission
            .admissions
            .iter()
            .position(|admission| admission.request_id.as_deref() == Some(request_id))
        else {
            return;
        };
        let (provider, model) = {
            let admission = &self.credential_admission.admissions[index];
            (
                admission.kind.prompt().model.provider.clone(),
                admission.kind.prompt().model.clone(),
            )
        };
        let Some(reference) = self.credential_admission.admissions[index]
            .profiles
            .chatgpt_credential_reference(&provider)
            .cloned()
        else {
            return;
        };
        let Some(BuiltinProviderProfile::Chatgpt(profile)) = self.credential_admission.admissions
            [index]
            .profiles
            .providers
            .get(&provider)
        else {
            return;
        };
        let current = profile.auth.clone();
        let mode = profile.responses_mode();
        let current_config = tau_provider_codex::resolved_config_for_provider_model(
            &model.provider,
            &model.model,
            tau_provider_codex::ResolvedCredentials::new(
                current.access_token.clone(),
                current.account_id.clone(),
            ),
            mode,
        );
        let identity = backend_profile_identity(&PromptBackend::Responses(current_config));
        let Some(CredentialObservation::Contents(generation)) =
            self.credential_admission.admissions[index]
                .observations
                .as_ref()
                .and_then(|observations| observations.get(&provider))
        else {
            return;
        };
        let key = PromptOAuthRefreshKey {
            provider: provider.clone(),
            path: reference.path().clone(),
            generation: generation.to_hex().to_string(),
            lite_compatibility: mode.is_lite_compatibility(),
        };
        if let Some(refresh) = self.credential_admission.oauth_refreshes.get(&key) {
            let transport_finished = refresh.transport_finished;
            let secret_in_flight = refresh.secret_in_flight;
            let forced = refresh.forced
                || identity.is_some_and(|identity| {
                    self.oauth_refresh_rejections
                        .take_unauthorized(&provider, identity)
                });
            self.credential_admission.admissions[index].oauth_refresh = Some(key);
            self.credential_admission.admissions[index].oauth_forced = forced;
            if let Some(observation) = self.credential_admission.admissions[index]
                .receipt_observation
                .as_mut()
            {
                observation.oauth_joined();
                if transport_finished {
                    observation.oauth_transport_finished();
                }
                if secret_in_flight {
                    observation.secret_started();
                }
            }
            return;
        }
        if identity.is_some_and(|identity| {
            self.oauth_refresh_rejections
                .unauthorized_exhausted(&provider, identity)
        }) {
            if let Some(BuiltinProviderProfile::Chatgpt(profile)) =
                self.credential_admission.admissions[index]
                    .profiles
                    .providers
                    .get_mut(&provider)
            {
                profile.auth.access_token.clear();
            }
            return;
        }
        let forced = identity.is_some_and(|identity| {
            self.oauth_refresh_rejections
                .take_unauthorized(&provider, identity)
        });
        if self
            .oauth_refresh_rejections
            .rejection(&provider, &current, mode)
            .is_some()
        {
            if forced
                && let Some(BuiltinProviderProfile::Chatgpt(profile)) =
                    self.credential_admission.admissions[index]
                        .profiles
                        .providers
                        .get_mut(&provider)
            {
                profile.auth.access_token.clear();
            }
            return;
        }
        let refresh_due = oauth_token_should_refresh(&current.access_token, current.expires_at_ms);
        if !forced && !refresh_due {
            return;
        }
        if current.refresh_token.trim().is_empty() {
            if (forced || oauth_token_is_expired(current.expires_at_ms))
                && let Some(BuiltinProviderProfile::Chatgpt(profile)) =
                    self.credential_admission.admissions[index]
                        .profiles
                        .providers
                        .get_mut(&provider)
            {
                profile.auth.access_token.clear();
            }
            return;
        }
        self.credential_admission.admissions[index].oauth_refresh = Some(key.clone());
        self.credential_admission.admissions[index].oauth_forced = forced;
        if let Some(observation) = self.credential_admission.admissions[index]
            .receipt_observation
            .as_mut()
        {
            observation.oauth_started();
        }
        let refresh_token = current.refresh_token.clone();
        self.credential_admission.oauth_refreshes.insert(
            key.clone(),
            PromptOAuthRefresh {
                current,
                forced,
                transport_finished: false,
                secret_in_flight: false,
            },
        );
        #[cfg(test)]
        if self.diagnostics.receipt.suppress_oauth_worker {
            return;
        }
        let tx = self.worker_tx.clone();
        let waker = self
            .worker_waker
            .as_ref()
            .expect("provider runtime worker waker is installed before prompt admission")
            .clone();
        let runtime = Arc::clone(&self.codex_runtime);
        thread::spawn(move || {
            let result =
                tau_provider_codex::oauth::openai_codex_refresh(&refresh_token, runtime.network());
            let _ = send_worker_message(
                &tx,
                &waker,
                WorkerMessage::PromptOAuthRefreshFinished { key, result },
            );
        });
    }

    /// Applies one failed refresh to every exact-generation waiter.
    fn fail_prompt_oauth_refresh(
        &mut self,
        key: &PromptOAuthRefreshKey,
        oauth_error: Option<&tau_provider_codex::oauth::OAuthError>,
    ) {
        let Some(refresh) = self.credential_admission.oauth_refreshes.remove(key) else {
            return;
        };
        for admission in &mut self.credential_admission.admissions {
            if admission.oauth_refresh.as_ref() != Some(key) {
                continue;
            }
            let provider = admission.kind.prompt().model.provider.clone();
            if let Some(BuiltinProviderProfile::Chatgpt(profile)) =
                admission.profiles.providers.get_mut(&provider)
            {
                profile.auth = refresh.current.clone();
                if let Some(error) = oauth_error {
                    self.oauth_refresh_rejections.record_if_permanent(
                        &provider,
                        &profile.auth,
                        profile.responses_mode(),
                        error,
                    );
                }
                if admission.oauth_forced || oauth_token_is_expired(profile.auth.expires_at_ms) {
                    profile.auth.access_token.clear();
                }
            }
            admission.oauth_refresh = None;
            if let Some(observation) = admission.receipt_observation.as_mut() {
                observation.secret_finished(0);
                observation.oauth_failed();
            }
        }
    }

    /// Adopts an authoritative OAuth record for every exact-generation waiter.
    fn complete_prompt_oauth_refresh(
        &mut self,
        key: &PromptOAuthRefreshKey,
        authoritative: OpenAiAuth,
    ) {
        self.credential_admission.oauth_refreshes.remove(key);
        for admission in &mut self.credential_admission.admissions {
            if admission.oauth_refresh.as_ref() != Some(key) {
                continue;
            }
            let provider = admission.kind.prompt().model.provider.clone();
            let mut authoritative_valid = false;
            if let Some(BuiltinProviderProfile::Chatgpt(profile)) =
                admission.profiles.providers.get_mut(&provider)
            {
                let valid = validate_authoritative_rotation(
                    &profile.auth,
                    &authoritative,
                    admission.oauth_forced,
                )
                .is_ok();
                if valid {
                    authoritative_valid = true;
                    profile.auth = authoritative.clone();
                    self.oauth_refresh_rejections
                        .clear_refresh_rejection(&provider);
                } else {
                    profile.auth.access_token.clear();
                }
            }
            admission.oauth_refresh = None;
            if let Some(observation) = admission.receipt_observation.as_mut() {
                if authoritative_valid {
                    observation.oauth_refreshed();
                } else {
                    observation.oauth_failed();
                }
            }
        }
    }

    /// Starts one finite Secret RPC and remembers its refresh continuation.
    fn start_prompt_oauth_rpc(
        &mut self,
        op: tau_proto::ExtensionDataRequestOp,
        continuation: PromptOAuthRpc,
    ) -> ClientResult<()> {
        let continuation_key = match &continuation {
            PromptOAuthRpc::CompareAndSwap { key } | PromptOAuthRpc::Reload { key } => key,
        };
        for admission in &mut self.credential_admission.admissions {
            if admission.oauth_refresh.as_ref() == Some(continuation_key)
                && let Some(observation) = admission.receipt_observation.as_mut()
            {
                observation.oauth_transport_finished();
                observation.secret_started();
            }
        }
        if let Some(refresh) = self
            .credential_admission
            .oauth_refreshes
            .get_mut(continuation_key)
        {
            refresh.secret_in_flight = true;
        }
        let client = self
            .extension_data_client
            .as_ref()
            .expect("production prompt OAuth has an extension-data client");
        let request_id = client.start_request(tau_proto::ExtensionDataScope::Secret, op)?;
        self.arm_prompt_credential_timeout(request_id.clone());
        self.credential_admission
            .oauth_rpcs
            .insert(request_id, continuation);
        Ok(())
    }

    /// Invalidates an OAuth operation after its final prompt waiter disappears.
    fn prune_unreferenced_prompt_oauth(&mut self, key: &PromptOAuthRefreshKey) {
        if prompt_oauth_has_waiter(&self.credential_admission.admissions, key) {
            return;
        }
        self.credential_admission.oauth_refreshes.remove(key);
        let mut canceled = Vec::new();
        self.credential_admission
            .oauth_rpcs
            .retain(|request_id, continuation| {
                let continuation_key = match continuation {
                    PromptOAuthRpc::CompareAndSwap { key } | PromptOAuthRpc::Reload { key } => key,
                };
                let retain = continuation_key != key;
                if !retain {
                    canceled.push(request_id.clone());
                }
                retain
            });
        if let Some(deadlines) = &self.credential_admission.deadlines {
            for request_id in canceled {
                deadlines.cancel(request_id);
            }
        }
    }

    /// Moves every ready head entry into normal prompt admission. A completed
    /// later Secret read remains behind an earlier unresolved prompt, so prompt
    /// eligibility and `ProviderPromptSubmitted` stay FIFO even when the
    /// harness replies out of order.
    fn drain_prompt_admissions(&mut self, handle: &ClientHandle) -> ClientResult<()> {
        while self
            .credential_admission
            .admissions
            .front()
            .is_some_and(|admission| {
                admission.observations.is_some() && admission.oauth_refresh.is_none()
            })
        {
            let mut admission = self
                .credential_admission
                .admissions
                .pop_front()
                .expect("ready admission is queued");
            if self.input_closed
                || self
                    .cancellation
                    .take_canceled(admission.kind.agent_prompt_id())
            {
                if let Some(observation) = admission.receipt_observation.take() {
                    observation.finished_before_worker(ReceiptOutcome::Canceled);
                }
                self.finish_canceled_admission(&admission, handle)?;
                continue;
            }
            self.finish_prompt_admission(admission, handle)?;
        }
        Ok(())
    }

    /// Applies a credential-ready transition after it reaches the FIFO
    /// admission head.
    fn finish_prompt_admission(
        &mut self,
        mut admission: PendingPromptAdmission,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        let provider = admission.kind.prompt().model.provider.clone();
        self.publish_selected_models_if_changed(
            &provider,
            &admission.profiles,
            admission.observations.clone().unwrap_or_default(),
            handle,
        )?;
        match admission.kind {
            PendingPromptAdmissionKind::Initial {
                agent_prompt_id,
                prompt,
            } => self.start_resolved_prompt(
                agent_prompt_id,
                prompt,
                &mut admission.profiles,
                admission.receipt_observation.take(),
                handle,
            ),
            PendingPromptAdmissionKind::RetryDue(mut job) => {
                let quota_started_at = admission
                    .receipt_observation
                    .as_ref()
                    .map(|_| Instant::now());
                let backend = self.resolve_admitted_backend_with_quota(
                    &job.prompt.model,
                    &mut admission.profiles,
                    handle,
                )?;
                if let (Some(observation), Some(started_at)) =
                    (admission.receipt_observation.as_mut(), quota_started_at)
                {
                    observation.quota_resolved(started_at.elapsed());
                }
                job.receipt_observation = admission.receipt_observation.take();
                if !automatic_retry_identity_matches(job.pinned_chatgpt_identity.as_ref(), &backend)
                {
                    return self.finish_identity_changed_prompt(&job, handle);
                }
                job.backend = backend;
                job.profile_identity = backend_profile_identity(&job.backend);
                if let Some(observation) = job.receipt_observation.as_mut() {
                    observation.queued(self.prompt_queue.len().saturating_add(1));
                }
                self.prompt_queue.push_back(job);
                Ok(())
            }
            PendingPromptAdmissionKind::Manual {
                mut job,
                request_id,
            } => {
                let quota_started_at = admission
                    .receipt_observation
                    .as_ref()
                    .map(|_| Instant::now());
                job.backend = self.resolve_admitted_backend_with_quota(
                    &job.prompt.model,
                    &mut admission.profiles,
                    handle,
                )?;
                if let (Some(observation), Some(started_at)) =
                    (admission.receipt_observation.as_mut(), quota_started_at)
                {
                    observation.quota_resolved(started_at.elapsed());
                }
                job.receipt_observation = admission.receipt_observation.take();
                job.profile_identity = backend_profile_identity(&job.backend);
                job.manual_cooldown_bypass = true;
                job.cooldown_probe = self
                    .shared_cooldowns
                    .get(&job.prompt.model.provider)
                    .filter(|cooldown| cooldown.not_before > self.retry_clock.now())
                    .map(|cooldown| CooldownProbe {
                        provider: job.prompt.model.provider.clone(),
                        generation: cooldown.generation,
                    });
                let agent_prompt_id = job.agent_prompt_id.clone();
                if let Some(observation) = job.receipt_observation.as_mut() {
                    observation.queued(self.prompt_queue.len().saturating_add(1));
                }
                self.prompt_queue.push_back(job);
                handle.emit_transient(Event::ProviderRetryPromptResultReported(
                    tau_proto::ProviderRetryPromptResult {
                        request_id,
                        agent_prompt_id,
                        status: tau_proto::RetryPromptStatus::Accepted,
                    },
                ))
            }
        }
    }

    fn handle_cancel_prompt(
        &mut self,
        cancel: tau_proto::UiCancelPrompt,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        self.cancel_all_prewarms();
        let Some(apid) = cancel.agent_prompt_id else {
            self.cancellation.cancel_all();
            self.cancel_generation = self.cancel_generation.saturating_add(1);
            if let Some(scheduler) = &self.retry_scheduler {
                scheduler.cancel_all();
            }
            while let Some(mut job) = self.prompt_queue.pop_front() {
                finish_receipt_canceled(&mut job.receipt_observation);
                self.finish_canceled_job(&job, handle)?;
            }
            while let Some(mut admission) = self.credential_admission.admissions.pop_front() {
                if let (Some(deadlines), Some(request_id)) = (
                    &self.credential_admission.deadlines,
                    admission.request_id.as_ref(),
                ) {
                    deadlines.cancel(request_id.clone());
                }
                finish_receipt_canceled(&mut admission.receipt_observation);
                self.finish_canceled_admission(&admission, handle)?;
            }
            self.credential_admission.oauth_refreshes.clear();
            self.credential_admission.oauth_rpcs.clear();
            if let Some(deadlines) = &self.credential_admission.deadlines {
                deadlines.cancel_all();
            }
            return Ok(());
        };
        self.cancellation.cancel(apid.clone());
        if let Some(scheduler) = &self.retry_scheduler {
            scheduler.cancel(apid.clone());
        }
        if finish_queued_canceled(&apid, &mut self.prompt_queue, handle)? {
            self.cancellation.take_canceled(&apid);
        }
        if let Some(index) = self
            .credential_admission
            .admissions
            .iter()
            .position(|admission| admission.kind.agent_prompt_id() == &apid)
            && let Some(mut admission) = self.credential_admission.admissions.remove(index)
        {
            let oauth_refresh = admission.oauth_refresh.clone();
            if let (Some(deadlines), Some(request_id)) = (
                &self.credential_admission.deadlines,
                admission.request_id.as_ref(),
            ) {
                deadlines.cancel(request_id.clone());
            }
            self.cancellation.take_canceled(&apid);
            finish_receipt_canceled(&mut admission.receipt_observation);
            self.finish_canceled_admission(&admission, handle)?;
            if let Some(key) = oauth_refresh {
                self.prune_unreferenced_prompt_oauth(&key);
            }
        }
        Ok(())
    }

    fn handle_retry_prompt(&mut self, retry: tau_proto::UiRetryPrompt) -> ClientResult<()> {
        let Some(agent_prompt_id) = retry.agent_prompt_id else {
            return Ok(());
        };
        if let Some(scheduler) = &self.retry_scheduler {
            scheduler.retry_now(retry.request_id, agent_prompt_id);
        }
        Ok(())
    }

    /// Cancels every session-owned job during final daemon shutdown.
    fn handle_session_shutdown(&mut self, handle: &ClientHandle) -> ClientResult<()> {
        self.handle_cancel_prompt(
            tau_proto::UiCancelPrompt {
                session_id: tau_proto::SessionId::parse("shutdown")
                    .expect("static session id must be valid"),
                target_agent_id: None,
                agent_prompt_id: None,
            },
            handle,
        )?;
        if let Err(error) = self.codex_runtime.invalidate_all_websockets() {
            tracing::debug!(
                target: LOG_TARGET,
                "failed to invalidate websocket pool after session shutdown: {error}",
            );
        }
        Ok(())
    }

    fn drain_worker_messages(&mut self, handle: &ClientHandle) -> ClientResult<()> {
        let mut drain_observation = WorkerDrainObservation::enabled();
        loop {
            let received = self.worker_rx.try_recv();
            if let (Ok(message), Some(observation)) = (&received, &mut drain_observation) {
                observation.message(matches!(message, WorkerMessage::Output { .. }));
            }
            match received {
                Ok(WorkerMessage::PromptOAuthRefreshFinished { key, result }) => {
                    let Some(refresh) = self.credential_admission.oauth_refreshes.get_mut(&key)
                    else {
                        continue;
                    };
                    refresh.transport_finished = true;
                    let current = refresh.current.clone();
                    for admission in &mut self.credential_admission.admissions {
                        if admission.oauth_refresh.as_ref() == Some(&key)
                            && let Some(observation) = admission.receipt_observation.as_mut()
                        {
                            observation.oauth_transport_finished();
                        }
                    }
                    match result {
                        Ok(tokens) => match merge_chatgpt_refresh(&current, tokens) {
                            Ok(refreshed) => {
                                let contents = serde_json::to_vec(
                                    &credential_record::ChatGptOAuthCredential::from(
                                        refreshed.clone(),
                                    ),
                                )
                                .map_err(|_| {
                                    ClientError::handler(
                                        "could not encode refreshed OAuth credential",
                                    )
                                })?;
                                self.start_prompt_oauth_rpc(
                                    tau_proto::ExtensionDataRequestOp::CompareAndSwapFile {
                                        path: key.path.clone(),
                                        expected_generation: key.generation.clone(),
                                        contents,
                                    },
                                    PromptOAuthRpc::CompareAndSwap { key },
                                )?;
                            }
                            Err(_) => self.fail_prompt_oauth_refresh(&key, None),
                        },
                        Err(error) => {
                            self.fail_prompt_oauth_refresh(&key, Some(&error));
                        }
                    }
                }
                Ok(WorkerMessage::PromptCredentialRpcTimedOut { request_id }) => {
                    if let Some(key) = apply_prompt_credential_timeout(
                        &request_id,
                        &mut self.credential_admission.oauth_rpcs,
                        &mut self.credential_admission.admissions,
                    ) {
                        self.fail_prompt_oauth_refresh(&key, None);
                    }
                }
                Ok(WorkerMessage::Output {
                    output,
                    output_cost,
                    cancel_generation,
                    agent_prompt_id,
                    cooldown_probe,
                }) => {
                    if let Some(observation) = output_cost {
                        observation.finish("dequeued");
                    }
                    let validated = validate_worker_output_and_probe_for_commit(
                        output,
                        (cancel_generation, self.cancel_generation, self.input_closed),
                        &agent_prompt_id,
                        &self.cancellation,
                        cooldown_probe.as_ref(),
                        &self.shared_cooldowns,
                    )?;
                    if let Some((output, released_provider)) = validated {
                        if let Some(provider) = released_provider {
                            self.release_shared_cooldown(&provider);
                        }
                        handle.send_prepared(output)?;
                    }
                }
                Ok(WorkerMessage::PromptDone) => {
                    self.active_prompts = self.active_prompts.saturating_sub(1);
                }
                Ok(WorkerMessage::CompactRouteUnavailable { identity }) => {
                    let mut profiles = self.load_all_profiles(handle)?;
                    let observes_oauth_refresh = profiles
                        .providers
                        .values()
                        .any(|profile| matches!(profile, BuiltinProviderProfile::Chatgpt(_)));
                    let provider_models = models_for_profiles(&profiles);
                    for model in provider_models {
                        if let Some(PromptBackend::Responses(config)) = resolve_prompt_backend(
                            &model.id,
                            &mut profiles,
                            &mut self.oauth_refresh_rejections,
                            self.codex_runtime.network(),
                            self.extension_data_client.as_ref(),
                        ) {
                            self.compact_profile_identities
                                .insert(model.id.provider.clone(), config.inference_identity());
                        }
                    }
                    self.observe_all_oauth_resolutions(observes_oauth_refresh, handle)?;
                    if compact_negative_identity_is_owned(
                        identity,
                        &self.compact_profile_identities,
                    ) {
                        let changed = self.unavailable_compact_identities.insert(identity);
                        if !changed {
                            continue;
                        }
                        let mut models = models_for_profiles(&profiles);
                        apply_compact_route_downgrades(
                            &mut models,
                            &self.compact_profile_identities,
                            &self.unavailable_compact_identities,
                        );
                        self.emit_model_declaration(models, handle)?;
                    } else {
                        self.codex_runtime.retire_compact_identity(identity);
                    }
                }
                Ok(WorkerMessage::PrewarmDone {
                    key,
                    generation,
                    terminal,
                }) => {
                    self.prewarm_supervisor.complete(&key, generation);
                    if let Some((refresh_id, status)) = terminal {
                        send_cache_refresh_terminal(handle, refresh_id, status)?;
                    }
                }
                Ok(WorkerMessage::Retry {
                    mut job,
                    decision,
                    live_detail,
                    canonical_unauthorized,
                    terminal_backend,
                }) => {
                    if terminal_backend.is_some() {
                        job.observed_backend = terminal_backend;
                    }
                    if self.input_closed
                        || job.cancel_generation != self.cancel_generation
                        || self.cancellation.is_canceled(&job.agent_prompt_id)
                    {
                        self.cancellation.take_canceled(&job.agent_prompt_id);
                        finish_receipt_canceled(&mut job.receipt_observation);
                        self.finish_canceled_job(&job, handle)?;
                        continue;
                    }
                    if canonical_unauthorized && let Some(identity) = job.profile_identity {
                        self.oauth_refresh_rejections
                            .record_unauthorized(job.prompt.model.provider.clone(), identity);
                    }
                    let retry_disposition = PromptRetryPolicy::for_operation(job.prompt.operation)
                        .after_failure(job.retry_state.attempts);
                    let policy_delay = job
                        .retry_state
                        .next_delay(decision.class, job.agent_prompt_id.as_str());
                    let retry_hint = scheduler_retry_hint(decision.class, decision.retry_after);
                    let hint_delay = retry_hint.unwrap_or(Duration::ZERO);
                    let hint_jitter = retry_hint.map_or(Duration::ZERO, |_| {
                        Duration::from_secs(
                            1 + stable_retry_hash(
                                job.agent_prompt_id.as_str(),
                                job.retry_state.attempts,
                            ) % RESET_BOUNDARY_JITTER_MAX.as_secs(),
                        )
                    });
                    let now = self.retry_clock.now();
                    let common_due = retry_common_due(now, policy_delay, hint_delay);
                    let provider = job.prompt.model.provider.clone();
                    let current_identity = self.provider_profile_identities.get(&provider).copied();
                    let may_share = decision.class.shares_cooldown()
                        && current_identity == Some(job.profile_identity);
                    let hinted_due = common_due.checked_add(hint_jitter).unwrap_or(common_due);
                    let independent_due = if may_share {
                        now.checked_add(policy_delay).unwrap_or(now)
                    } else {
                        hinted_due
                    };
                    let mut due = independent_due;
                    let mut cooldown_constraint = None;
                    if may_share {
                        let shared = install_shared_cooldown(
                            &mut self.shared_cooldowns,
                            &mut self.shared_cooldown_generation,
                            provider.clone(),
                            common_due,
                            decision.class,
                        );
                        self.prewarm_supervisor.cancel_provider(&provider);
                        let generation = shared.generation;
                        due = independent_due.max(cooldown_due_for_job(shared.not_before, &job));
                        cooldown_constraint = Some(CooldownConstraint {
                            generation,
                            boundary: shared.not_before,
                        });
                        self.retry_scheduler
                            .as_ref()
                            .expect("retry scheduler starts with the runtime waker")
                            .extend_cooldown(
                                job.prompt.model.provider.clone(),
                                shared.not_before,
                                generation,
                            );
                    }
                    if let PromptRetryDisposition::Terminal(attempt) = retry_disposition {
                        let mut finished = simple_finished(
                            job.agent_prompt_id,
                            job.prompt.agent_id,
                            job.prompt.originator,
                            "provider retry budget exhausted during standalone compaction",
                        );
                        finished.provider_attempt = attempt;
                        finished.backend = job.observed_backend;
                        handle.send(HarnessInputMessage::emit_transient(
                            Event::ProviderResponseFinishedReported(finished),
                        ))?;
                        continue;
                    }
                    emit_retry_status(
                        &job,
                        decision.class,
                        due,
                        now,
                        live_detail
                            .as_ref()
                            .map(tau_provider_codex::RedactedProviderDetail::as_str),
                        handle,
                    )?;
                    self.retry_scheduler
                        .as_ref()
                        .expect("retry scheduler starts with the runtime waker")
                        .schedule(job, independent_due, cooldown_constraint);
                }
                Ok(WorkerMessage::RetryDue(mut job)) => {
                    if let Some(observation) = job.receipt_observation.as_mut() {
                        observation.cooldown_dequeued();
                    }
                    if let Some(scheduler) = &self.retry_scheduler {
                        scheduler
                            .delayed_count
                            .fetch_sub(1, AtomicOrdering::Relaxed);
                    }
                    if self.input_closed
                        || job.cancel_generation != self.cancel_generation
                        || self.cancellation.take_canceled(&job.agent_prompt_id)
                    {
                        finish_receipt_canceled(&mut job.receipt_observation);
                        self.finish_canceled_job(&job, handle)?;
                        continue;
                    }
                    let Some(PendingPromptAdmissionKind::RetryDue(mut job)) = self
                        .start_retry_credential_read(PendingPromptAdmissionKind::RetryDue(job))?
                    else {
                        continue;
                    };
                    let mut profiles =
                        self.load_selected_profile(&job.prompt.model.provider, handle)?;
                    let backend =
                        self.resolve_backend_with_quota(&job.prompt.model, &mut profiles, handle)?;
                    if !automatic_retry_identity_matches(
                        job.pinned_chatgpt_identity.as_ref(),
                        &backend,
                    ) {
                        self.finish_identity_changed_prompt(&job, handle)?;
                        continue;
                    }
                    job.backend = backend;
                    job.profile_identity = backend_profile_identity(&job.backend);
                    if let Some(observation) = job.receipt_observation.as_mut() {
                        observation.queued(self.prompt_queue.len().saturating_add(1));
                    }
                    self.prompt_queue.push_back(job);
                }
                Ok(WorkerMessage::ManualRetry {
                    mut job,
                    request_id,
                    agent_prompt_id,
                }) => {
                    let status = if let Some(owned_job) = job.take() {
                        let mut owned_job = owned_job;
                        if let Some(observation) = owned_job.receipt_observation.as_mut() {
                            observation.cooldown_dequeued();
                        }
                        if let Some(scheduler) = &self.retry_scheduler {
                            scheduler
                                .delayed_count
                                .fetch_sub(1, AtomicOrdering::Relaxed);
                        }
                        if self.input_closed
                            || owned_job.cancel_generation != self.cancel_generation
                            || self.cancellation.take_canceled(&owned_job.agent_prompt_id)
                        {
                            finish_receipt_canceled(&mut owned_job.receipt_observation);
                            self.finish_canceled_job(&owned_job, handle)?;
                            tau_proto::RetryPromptStatus::NotParked
                        } else {
                            let Some(PendingPromptAdmissionKind::Manual {
                                job: mut owned_job,
                                request_id: _,
                            }) = self.start_retry_credential_read(
                                PendingPromptAdmissionKind::Manual {
                                    job: owned_job,
                                    request_id: request_id.clone(),
                                },
                            )?
                            else {
                                // The response is emitted only after this
                                // admission reaches the FIFO-ready head.
                                continue;
                            };
                            let mut profiles = self
                                .load_selected_profile(&owned_job.prompt.model.provider, handle)?;
                            owned_job.backend = self.resolve_backend_with_quota(
                                &owned_job.prompt.model,
                                &mut profiles,
                                handle,
                            )?;
                            owned_job.profile_identity =
                                backend_profile_identity(&owned_job.backend);
                            owned_job.manual_cooldown_bypass = true;
                            owned_job.cooldown_probe = self
                                .shared_cooldowns
                                .get(&owned_job.prompt.model.provider)
                                .filter(|cooldown| cooldown.not_before > self.retry_clock.now())
                                .map(|cooldown| CooldownProbe {
                                    provider: owned_job.prompt.model.provider.clone(),
                                    generation: cooldown.generation,
                                });
                            if let Some(observation) = owned_job.receipt_observation.as_mut() {
                                observation.queued(self.prompt_queue.len().saturating_add(1));
                            }
                            self.prompt_queue.push_back(owned_job);
                            tau_proto::RetryPromptStatus::Accepted
                        }
                    } else {
                        tau_proto::RetryPromptStatus::NotParked
                    };
                    let mut frame_writer = handle_report_sink(handle);
                    frame_writer.send_report(HarnessInputMessage::emit_transient(
                        Event::ProviderRetryPromptResultReported(
                            tau_proto::ProviderRetryPromptResult {
                                request_id,
                                agent_prompt_id,
                                status,
                            },
                        ),
                    ))?;
                }
                Ok(WorkerMessage::DelayedCanceled {
                    mut job,
                    delayed_count,
                }) => {
                    if let Some(scheduler) = &self.retry_scheduler {
                        scheduler
                            .delayed_count
                            .fetch_sub(delayed_count, AtomicOrdering::Relaxed);
                    }
                    self.cancellation.take_canceled(&job.agent_prompt_id);
                    finish_receipt_canceled(&mut job.receipt_observation);
                    self.finish_canceled_job(&job, handle)?;
                }
                Ok(WorkerMessage::QuotaRolling {
                    model,
                    profile_identity,
                    observation,
                    observed_at_unix_ms,
                }) => {
                    if let Some(event) = self.quota.merge_rolling(
                        model,
                        profile_identity,
                        observation,
                        observed_at_unix_ms,
                    ) {
                        handle.send(quota_report_message(event))?;
                    }
                }
                Ok(WorkerMessage::QuotaFetchFinished {
                    provider,
                    profile_epoch,
                    fetch_start_sequence,
                    observed_at_unix_ms,
                    result,
                }) => match result {
                    Ok(snapshot) => {
                        let refresh_provider = provider.clone();
                        let refresh_epoch = profile_epoch.clone();
                        if let Some(event) = self.quota.finish_fetch(
                            provider,
                            profile_epoch,
                            fetch_start_sequence,
                            snapshot,
                            observed_at_unix_ms,
                        ) {
                            handle.send(quota_report_message(event))?;
                            let delay = self.quota.refresh_delay(&refresh_provider);
                            self.schedule_quota_refresh(refresh_provider, refresh_epoch, delay);
                        } else if self.quota.epoch_matches(&refresh_provider, &refresh_epoch) {
                            let delay = self.quota.failure_delay(&refresh_provider);
                            self.schedule_quota_refresh(refresh_provider, refresh_epoch, delay);
                        }
                    }
                    Err(error) => {
                        self.quota.fail_fetch(&provider, &profile_epoch);
                        tracing::debug!(
                            target: LOG_TARGET,
                            provider = %provider,
                            "quota reconciliation unavailable: {error}"
                        );
                        if self.quota.epoch_matches(&provider, &profile_epoch) {
                            let delay = self.quota.failure_delay(&provider);
                            self.schedule_quota_refresh(provider, profile_epoch, delay);
                        }
                    }
                },
                Ok(WorkerMessage::QuotaRefreshDue {
                    provider,
                    profile_epoch,
                    refresh_generation,
                }) => {
                    if self
                        .quota
                        .refresh_is_current(&provider, &profile_epoch, refresh_generation)
                    {
                        let mut profiles = self.load_selected_profile(&provider, handle)?;
                        let observes_oauth_refresh =
                            profiles.chatgpt_credential_reference(&provider).is_some();
                        let config = models_for_profiles(&profiles)
                            .into_iter()
                            .find(|model| model.id.provider == provider)
                            .and_then(|model| {
                                resolve_responses_backend(
                                    &model.id,
                                    &mut profiles,
                                    &mut self.oauth_refresh_rejections,
                                    self.codex_runtime.network(),
                                    self.extension_data_client.as_ref(),
                                )
                            });
                        self.observe_selected_oauth_resolution(
                            &provider,
                            observes_oauth_refresh,
                            handle,
                        )?;
                        if let Some(config) = config {
                            let started = self.ensure_quota_profile(&provider, &config, handle)?;
                            if !started && let Some(epoch) = self.quota.profile_epoch(&provider) {
                                self.schedule_quota_refresh(
                                    provider,
                                    epoch,
                                    QUOTA_FETCH_MIN_INTERVAL,
                                );
                            }
                        } else if let Some(event) = self.quota.clear_profile(&provider) {
                            self.clear_prewarm_profile(&provider);
                            handle.send(quota_report_message(event))?;
                        } else {
                            self.clear_prewarm_profile(&provider);
                        }
                    }
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return Ok(()),
            }
        }
    }

    fn park_cooled_queued_prompts(&mut self, handle: &ClientHandle) -> ClientResult<()> {
        let scheduler = self
            .retry_scheduler
            .as_ref()
            .expect("retry scheduler starts with the runtime waker");
        reconcile_cooled_queued_prompts(
            &mut self.prompt_queue,
            &*self.retry_clock,
            &self.shared_cooldowns,
            #[cfg(test)]
            None,
            |mut job, now, cooldown| {
                let due = cooldown_due_for_job(cooldown.not_before, &job);
                emit_retry_status(&job, cooldown.class, due, now, None, handle)?;
                if let Some(observation) = job.receipt_observation.as_mut() {
                    observation.slot_dequeued();
                    observation.cooldown_queued(
                        scheduler
                            .delayed_count
                            .load(AtomicOrdering::Relaxed)
                            .saturating_add(1),
                    );
                }
                scheduler.schedule(
                    job,
                    now,
                    Some(CooldownConstraint {
                        generation: cooldown.generation,
                        boundary: cooldown.not_before,
                    }),
                );
                Ok(())
            },
        )
    }

    fn prompt_worker_context(&self) -> PromptWorkerContext {
        PromptWorkerContext {
            worker_tx: self.worker_tx.clone(),
            worker_waker: self
                .worker_waker
                .as_ref()
                .expect("provider runtime worker waker is installed before dispatch")
                .clone(),
            worker_output_depth: self.diagnostics.output_queue.clone(),
            prompt_executor: self.prompt_executor.clone(),
            cancellation: self.cancellation.clone(),
            codex_runtime: self.codex_runtime.clone(),
        }
    }
}

/// Builds a worker context only when queued work can consume an available slot.
fn with_queued_prompt_start_capacity<T>(
    active_prompts: usize,
    prompt_concurrency_limit: usize,
    queued_prompts: usize,
    build_context: impl FnOnce() -> T,
) -> Option<T> {
    (active_prompts < prompt_concurrency_limit && queued_prompts != 0).then(build_context)
}

/// Reports whether one exact-generation refresh still has a prompt owner.
fn prompt_oauth_has_waiter(
    admissions: &VecDeque<PendingPromptAdmission>,
    key: &PromptOAuthRefreshKey,
) -> bool {
    admissions
        .iter()
        .any(|admission| admission.oauth_refresh.as_ref() == Some(key))
}

/// Loads the selected profile at the due/manual transition rather than reusing
/// the generation captured by the previous attempt.
fn load_fresh_retry_profiles<F>(
    load_prompt_profiles: &mut F,
    startup_responses_modes: &BTreeMap<ProviderName, CodexMode>,
    provider: &ProviderName,
) -> BuiltinProviderProfiles
where
    F: FnMut(Option<&ProviderName>) -> BuiltinProviderProfiles,
{
    let mut profiles = load_prompt_profiles(Some(provider));
    profiles.apply_startup_responses_modes(startup_responses_modes);
    profiles
}

/// Removes at most one provider from the startup quota snapshot. The reactive
/// loop regains control between calls, so newly arrived prompt input is drained
/// before the next provider's best-effort quota work.
fn take_next_initial_quota_profile(
    startup_profiles: &mut Option<BuiltinProviderProfiles>,
) -> Option<BuiltinProviderProfiles> {
    let profiles = startup_profiles.as_mut()?;
    let Some(provider) = profiles.providers.keys().next().cloned() else {
        *startup_profiles = None;
        return None;
    };
    let selected = profiles.selected(&provider);
    profiles.providers.remove(&provider);
    profiles.credentials.remove(&provider);
    profiles.missing_logins.remove(&provider);
    if profiles.providers.is_empty() {
        *startup_profiles = None;
    }
    Some(selected)
}

/// Invalidates one expired Secret correlation. OAuth continuations return their
/// shared key to the caller; initial/retry reads become credential-missing and
/// ready. Repeated or late notifications are no-ops.
fn apply_prompt_credential_timeout(
    request_id: &str,
    oauth_rpcs: &mut HashMap<String, PromptOAuthRpc>,
    admissions: &mut VecDeque<PendingPromptAdmission>,
) -> Option<PromptOAuthRefreshKey> {
    if let Some(continuation) = oauth_rpcs.remove(request_id) {
        return Some(match continuation {
            PromptOAuthRpc::CompareAndSwap { key } | PromptOAuthRpc::Reload { key } => key,
        });
    }
    if let Some(admission) = admissions.iter_mut().find(|admission| {
        admission.request_id.as_deref() == Some(request_id) && admission.observations.is_none()
    }) {
        if let Some(observation) = admission.receipt_observation.as_mut() {
            observation.secret_finished(0);
        }
        let observations = hydrate_profile_credentials_with(&mut admission.profiles, |_| {
            Err(tau_client::ExtensionDataRpcError::Timeout)
        });
        admission.observations = Some(observations);
        admission.request_id = None;
    }
    None
}

/// Both a successful CAS and a generation-mismatch loser must reload the
/// authoritative Secret record before any prompt adopts refreshed OAuth.
fn prompt_oauth_cas_requires_reload(result: &tau_proto::ExtensionDataResultPayload) -> bool {
    matches!(
        result,
        tau_proto::ExtensionDataResultPayload::Ok {
            value: tau_proto::ExtensionDataValue::CompareAndSwapFile,
        } | tau_proto::ExtensionDataResultPayload::Error {
            kind: tau_proto::ExtensionDataErrorKind::GenerationMismatch,
            ..
        }
    )
}

/// Drops stale compact evidence without restoring aliases that share an
/// identity.
fn reconcile_compact_state_after_credential_changes(
    previous: &BTreeMap<ProviderName, CredentialObservation>,
    current: &BTreeMap<ProviderName, CredentialObservation>,
    identities: &mut HashMap<ProviderName, InferenceProfileIdentity>,
    unavailable: &mut HashSet<InferenceProfileIdentity>,
) -> Vec<InferenceProfileIdentity> {
    let mut superseded_negative_identities = Vec::new();
    for provider in previous.keys().chain(current.keys()) {
        if previous.get(provider) != current.get(provider)
            && let Some(identity) = identities.remove(provider)
            && !identities.values().any(|current| current == &identity)
        {
            unavailable.remove(&identity);
            superseded_negative_identities.push(identity);
        }
    }
    superseded_negative_identities
}

/// Returns whether a route-negative result still belongs to any current
/// provider alias sharing its inference identity.
fn compact_negative_identity_is_owned(
    identity: InferenceProfileIdentity,
    identities: &HashMap<ProviderName, InferenceProfileIdentity>,
) -> bool {
    identities.values().any(|current| current == &identity)
}

/// Returns whether current local route or credential evidence replaces the last
/// declaration.
fn declaration_needs_publication(
    declared_models: Option<&Vec<ProviderModelInfo>>,
    declared_observations: Option<&BTreeMap<ProviderName, CredentialObservation>>,
    models: &[ProviderModelInfo],
    observations: &BTreeMap<ProviderName, CredentialObservation>,
) -> bool {
    declared_models.is_none_or(|declared| declared.as_slice() != models)
        || declared_observations != Some(observations)
}

/// Runs the authoritative post-resolution observation only for OAuth-backed
/// routes.
fn observe_oauth_resolution_with(
    observes_oauth_refresh: bool,
    rehydrate_and_publish: impl FnOnce() -> ClientResult<()>,
) -> ClientResult<()> {
    if observes_oauth_refresh {
        rehydrate_and_publish()?;
    }
    Ok(())
}

/// Reconciles one material inference identity and removes only that provider's
/// obsolete shared cooldown when the identity changes or disappears.
fn reconcile_inference_identity(
    identities: &mut BTreeMap<ProviderName, Option<BackendProfileIdentity>>,
    cooldowns: &mut BTreeMap<ProviderName, SharedCooldown>,
    provider: &ProviderName,
    identity: Option<BackendProfileIdentity>,
) -> (bool, Option<SharedCooldown>) {
    let changed = identities
        .insert(provider.clone(), identity)
        .is_some_and(|previous| previous != identity);
    let removed = changed.then(|| cooldowns.remove(provider)).flatten();
    (changed, removed)
}

/// Computes the common retry boundary, falling back to generated class cadence
/// when an otherwise valid trusted hint exceeds the monotonic clock range.
fn retry_common_due(now: Instant, policy_delay: Duration, hint_delay: Duration) -> Instant {
    now.checked_add(policy_delay.max(hint_delay))
        .unwrap_or_else(|| now.checked_add(policy_delay).unwrap_or(now))
}

/// Keeps usage-window reset estimates informational so early provider or user
/// recovery is discovered through the bounded persistent-failure cadence.
fn scheduler_retry_hint(class: RetryClass, retry_after: Option<Duration>) -> Option<Duration> {
    match class {
        RetryClass::UsageWindow => None,
        RetryClass::Transport
        | RetryClass::Overload
        | RetryClass::Throttle
        | RetryClass::Account
        | RetryClass::Auth
        | RetryClass::Unknown => retry_after,
    }
}

/// Installs newer shared evidence without allowing it to shorten an existing
/// provider boundary.
fn install_shared_cooldown(
    cooldowns: &mut BTreeMap<ProviderName, SharedCooldown>,
    next_generation: &mut u64,
    provider: ProviderName,
    common_due: Instant,
    class: RetryClass,
) -> SharedCooldown {
    *next_generation = next_generation
        .checked_add(1)
        .expect("shared cooldown generation exhausted");
    let generation = *next_generation;
    let shared = cooldowns.entry(provider).or_insert(SharedCooldown {
        not_before: common_due,
        class,
        generation,
    });
    shared.generation = generation;
    if shared.not_before < common_due {
        shared.not_before = common_due;
        shared.class = class;
    }
    *shared
}

/// Revalidates queued worker output at the main-loop serialization boundary.
///
/// Targeted/global cancellation may race after transport output is enqueued.
/// Tentative output is dropped, while a queued successful terminal is replaced
/// with exactly one canceled terminal and consumes the targeted marker.
fn validate_worker_output_for_commit(
    output: tau_client::PeerOutput,
    dispatch_generation: u64,
    current_generation: u64,
    input_closed: bool,
    agent_prompt_id: &tau_proto::AgentPromptId,
    cancellation: &CancellationState,
) -> ClientResult<Option<tau_client::PeerOutput>> {
    let targeted = cancellation.is_canceled(agent_prompt_id);
    if dispatch_generation == current_generation && !input_closed && !targeted {
        return Ok(Some(output));
    }
    if is_intentional_partial_clear_for(output.message(), agent_prompt_id) {
        return Ok(Some(output));
    }
    let HarnessInputMessage::Emit(emit) = output.message() else {
        return Ok(None);
    };
    let Event::ProviderResponseFinishedReported(finished) = emit.event.as_ref() else {
        return Ok(None);
    };
    cancellation.take_canceled(agent_prompt_id);
    let mut canceled = simple_finished(
        finished.agent_prompt_id.clone(),
        finished.agent_id.clone(),
        finished.originator.clone(),
        "(cancelled by harness)",
    );
    canceled.backend = finished.backend.clone();
    Ok(Some(tau_client::PeerOutput::prepare(
        HarnessInputMessage::emit_transient(Event::ProviderResponseFinishedReported(canceled)),
    )?))
}

fn is_intentional_partial_clear_for(
    message: &HarnessInputMessage,
    agent_prompt_id: &tau_proto::AgentPromptId,
) -> bool {
    matches!(
        message,
        HarnessInputMessage::Emit(emit)
            if matches!(
                emit.event.as_ref(),
                Event::ProviderResponseUpdatedReported(update)
                    if update.agent_prompt_id == *agent_prompt_id
                        && update.status.as_ref().is_some_and(|status| status.clear_response)
            )
    )
}

/// Validates cancellation before deriving any successful-probe release action.
fn validate_worker_output_and_probe_for_commit(
    output: tau_client::PeerOutput,
    commit_state: (u64, u64, bool),
    agent_prompt_id: &tau_proto::AgentPromptId,
    cancellation: &CancellationState,
    probe: Option<&CooldownProbe>,
    cooldowns: &BTreeMap<ProviderName, SharedCooldown>,
) -> ClientResult<Option<(tau_client::PeerOutput, Option<ProviderName>)>> {
    let (dispatch_generation, current_generation, input_closed) = commit_state;
    let output = validate_worker_output_for_commit(
        output,
        dispatch_generation,
        current_generation,
        input_closed,
        agent_prompt_id,
        cancellation,
    )?;
    let Some(output) = output else {
        return Ok(None);
    };
    let released_provider = probe
        .filter(|probe| {
            successful_probe_matches(output.message(), agent_prompt_id, probe, cooldowns)
        })
        .map(|probe| probe.provider.clone());
    Ok(Some((output, released_provider)))
}

/// Returns whether a committed frame authoritatively proves provider success.
fn is_successful_terminal_for(
    message: &HarnessInputMessage,
    agent_prompt_id: &tau_proto::AgentPromptId,
) -> bool {
    let HarnessInputMessage::Emit(emit) = message else {
        return false;
    };
    let Event::ProviderResponseFinishedReported(finished) = emit.event.as_ref() else {
        return false;
    };
    finished.agent_prompt_id == *agent_prompt_id
        && finished.error.is_none()
        && finished.failure_kind.is_none()
        && matches!(
            finished.stop_reason,
            ProviderStopReason::EndTurn
                | ProviderStopReason::ToolCalls
                | ProviderStopReason::Length
        )
}

/// Checks that a successful terminal belongs to the still-current cooldown
/// generation captured by its manually admitted attempt.
fn successful_probe_matches(
    message: &HarnessInputMessage,
    agent_prompt_id: &tau_proto::AgentPromptId,
    probe: &CooldownProbe,
    cooldowns: &BTreeMap<ProviderName, SharedCooldown>,
) -> bool {
    is_successful_terminal_for(message, agent_prompt_id)
        && cooldowns
            .get(&probe.provider)
            .is_some_and(|cooldown| cooldown.generation == probe.generation)
}

type PromptExecutor = Arc<dyn Fn(PromptExecution) + Send + Sync + 'static>;

/// Owned inputs for one finite prewarm worker attempt.
struct PrewarmExecution {
    /// Shared ChatGPT runtime and connection pool.
    runtime: Arc<CodexRuntime>,
    /// Resolved immutable backend configuration.
    config: ResolvedConfig,
    /// Owned prefix request received from the harness.
    request: tau_proto::AgentPromptPrewarmRequested,
    /// Whether this durable session permits provider request captures.
    debug_provider_requests: bool,
    /// Supervisor-owned cancellation source.
    abort: PrewarmAbort,
}

/// Injected finite prewarm attempt used by production and runtime tests.
type PrewarmExecutor =
    Arc<dyn Fn(PrewarmExecution) -> tau_proto::ProviderCacheRefreshStatus + Send + Sync + 'static>;

struct PromptJob {
    agent_prompt_id: tau_proto::AgentPromptId,
    debug_provider_requests: bool,
    prompt: tau_proto::AgentPromptCreated,
    backend: PromptBackend,
    /// Immutable account anchor for every automatic retry of this owned prompt.
    pinned_chatgpt_identity: Option<ChatGptRetryIdentity>,
    /// Inference profile identity used by the next finite attempt.
    profile_identity: Option<BackendProfileIdentity>,
    retry_state: PromptRetryState,
    /// Backend reached by any finite attempt in this logical provider turn.
    observed_backend: Option<ProviderBackend>,
    /// Runtime global-cancel generation at logical prompt creation.
    cancel_generation: u64,
    /// Lets one manually released job pass a still-active shared cooldown once.
    manual_cooldown_bypass: bool,
    /// Shared cooldown generation this job was manually admitted to probe.
    cooldown_probe: Option<CooldownProbe>,
    /// Enabled-only content-free receipt observation.
    receipt_observation: Option<ReceiptObservation>,
}

/// One prompt held at the credential boundary before it is eligible for
/// `ProviderPromptSubmitted`.
///
/// The main loop owns this state.  In particular, a Secret reply only makes an
/// entry ready; [`ProviderRuntime::drain_prompt_admissions`] preserves FIFO
/// submission when replies arrive out of order.
struct PendingPromptAdmission {
    /// The logical prompt transition that owns this credential read.
    kind: PendingPromptAdmissionKind,
    /// The selected credential-free profile plus any hydrated credential.
    profiles: BuiltinProviderProfiles,
    /// Correlation identifier returned by `ExtensionDataClient::start_request`,
    /// or `None` for an immediately ready keyless admission.
    request_id: Option<String>,
    /// Credential generation observations returned with the selected Secret
    /// read.
    observations: Option<BTreeMap<ProviderName, CredentialObservation>>,
    /// Exact OAuth refresh generation awaited before this admission is ready.
    oauth_refresh: Option<PromptOAuthRefreshKey>,
    /// Whether this admission is recovering one canonical unauthorized result.
    oauth_forced: bool,
    /// Enabled-only content-free receipt observation.
    receipt_observation: Option<ReceiptObservation>,
}

/// Provider, startup mode, and exact Secret generation used to coalesce OAuth
/// refresh.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct PromptOAuthRefreshKey {
    /// Provider namespace whose rejection and identity state owns this refresh.
    provider: ProviderName,
    /// Opaque Secret-scope path; never rendered.
    path: tau_proto::ExtensionDataPath,
    /// BLAKE3 generation expected by Secret compare-and-swap.
    generation: String,
    /// Startup-selected Responses Lite mode, retained without making
    /// `CodexMode` part of the hash key.
    lite_compatibility: bool,
}

/// One shared network refresh and its main-loop-owned Secret publication state.
struct PromptOAuthRefresh {
    /// Credential generation supplied to the OAuth endpoint.
    current: OpenAiAuth,
    /// Whether this flight consumed forced recovery authority for the
    /// generation.
    forced: bool,
    /// Whether network OAuth ended and Secret publication has begun.
    transport_finished: bool,
    /// Whether a shared Secret CAS or authoritative reload is in flight.
    secret_in_flight: bool,
}

/// Main-loop continuation for one prompt-owned Secret operation.
enum PromptOAuthRpc {
    /// Publish the refreshed credential, then verify the authoritative record.
    CompareAndSwap {
        /// Exact refresh operation.
        key: PromptOAuthRefreshKey,
    },
    /// Adopt the authoritative record after CAS success or a losing CAS.
    Reload {
        /// Exact refresh operation.
        key: PromptOAuthRefreshKey,
    },
}

/// Prompt ownership returned after an asynchronous credential read.
enum PendingPromptAdmissionKind {
    /// Initial prompt admission has not emitted its submitted marker.
    Initial {
        /// Prompt identity used by cancellation and Secret-result correlation.
        agent_prompt_id: tau_proto::AgentPromptId,
        /// Fully materialized prompt retained while the Secret read is pending.
        prompt: tau_proto::AgentPromptCreated,
    },
    /// A scheduler-owned automatic retry whose credential must be reloaded.
    RetryDue(PromptJob),
    /// A scheduler-owned manual retry awaiting its correlated UI result.
    Manual {
        /// Owned job released by the retry scheduler.
        job: PromptJob,
        /// UI request correlation retained until admission completes.
        request_id: tau_proto::RetryPromptRequestId,
    },
}

impl PendingPromptAdmissionKind {
    /// Moves the enabled-only receipt observation into admission ownership.
    fn take_receipt_observation(&mut self) -> Option<ReceiptObservation> {
        match self {
            Self::Initial { .. } => None,
            Self::RetryDue(job) | Self::Manual { job, .. } => job.receipt_observation.take(),
        }
    }

    /// Returns the prompt identity protected by this admission.
    fn agent_prompt_id(&self) -> &tau_proto::AgentPromptId {
        match self {
            Self::Initial {
                agent_prompt_id, ..
            } => agent_prompt_id,
            Self::RetryDue(job) | Self::Manual { job, .. } => &job.agent_prompt_id,
        }
    }

    /// Returns the prompt retained by this admission.
    fn prompt(&self) -> &tau_proto::AgentPromptCreated {
        match self {
            Self::Initial { prompt, .. } => prompt,
            Self::RetryDue(job) | Self::Manual { job, .. } => &job.prompt,
        }
    }
}

/// Shared provider-profile lower bound and its visible normalized reason.
#[derive(Clone, Copy, Debug)]
struct SharedCooldown {
    /// Common earliest provider-contact instant before prompt-local jitter.
    not_before: Instant,
    /// Failure class that established the current lower bound.
    class: RetryClass,
    /// Monotonic evidence generation used to reject stale successful probes.
    generation: u64,
}

/// Exact shared cooldown generation a manually admitted attempt may invalidate.
#[derive(Clone, Debug)]
struct CooldownProbe {
    /// Provider profile whose cooldown was bypassed.
    provider: ProviderName,
    /// Cooldown generation current when the scheduler transferred the job.
    generation: u64,
}

/// One immutable clock and cooldown view used while reconciling queued prompts.
struct QueuedPromptReconciliation<'a> {
    /// Single monotonic observation shared by this complete FIFO pass.
    now: Instant,
    /// Current provider-scoped cooldown evidence, owned by the main loop.
    cooldowns: &'a BTreeMap<ProviderName, SharedCooldown>,
}

impl<'a> QueuedPromptReconciliation<'a> {
    /// Samples time once before the caller starts walking the FIFO queue.
    fn from_clock(
        clock: &dyn RetryClock,
        cooldowns: &'a BTreeMap<ProviderName, SharedCooldown>,
    ) -> Self {
        Self {
            now: clock.now(),
            cooldowns,
        }
    }

    /// Classifies one FIFO job against this pass's coherent cooldown snapshot.
    /// Manual retries retain their one-shot bypass regardless of evidence.
    fn active_cooldown_for(&mut self, job: &PromptJob) -> Option<SharedCooldown> {
        (!job.manual_cooldown_bypass)
            .then(|| self.cooldowns.get(&job.prompt.model.provider).copied())
            .flatten()
            .filter(|cooldown| cooldown.not_before > self.now)
    }
}

/// Test-only counters collected by the production queued-prompt reconciliation
/// seam.
#[cfg(test)]
#[derive(Debug, Default, Eq, PartialEq)]
struct QueuedPromptReconciliationMetrics {
    /// Number of coherent clock snapshots taken.
    clock_samples: usize,
    /// Number of FIFO jobs classified against that snapshot.
    classifications: usize,
    /// Number of jobs transferred to delayed ownership.
    parked: usize,
}

/// Partitions FIFO prompt work using one coherent cooldown snapshot, retaining
/// immediately eligible jobs and transferring active cooldowns through `park`.
fn reconcile_cooled_queued_prompts(
    prompt_queue: &mut VecDeque<PromptJob>,
    clock: &dyn RetryClock,
    cooldowns: &BTreeMap<ProviderName, SharedCooldown>,
    #[cfg(test)] mut metrics: Option<&mut QueuedPromptReconciliationMetrics>,
    mut park: impl FnMut(PromptJob, Instant, SharedCooldown) -> ClientResult<()>,
) -> ClientResult<()> {
    if prompt_queue.is_empty() {
        return Ok(());
    }
    // One pass owns one monotonic snapshot. Jobs at a shared-cooldown boundary
    // therefore all make the same before/at/after decision.
    let mut reconciliation = QueuedPromptReconciliation::from_clock(clock, cooldowns);
    #[cfg(test)]
    if let Some(metrics) = &mut metrics {
        metrics.clock_samples += 1;
    }
    let mut pending = std::mem::take(prompt_queue);
    let mut retained = VecDeque::with_capacity(pending.len());
    while let Some(job) = pending.pop_front() {
        let cooldown = reconciliation.active_cooldown_for(&job);
        #[cfg(test)]
        if let Some(metrics) = &mut metrics {
            metrics.classifications += 1;
        }
        let Some(cooldown) = cooldown else {
            retained.push_back(job);
            continue;
        };
        if let Err(error) = park(job, reconciliation.now, cooldown) {
            retained.append(&mut pending);
            *prompt_queue = retained;
            return Err(error);
        }
        #[cfg(test)]
        if let Some(metrics) = &mut metrics {
            metrics.parked += 1;
        }
    }
    *prompt_queue = retained;
    Ok(())
}

fn cooldown_due_for_job(not_before: Instant, job: &PromptJob) -> Instant {
    let jitter = cooldown_jitter(
        job.agent_prompt_id.as_str(),
        job.retry_state.attempts.saturating_add(1),
    );
    not_before.checked_add(jitter).unwrap_or(not_before)
}

fn cooldown_jitter(prompt_id: &str, attempt: u64) -> Duration {
    let max_millis: u64 = RESET_BOUNDARY_JITTER_MAX
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    Duration::from_millis(1 + stable_retry_hash(prompt_id, attempt) % max_millis)
}

/// Saturating Fibonacci state retained with a logical prompt across attempts.
#[derive(Clone, Debug, Default)]
struct PromptRetryState {
    /// Number of failed provider attempts observed so far.
    attempts: u64,
    /// Previous Fibonacci value in milliseconds.
    previous: u64,
    /// Current Fibonacci value in milliseconds.
    current: u64,
}

impl PromptRetryState {
    fn next_delay(&mut self, class: RetryClass, prompt_id: &str) -> Duration {
        self.attempts = self.attempts.saturating_add(1);
        let base_millis: u64 = RETRY_BASE_DELAY.as_millis().try_into().unwrap_or(u64::MAX);
        let fibonacci = if self.current == 0 {
            self.previous = base_millis;
            self.current = base_millis;
            self.current
        } else {
            let value = self.previous;
            let next = self.previous.saturating_add(self.current);
            self.previous = self.current;
            self.current = next;
            value
        };
        let ceiling: u64 = class
            .generated_delay_ceiling()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        let base_ceiling = ceiling.saturating_mul(5) / 6;
        let base = fibonacci.min(base_ceiling).max(base_millis);
        let jitter_range = (base / 5).max(1);
        let jitter = stable_retry_hash(prompt_id, self.attempts) % (jitter_range + 1);
        Duration::from_millis(base.saturating_add(jitter).min(ceiling))
    }
}

/// Retry-attempt authority applied before the shared scheduler accepts delayed
/// provider work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PromptRetryPolicy {
    /// Ordinary inference retains the existing deliberately unbounded policy.
    UnboundedInference,
    /// Standalone compaction terminates after five total provider attempts.
    FiveAttemptStandaloneCompaction,
}

/// Closed scheduling result after one provider attempt fails transiently.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PromptRetryDisposition {
    /// The shared scheduler retains the logical prompt for another attempt.
    Retry,
    /// The logical prompt terminalizes at this finite one-based attempt.
    Terminal(tau_proto::ProviderAttempt),
}

/// Finite total-attempt limit for standalone compaction.
const STANDALONE_COMPACTION_ATTEMPT_LIMIT: tau_proto::ProviderAttempt =
    match tau_proto::ProviderAttempt::new(5) {
        Some(attempt) => attempt,
        None => panic!("standalone compaction attempt limit must be nonzero"),
    };

impl PromptRetryPolicy {
    /// Selects retry authority from the immutable prompt operation.
    fn for_operation(operation: tau_proto::PromptOperation) -> Self {
        match operation {
            tau_proto::PromptOperation::Inference => Self::UnboundedInference,
            tau_proto::PromptOperation::StandaloneCompaction => {
                Self::FiveAttemptStandaloneCompaction
            }
        }
    }

    /// Returns the complete scheduling disposition after one more failure.
    fn after_failure(self, previous_failures: u64) -> PromptRetryDisposition {
        match self {
            Self::UnboundedInference => PromptRetryDisposition::Retry,
            Self::FiveAttemptStandaloneCompaction
                if previous_failures.saturating_add(1)
                    >= u64::from(STANDALONE_COMPACTION_ATTEMPT_LIMIT.get()) =>
            {
                PromptRetryDisposition::Terminal(STANDALONE_COMPACTION_ATTEMPT_LIMIT)
            }
            Self::FiveAttemptStandaloneCompaction => PromptRetryDisposition::Retry,
        }
    }
}

fn stable_retry_hash(prompt_id: &str, attempt: u64) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64 ^ attempt;
    for byte in prompt_id.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

struct ScheduledPrompt {
    due: Instant,
    /// Prompt-local eligibility before any shared provider cooldown.
    independent_due: Instant,
    /// Shared cooldown generation currently constraining this entry.
    cooldown_generation: Option<u64>,
    sequence: u64,
    job: PromptJob,
}

/// One shared provider cooldown currently constraining a scheduled prompt.
#[derive(Clone, Copy)]
struct CooldownConstraint {
    /// Exact cooldown evidence generation.
    generation: u64,
    /// Common provider-contact boundary before prompt-local jitter.
    boundary: Instant,
}

/// Deterministic delayed-prompt queue owned by the single retry scheduler.
///
/// Time is supplied to [`Self::pop_due`] by the caller so scheduling and
/// cooldown behavior can be acceptance-tested without wall-clock sleeps.
/// See `SPEC-tau-ext-provider-builtin-retry-scheduler`.
#[derive(Default)]
struct RetryScheduleQueue {
    /// Min-heap of delayed logical prompts.
    prompts: BinaryHeap<ScheduledPrompt>,
    /// Exact logical-prompt membership mirrored by every heap mutation.
    prompt_ids: HashSet<tau_proto::AgentPromptId>,
    /// Stable FIFO tie-breaker for equal deadlines.
    sequence: u64,
    /// Number of entries visited by bulk queue mutations.
    #[cfg(test)]
    mutation_work: usize,
}

impl RetryScheduleQueue {
    /// Adds one logical prompt at its current eligible deadline.
    fn schedule(
        &mut self,
        independent_due: Instant,
        cooldown: Option<CooldownConstraint>,
        job: PromptJob,
    ) -> Result<(), Box<PromptJob>> {
        if !self.prompt_ids.insert(job.agent_prompt_id.clone()) {
            return Err(Box::new(job));
        }
        let due = cooldown.map_or(independent_due, |constraint| {
            independent_due.max(cooldown_due_for_job(constraint.boundary, &job))
        });
        self.sequence = self.sequence.saturating_add(1);
        self.prompts.push(ScheduledPrompt {
            due,
            independent_due,
            cooldown_generation: cooldown.map(|constraint| constraint.generation),
            sequence: self.sequence,
            job,
        });
        Ok(())
    }

    /// Removes and returns the next prompt when its deadline has arrived.
    fn pop_due(&mut self, now: Instant) -> Option<PromptJob> {
        if self
            .prompts
            .peek()
            .is_none_or(|scheduled| scheduled.due > now)
        {
            return None;
        }
        self.prompts.pop().map(|scheduled| {
            let removed = self.prompt_ids.remove(&scheduled.job.agent_prompt_id);
            assert!(removed, "heap membership must have an exact prompt ID");
            scheduled.job
        })
    }

    /// Returns the earliest deadline, if any.
    fn next_due(&self) -> Option<Instant> {
        self.prompts.peek().map(|scheduled| scheduled.due)
    }

    /// Removes all delayed instances of one logical prompt.
    fn cancel(&mut self, prompt_id: &tau_proto::AgentPromptId) -> Vec<PromptJob> {
        if !self.prompt_ids.contains(prompt_id) {
            return Vec::new();
        }
        self.remove_matching(|scheduled| scheduled.job.agent_prompt_id == *prompt_id)
    }

    /// Removes every delayed logical prompt.
    fn cancel_all(&mut self) -> Vec<PromptJob> {
        self.prompt_ids.clear();
        self.prompts
            .drain()
            .map(|scheduled| scheduled.job)
            .collect()
    }

    /// Monotonically moves same-provider prompts beyond a shared cooldown.
    fn extend_cooldown(&mut self, provider: &ProviderName, due: Instant, generation: u64) {
        let mut prompts = std::mem::take(&mut self.prompts).into_vec();
        #[cfg(test)]
        {
            self.mutation_work += prompts.len();
        }
        for scheduled in &mut prompts {
            if scheduled.job.prompt.model.provider == *provider {
                if scheduled.cooldown_generation.is_none() {
                    scheduled.independent_due = scheduled.due;
                }
                scheduled.cooldown_generation = Some(generation);
                scheduled.due = scheduled
                    .independent_due
                    .max(cooldown_due_for_job(due, &scheduled.job));
            }
        }
        self.prompts = BinaryHeap::from(prompts);
    }

    /// Advances only matching provider prompts after an authoritative probe.
    fn release_cooldown(&mut self, provider: &ProviderName, generation: u64, now: Instant) {
        let mut prompts = std::mem::take(&mut self.prompts).into_vec();
        #[cfg(test)]
        {
            self.mutation_work += prompts.len();
        }
        for scheduled in &mut prompts {
            if scheduled.job.prompt.model.provider == *provider
                && scheduled.cooldown_generation == Some(generation)
            {
                scheduled.cooldown_generation = None;
                scheduled.due = scheduled
                    .independent_due
                    .max(cooldown_due_for_job(now, &scheduled.job));
            }
        }
        self.prompts = BinaryHeap::from(prompts);
    }

    /// Number of logical prompts currently parked outside the worker pool.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.prompts.len()
    }

    /// Snapshot of prompt IDs and deadlines for deterministic acceptance tests.
    #[cfg(test)]
    fn deadlines(&self) -> Vec<(tau_proto::AgentPromptId, ProviderName, Instant)> {
        self.prompts
            .iter()
            .map(|scheduled| {
                (
                    scheduled.job.agent_prompt_id.clone(),
                    scheduled.job.prompt.model.provider.clone(),
                    scheduled.due,
                )
            })
            .collect()
    }

    /// Reports whether the heap and exact prompt-ID index describe one set.
    #[cfg(test)]
    fn membership_is_exact(&self) -> bool {
        self.prompts.len() == self.prompt_ids.len()
            && self
                .prompts
                .iter()
                .all(|scheduled| self.prompt_ids.contains(&scheduled.job.agent_prompt_id))
    }

    /// Partitions entries matching a scheduler command, then heapifies once.
    fn remove_matching(
        &mut self,
        mut predicate: impl FnMut(&ScheduledPrompt) -> bool,
    ) -> Vec<PromptJob> {
        let mut removed = Vec::new();
        let prompts = std::mem::take(&mut self.prompts).into_vec();
        let mut retained = Vec::with_capacity(prompts.len());
        #[cfg(test)]
        {
            self.mutation_work += prompts.len();
        }
        for scheduled in prompts {
            if predicate(&scheduled) {
                let present = self.prompt_ids.remove(&scheduled.job.agent_prompt_id);
                assert!(present, "heap membership must have an exact prompt ID");
                removed.push(scheduled.job);
            } else {
                retained.push(scheduled);
            }
        }
        self.prompts = BinaryHeap::from(retained);
        removed
    }

    /// Returns and resets the number of entries visited by bulk mutations.
    #[cfg(test)]
    fn take_mutation_work(&mut self) -> usize {
        std::mem::take(&mut self.mutation_work)
    }
}

impl PartialEq for ScheduledPrompt {
    fn eq(&self, other: &Self) -> bool {
        self.due == other.due && self.sequence == other.sequence
    }
}

impl Eq for ScheduledPrompt {}

impl PartialOrd for ScheduledPrompt {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScheduledPrompt {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .due
            .cmp(&self.due)
            .then_with(|| other.sequence.cmp(&self.sequence))
    }
}

enum SchedulerCommand {
    Schedule {
        /// Prompt-local eligibility before shared provider policy.
        independent_due: Instant,
        /// Optional shared provider constraint applied at insertion.
        cooldown: Option<CooldownConstraint>,
        job: Box<PromptJob>,
    },
    Cancel(tau_proto::AgentPromptId),
    CancelAll,
    RetryNow {
        request_id: tau_proto::RetryPromptRequestId,
        agent_prompt_id: tau_proto::AgentPromptId,
    },
    ExtendCooldown {
        provider: ProviderName,
        due: Instant,
        /// New shared-cooldown evidence generation.
        generation: u64,
    },
    /// Removes one exact shared-cooldown generation from matching entries.
    ReleaseCooldown {
        /// Provider namespace whose parked work may be released.
        provider: ProviderName,
        /// Exact shared-cooldown generation being invalidated.
        generation: u64,
        /// Release instant used as the anti-herd jitter boundary.
        now: Instant,
    },
    /// Interrupts a timer wait after an injected clock advances.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "constructed by injected virtual clocks")
    )]
    Wake {
        /// Acknowledges that the actor observed the new clock value.
        acknowledged: Option<SyncSender<()>>,
    },
}

/// One actor command tagged at mailbox admission so timer ordering does not
/// depend on when the scheduler thread happens to run.
struct SchedulerMailboxCommand {
    /// Serialized logical-admission publication timestamp.
    admitted_at: Arc<SchedulerAdmissionTimestamp>,
    /// Atomic scheduler mutation to apply.
    command: SchedulerCommand,
}

impl SchedulerMailboxCommand {
    /// Resolves transport-owned admission publication before synchronous state
    /// evaluates timer precedence.
    fn resolve(self) -> (Instant, SchedulerCommand) {
        (self.admitted_at.wait(), self.command)
    }
}

/// Clone-shared bounded mailbox sender with one coherent FIFO admission order.
struct SchedulerCommandSender {
    /// Bounded actor mailbox transport.
    commands: SyncSender<SchedulerMailboxCommand>,
    /// Clock sampled only after a bounded send succeeds.
    clock: Arc<dyn RetryClock>,
    /// Serializes send completion and publication across every producer.
    admission: Mutex<()>,
    /// Test hook fired when gate acquisition observes another producer.
    #[cfg(test)]
    gate_blocked: Mutex<Option<SyncSender<()>>>,
    /// Test hook fired at the bounded-send call boundary.
    #[cfg(test)]
    send_attempted: Mutex<Option<SyncSender<()>>>,
    /// Test hook fired after bounded admission reports a full mailbox.
    #[cfg(test)]
    send_blocked: Mutex<Option<SyncSender<()>>>,
}

impl SchedulerCommandSender {
    /// Sends and timestamps one command while holding the shared admission
    /// gate.
    fn send(
        &self,
        command: SchedulerCommand,
    ) -> Result<(), mpsc::SendError<SchedulerMailboxCommand>> {
        #[cfg(test)]
        let _admission = if let Some(blocked) = self
            .gate_blocked
            .lock()
            .expect("scheduler gate-blocked hook")
            .take()
        {
            match self.admission.try_lock() {
                Ok(admission) => admission,
                Err(TryLockError::WouldBlock) => {
                    let _ = blocked.send(());
                    self.admission.lock().expect("scheduler admission gate")
                }
                Err(TryLockError::Poisoned(error)) => error.into_inner(),
            }
        } else {
            self.admission.lock().expect("scheduler admission gate")
        };
        #[cfg(not(test))]
        let _admission = self.admission.lock().expect("scheduler admission gate");
        #[cfg(test)]
        if let Some(attempted) = self
            .send_attempted
            .lock()
            .expect("scheduler send-attempt hook")
            .take()
        {
            let _ = attempted.send(());
        }
        let admitted_at = Arc::new(SchedulerAdmissionTimestamp::pending());
        let mailbox = SchedulerMailboxCommand {
            admitted_at: Arc::clone(&admitted_at),
            command,
        };
        #[cfg(test)]
        let result = if let Some(blocked) = self
            .send_blocked
            .lock()
            .expect("scheduler send-blocked hook")
            .take()
        {
            match self.commands.try_send(mailbox) {
                Ok(()) => Ok(()),
                Err(mpsc::TrySendError::Full(mailbox)) => {
                    let _ = blocked.send(());
                    self.commands.send(mailbox)
                }
                Err(mpsc::TrySendError::Disconnected(mailbox)) => Err(mpsc::SendError(mailbox)),
            }
        } else {
            self.commands.send(mailbox)
        };
        #[cfg(not(test))]
        let result = self.commands.send(mailbox);
        if result.is_ok() {
            admitted_at.publish(self.clock.now());
        }
        result
    }
}

/// Logical admission timestamp published after a bounded send succeeds.
struct SchedulerAdmissionTimestamp {
    /// Timestamp value, absent while a full-mailbox sender remains blocked.
    value: Mutex<Option<Instant>>,
    /// Wakes an actor that received the command as its sender unblocked.
    ready: Condvar,
    /// Test hook fired immediately before waiting on an unpublished timestamp.
    #[cfg(test)]
    pending_observed: Mutex<Option<SyncSender<()>>>,
}

impl SchedulerAdmissionTimestamp {
    /// Creates an unpublished admission timestamp.
    fn pending() -> Self {
        Self {
            value: Mutex::new(None),
            ready: Condvar::new(),
            #[cfg(test)]
            pending_observed: Mutex::new(None),
        }
    }

    /// Publishes logical admission at the producer's current clock instant.
    fn publish(&self, now: Instant) {
        *self.value.lock().expect("scheduler admission timestamp") = Some(now);
        self.ready.notify_one();
    }

    /// Waits until the successful sender publishes its admission instant.
    fn wait(&self) -> Instant {
        let mut value = self.value.lock().expect("scheduler admission timestamp");
        while value.is_none() {
            #[cfg(test)]
            if let Some(observed) = self
                .pending_observed
                .lock()
                .expect("scheduler pending-observed hook")
                .take()
            {
                let _ = observed.send(());
            }
            value = self
                .ready
                .wait(value)
                .expect("scheduler admission timestamp wait");
        }
        value.expect("scheduler admission timestamp is published")
    }
}

/// Monotonic retry clock, injectable so long quota windows need no wall wait.
trait RetryClock: Send + Sync {
    /// Returns the current monotonic scheduler instant.
    fn now(&self) -> Instant;

    /// Receives the actor command channel for virtual-time wakeups.
    fn attach_scheduler(&self, _commands: std::sync::Weak<SchedulerCommandSender>) {}
}

/// Production retry clock backed by the process monotonic clock.
struct SystemRetryClock;

impl RetryClock for SystemRetryClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// One deterministic output produced by synchronous scheduler mutation.
enum RetrySchedulerAction {
    /// A delayed job became eligible.
    Due(PromptJob),
    /// A delayed job was canceled, with the ownership-count adjustment.
    Canceled {
        /// Logical job whose terminal cancellation must be emitted.
        job: PromptJob,
        /// Number of delayed ownership entries consumed.
        delayed_count: usize,
    },
    /// Result of an exact manual ownership transfer.
    Manual {
        /// Transferred job, or `None` when it was not parked.
        job: Option<PromptJob>,
        /// Correlation ID for the control request.
        request_id: tau_proto::RetryPromptRequestId,
        /// Requested logical prompt.
        agent_prompt_id: tau_proto::AgentPromptId,
    },
}

/// Synchronous single-owner retry scheduler state.
///
/// The actor is only transport and waiting: every mutation and resulting
/// ownership action happens here and is directly testable at a supplied time.
#[derive(Default)]
struct RetrySchedulerState {
    /// Delayed logical-prompt queue.
    queue: RetryScheduleQueue,
}

impl RetrySchedulerState {
    /// Applies one command atomically and returns all immediate ownership
    /// actions.
    fn step(&mut self, command: SchedulerCommand) -> Vec<RetrySchedulerAction> {
        match command {
            SchedulerCommand::Schedule {
                independent_due,
                cooldown,
                job,
            } => {
                if let Err(duplicate) = self.queue.schedule(independent_due, cooldown, *job)
                    && let Some(original) = self.queue.cancel(&duplicate.agent_prompt_id).pop()
                {
                    return vec![RetrySchedulerAction::Canceled {
                        job: original,
                        delayed_count: 2,
                    }];
                }
                Vec::new()
            }
            SchedulerCommand::Cancel(prompt_id) => self
                .queue
                .cancel(&prompt_id)
                .into_iter()
                .map(|job| RetrySchedulerAction::Canceled {
                    job,
                    delayed_count: 1,
                })
                .collect(),
            SchedulerCommand::CancelAll => self
                .queue
                .cancel_all()
                .into_iter()
                .map(|job| RetrySchedulerAction::Canceled {
                    job,
                    delayed_count: 1,
                })
                .collect(),
            SchedulerCommand::RetryNow {
                request_id,
                agent_prompt_id,
            } => {
                let mut matches = self.queue.cancel(&agent_prompt_id);
                if matches.len() > 1 {
                    let action = matches.pop().map(|job| RetrySchedulerAction::Canceled {
                        job,
                        delayed_count: matches.len() + 1,
                    });
                    return action
                        .into_iter()
                        .chain(std::iter::once(RetrySchedulerAction::Manual {
                            job: None,
                            request_id,
                            agent_prompt_id,
                        }))
                        .collect();
                }
                vec![RetrySchedulerAction::Manual {
                    job: matches.pop(),
                    request_id,
                    agent_prompt_id,
                }]
            }
            SchedulerCommand::ExtendCooldown {
                provider,
                due,
                generation,
            } => {
                self.queue.extend_cooldown(&provider, due, generation);
                Vec::new()
            }
            SchedulerCommand::ReleaseCooldown {
                provider,
                generation,
                now,
            } => {
                self.queue.release_cooldown(&provider, generation, now);
                Vec::new()
            }
            SchedulerCommand::Wake { .. } => Vec::new(),
        }
    }

    /// Orders one timestamped mailbox command against the current timer
    /// boundary, independent of when the actor thread processes it.
    fn step_mailbox(
        &mut self,
        admitted_at: Instant,
        command: SchedulerCommand,
    ) -> (Vec<RetrySchedulerAction>, Option<SyncSender<()>>) {
        let (wake, acknowledged) = match &command {
            SchedulerCommand::Wake { acknowledged } => (true, acknowledged.clone()),
            _ => (false, None),
        };
        let mut actions = if !wake
            && self
                .next_due()
                .is_some_and(|deadline| deadline <= admitted_at)
        {
            self.advance(admitted_at)
        } else {
            Vec::new()
        };
        actions.extend(self.step(command));
        (actions, acknowledged)
    }

    /// Advances supplied virtual time and returns every newly eligible job.
    fn advance(&mut self, now: Instant) -> Vec<RetrySchedulerAction> {
        std::iter::from_fn(|| self.queue.pop_due(now))
            .map(RetrySchedulerAction::Due)
            .collect()
    }

    /// Returns the next timer boundary.
    fn next_due(&self) -> Option<Instant> {
        self.queue.next_due()
    }
}

/// One correlated RPC deadline retained by the credential timer actor.
#[derive(Eq, PartialEq)]
struct PromptCredentialDeadline {
    /// Absolute monotonic deadline.
    due: Instant,
    /// Stable tie-breaker for requests armed at the same instant.
    sequence: u64,
    /// Extension-data correlation invalidated at the deadline.
    request_id: String,
}

impl Ord for PromptCredentialDeadline {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .due
            .cmp(&self.due)
            .then_with(|| other.sequence.cmp(&self.sequence))
    }
}

impl PartialOrd for PromptCredentialDeadline {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Bounded commands accepted by the prompt credential deadline actor.
enum PromptCredentialDeadlineCommand {
    /// Arms one already-issued extension-data request.
    Schedule {
        /// Correlation invalidated when `due` is reached.
        request_id: String,
        /// Absolute monotonic deadline.
        due: Instant,
    },
    /// Invalidates one deadline after a reply or prompt cancellation.
    Cancel(String),
    /// Invalidates every outstanding deadline during session/process shutdown.
    CancelAll,
}

/// Pure min-heap state for prompt credential deadlines.
#[derive(Default)]
struct PromptCredentialDeadlineQueue {
    /// Deadlines ordered earliest first.
    deadlines: BinaryHeap<PromptCredentialDeadline>,
    /// Requests that still own a live deadline.
    active: HashSet<String>,
    /// Stable deadline tie-breaker.
    sequence: u64,
}

impl PromptCredentialDeadlineQueue {
    /// Applies one scheduler command.
    fn apply(&mut self, command: PromptCredentialDeadlineCommand) {
        match command {
            PromptCredentialDeadlineCommand::Schedule { request_id, due } => {
                self.active.insert(request_id.clone());
                self.deadlines.push(PromptCredentialDeadline {
                    due,
                    sequence: self.sequence,
                    request_id,
                });
                self.sequence = self.sequence.wrapping_add(1);
            }
            PromptCredentialDeadlineCommand::Cancel(request_id) => {
                self.active.remove(&request_id);
            }
            PromptCredentialDeadlineCommand::CancelAll => self.active.clear(),
        }
        self.discard_invalid();
    }

    /// Returns all live request ids due at `now`.
    fn pop_due(&mut self, now: Instant) -> Vec<String> {
        self.discard_invalid();
        let mut due = Vec::new();
        while self
            .deadlines
            .peek()
            .is_some_and(|deadline| deadline.due <= now)
        {
            let deadline = self
                .deadlines
                .pop()
                .expect("peeked credential deadline remains present");
            if self.active.remove(&deadline.request_id) {
                due.push(deadline.request_id);
            }
            self.discard_invalid();
        }
        due
    }

    /// Returns the earliest still-live deadline.
    fn next_due(&mut self) -> Option<Instant> {
        self.discard_invalid();
        self.deadlines.peek().map(|deadline| deadline.due)
    }

    /// Removes lazily canceled entries from the heap head.
    fn discard_invalid(&mut self) {
        while self
            .deadlines
            .peek()
            .is_some_and(|deadline| !self.active.contains(&deadline.request_id))
        {
            self.deadlines.pop();
        }
    }
}

/// Owned bounded actor that replaces one sleeping thread per credential RPC.
struct PromptCredentialDeadlineScheduler {
    /// Last command sender; dropping it disconnects the actor.
    commands: Option<SyncSender<PromptCredentialDeadlineCommand>>,
    /// Joinable actor thread.
    actor: Option<thread::JoinHandle<()>>,
}

impl PromptCredentialDeadlineScheduler {
    /// Starts the one process-local credential deadline actor.
    fn start(worker_tx: Sender<WorkerMessage>, worker_waker: ManualRuntimeWaker) -> Self {
        let (commands, receiver) = mpsc::sync_channel(PROMPT_CREDENTIAL_DEADLINE_MAILBOX_CAPACITY);
        let actor = thread::spawn(move || {
            run_prompt_credential_deadline_scheduler(receiver, worker_tx, worker_waker);
        });
        Self {
            commands: Some(commands),
            actor: Some(actor),
        }
    }

    /// Arms one request's absolute deadline.
    fn schedule(&self, request_id: String, due: Instant) {
        let _ = self
            .commands
            .as_ref()
            .expect("live deadline scheduler owns its command sender")
            .send(PromptCredentialDeadlineCommand::Schedule { request_id, due });
    }

    /// Lazily invalidates one request deadline.
    fn cancel(&self, request_id: String) {
        let _ = self
            .commands
            .as_ref()
            .expect("live deadline scheduler owns its command sender")
            .send(PromptCredentialDeadlineCommand::Cancel(request_id));
    }

    /// Invalidates all request deadlines.
    fn cancel_all(&self) {
        let _ = self
            .commands
            .as_ref()
            .expect("live deadline scheduler owns its command sender")
            .send(PromptCredentialDeadlineCommand::CancelAll);
    }
}

impl Drop for PromptCredentialDeadlineScheduler {
    fn drop(&mut self) {
        self.commands.take();
        if let Some(actor) = self.actor.take() {
            let _ = actor.join();
        }
    }
}

/// Runs the prompt credential min-heap and emits only live expired ids.
fn run_prompt_credential_deadline_scheduler(
    commands: Receiver<PromptCredentialDeadlineCommand>,
    worker_tx: Sender<WorkerMessage>,
    worker_waker: ManualRuntimeWaker,
) {
    let mut queue = PromptCredentialDeadlineQueue::default();
    loop {
        let command = match queue.next_due() {
            Some(due) => commands.recv_timeout(
                due.checked_duration_since(Instant::now())
                    .unwrap_or(Duration::ZERO),
            ),
            None => commands
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected),
        };
        match command {
            Ok(command) => queue.apply(command),
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                for request_id in queue.pop_due(Instant::now()) {
                    if send_worker_message(
                        &worker_tx,
                        &worker_waker,
                        WorkerMessage::PromptCredentialRpcTimedOut { request_id },
                    )
                    .is_err()
                    {
                        return;
                    }
                }
            }
        }
    }
}

/// RAII owner of the delayed-work scheduler actor.
///
/// Dropping the last strong command sender disconnects the actor, then joins
/// its thread. `delayed_count` tracks jobs owned by either the bounded command
/// channel or synchronous scheduler state until the provider consumes an
/// action.
struct RetryScheduler {
    /// Last strong sender whose drop terminates the scheduler actor.
    commands: Arc<SchedulerCommandSender>,
    /// Count of logical prompts currently owned outside the provider main loop.
    delayed_count: Arc<AtomicUsize>,
    /// Joinable actor thread; dropping the scheduler disconnects and joins it.
    actor: Option<thread::JoinHandle<()>>,
}

impl RetryScheduler {
    fn start(
        worker_tx: Sender<WorkerMessage>,
        worker_waker: ManualRuntimeWaker,
        clock: Arc<dyn RetryClock>,
    ) -> Self {
        // Bound scheduler admission independently of the parked-job heap. The
        // harness already caps outstanding manual controls, and backpressure
        // here also covers internal schedule/cancel/cooldown producers.
        let (command_tx, receiver) = mpsc::sync_channel(RETRY_SCHEDULER_MAILBOX_CAPACITY);
        let commands = Arc::new(SchedulerCommandSender {
            commands: command_tx,
            clock: Arc::clone(&clock),
            admission: Mutex::new(()),
            #[cfg(test)]
            gate_blocked: Mutex::new(None),
            #[cfg(test)]
            send_attempted: Mutex::new(None),
            #[cfg(test)]
            send_blocked: Mutex::new(None),
        });
        clock.attach_scheduler(Arc::downgrade(&commands));
        let delayed_count = Arc::new(AtomicUsize::new(0));
        let actor_clock = Arc::clone(&clock);
        let actor = thread::spawn(move || {
            run_retry_scheduler(receiver, worker_tx, worker_waker, actor_clock);
        });
        Self {
            commands,
            delayed_count,
            actor: Some(actor),
        }
    }

    fn schedule(
        &self,
        job: PromptJob,
        independent_due: Instant,
        cooldown: Option<CooldownConstraint>,
    ) {
        self.delayed_count.fetch_add(1, AtomicOrdering::Relaxed);
        if self
            .send(SchedulerCommand::Schedule {
                independent_due,
                cooldown,
                job: Box::new(job),
            })
            .is_err()
        {
            self.delayed_count.fetch_sub(1, AtomicOrdering::Relaxed);
        }
    }

    fn cancel(&self, prompt_id: tau_proto::AgentPromptId) {
        let _ = self.send(SchedulerCommand::Cancel(prompt_id));
    }

    /// Requests cancellation of every delayed retry job owned by the scheduler.
    fn cancel_all(&self) {
        let _ = self.send(SchedulerCommand::CancelAll);
    }

    fn retry_now(
        &self,
        request_id: tau_proto::RetryPromptRequestId,
        agent_prompt_id: tau_proto::AgentPromptId,
    ) {
        let _ = self.send(SchedulerCommand::RetryNow {
            request_id,
            agent_prompt_id,
        });
    }

    fn extend_cooldown(&self, provider: ProviderName, due: Instant, generation: u64) {
        let _ = self.send(SchedulerCommand::ExtendCooldown {
            provider,
            due,
            generation,
        });
    }

    fn release_cooldown(&self, provider: ProviderName, generation: u64, now: Instant) {
        let _ = self.send(SchedulerCommand::ReleaseCooldown {
            provider,
            generation,
            now,
        });
    }

    fn is_empty(&self) -> bool {
        self.delayed_count.load(AtomicOrdering::Relaxed) == 0
    }

    /// Admits one timestamped command to the scheduler actor.
    fn send(
        &self,
        command: SchedulerCommand,
    ) -> Result<(), mpsc::SendError<SchedulerMailboxCommand>> {
        self.commands.send(command)
    }
}

fn run_retry_scheduler(
    commands: Receiver<SchedulerMailboxCommand>,
    worker_tx: Sender<WorkerMessage>,
    worker_waker: ManualRuntimeWaker,
    clock: Arc<dyn RetryClock>,
) {
    let mut state = RetrySchedulerState::default();
    loop {
        let command = match state.next_due() {
            Some(next_due) => commands.recv_timeout(
                next_due
                    .checked_duration_since(clock.now())
                    .unwrap_or(Duration::ZERO),
            ),
            None => commands
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected),
        };
        match command {
            Ok(mailbox) => {
                let (admitted_at, command) = mailbox.resolve();
                let (actions, acknowledged) = state.step_mailbox(admitted_at, command);
                if !send_scheduler_actions(actions, &worker_tx, &worker_waker) {
                    return;
                }
                if let Some(acknowledged) = acknowledged {
                    // A virtual-time wake is a barrier: all jobs made due by
                    // the new instant reach the provider worker channel first.
                    if !send_scheduler_actions(
                        state.advance(clock.now()),
                        &worker_tx,
                        &worker_waker,
                    ) {
                        return;
                    }
                    let _ = acknowledged.send(());
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if !send_scheduler_actions(state.advance(clock.now()), &worker_tx, &worker_waker) {
                    return;
                }
            }
        }
    }
}

impl Drop for RetryScheduler {
    fn drop(&mut self) {
        // Disconnect the actor before joining; virtual clocks retain only Weak.
        let (commands, _) = mpsc::sync_channel(0);
        self.commands = Arc::new(SchedulerCommandSender {
            commands,
            clock: Arc::new(SystemRetryClock),
            admission: Mutex::new(()),
            #[cfg(test)]
            gate_blocked: Mutex::new(None),
            #[cfg(test)]
            send_attempted: Mutex::new(None),
            #[cfg(test)]
            send_blocked: Mutex::new(None),
        });
        if let Some(actor) = self.actor.take() {
            let _ = actor.join();
        }
    }
}

/// Delivers pure scheduler actions to the provider actor.
fn send_scheduler_actions(
    actions: Vec<RetrySchedulerAction>,
    worker_tx: &Sender<WorkerMessage>,
    worker_waker: &ManualRuntimeWaker,
) -> bool {
    actions.into_iter().all(|action| {
        let message = match action {
            RetrySchedulerAction::Due(job) => WorkerMessage::RetryDue(job),
            RetrySchedulerAction::Canceled { job, delayed_count } => {
                WorkerMessage::DelayedCanceled { job, delayed_count }
            }
            RetrySchedulerAction::Manual {
                job,
                request_id,
                agent_prompt_id,
            } => WorkerMessage::ManualRetry {
                job,
                request_id,
                agent_prompt_id,
            },
        };
        send_worker_message(worker_tx, worker_waker, message).is_ok()
    })
}

#[derive(Clone)]
enum PromptBackend {
    /// Mutable provider profile/model state was unavailable at this attempt.
    Unavailable {
        /// Selected provider whose missing OAuth Secret requires a fresh login.
        login_required: Option<ProviderName>,
    },
    Responses(ResolvedConfig),
    ChatCompletions {
        /// One shared immutable route snapshot, including its model catalog.
        provider: Arc<ChatCompletionsProvider>,
        /// Stable position of the selected model in the route snapshot.
        model_index: usize,
    },
    /// Generic public Responses API request using profile-selected SSE or
    /// WebSocket.
    PublicResponses {
        /// One shared immutable route snapshot, including its model catalog.
        provider: Arc<ResponsesProvider>,
        /// Stable position of the selected model in the route snapshot.
        model_index: usize,
    },
}

struct PromptExecution {
    job: PromptJob,
    /// Cooldown generation this exact finite attempt may invalidate.
    cooldown_probe: Option<CooldownProbe>,
    output_tx: Sender<WorkerMessage>,
    output_waker: ManualRuntimeWaker,
    /// Shared typed-output queue depth for private observations.
    worker_output_depth: Option<Arc<WorkerQueueState>>,
    cancellation: Arc<CancellationState>,
    codex_runtime: Arc<CodexRuntime>,
}

struct PromptWorkerContext {
    worker_tx: Sender<WorkerMessage>,
    worker_waker: ManualRuntimeWaker,
    /// Shared typed-output queue depth for private observations.
    worker_output_depth: Option<Arc<WorkerQueueState>>,
    prompt_executor: PromptExecutor,
    cancellation: Arc<CancellationState>,
    codex_runtime: Arc<CodexRuntime>,
}

impl PromptExecution {
    fn frame_writer(&self) -> WorkerReportSink {
        WorkerReportSink {
            tx: self.output_tx.clone(),
            waker: self.output_waker.clone(),
            worker_output_depth: self.worker_output_depth.clone(),
            cancel_generation: self.job.cancel_generation,
            agent_prompt_id: self.job.agent_prompt_id.clone(),
            cooldown_probe: self.cooldown_probe.clone(),
        }
    }
}

enum WorkerMessage {
    /// OAuth network result for one exact prompt credential generation.
    PromptOAuthRefreshFinished {
        /// Refresh generation shared by its current waiters.
        key: PromptOAuthRefreshKey,
        /// Provider-typed OAuth result, kept inside the provider process.
        result: Result<
            tau_provider_codex::oauth::OAuthTokenRefresh,
            tau_provider_codex::oauth::OAuthError,
        >,
    },
    /// One correlated prompt credential RPC exceeded its finite deadline.
    PromptCredentialRpcTimedOut {
        /// Request correlation to invalidate.
        request_id: String,
    },
    /// One measured typed provider frame awaiting main-loop arbitration and
    /// client-writer serialization.
    Output {
        output: tau_client::PeerOutput,
        /// Enabled-only process-local queue observation.
        output_cost: Option<WorkerOutputObservation>,
        cancel_generation: u64,
        agent_prompt_id: tau_proto::AgentPromptId,
        /// Cooldown generation carried by the manually admitted attempt.
        cooldown_probe: Option<CooldownProbe>,
    },
    /// Marker that one prompt worker finished and freed a concurrency slot.
    PromptDone,
    /// A canonical v2 unsupported code removed compaction for this generation.
    CompactRouteUnavailable {
        /// Exact resolved profile generation that observed the rejection.
        identity: InferenceProfileIdentity,
    },
    /// Exact supervised prewarm worker completion.
    PrewarmDone {
        /// Cache owner whose work finished.
        key: PrewarmKey,
        /// Generation captured when the main loop admitted the work.
        generation: u64,
        /// Scheduler terminal emitted only for lifecycle-aware refreshes.
        terminal: Option<(
            tau_proto::ProviderCacheRefreshId,
            tau_proto::ProviderCacheRefreshStatus,
        )>,
    },
    /// Retryable attempt outcome returned with the still-pending logical
    /// prompt.
    Retry {
        /// Logical prompt state to park outside the worker pool.
        job: PromptJob,
        /// Structured cadence and hint decision.
        decision: RetryDecision,
        /// Bounded, redacted provider detail for ordinary live status only.
        live_detail: Option<tau_provider_codex::RedactedProviderDetail>,
        /// Whether an exact canonical 401 authorizes forced credential
        /// recovery.
        canonical_unauthorized: bool,
        /// Backend reached by the failed attempt, retained if finite retry
        /// policy synthesizes a terminal.
        terminal_backend: Option<ProviderBackend>,
    },
    /// A delayed logical prompt whose retry deadline has arrived.
    RetryDue(PromptJob),
    /// Result and optional owned job from an atomic manual scheduler release.
    ManualRetry {
        /// Parked job, or `None` when the timer/another command won.
        job: Option<PromptJob>,
        /// Request correlation identifier.
        request_id: tau_proto::RetryPromptRequestId,
        /// Exact prompt checked by the scheduler.
        agent_prompt_id: tau_proto::AgentPromptId,
    },
    /// A delayed prompt removed by targeted or global cancellation.
    DelayedCanceled {
        /// One representative owner used to emit exactly one terminal result.
        job: PromptJob,
        /// Number of scheduler entries removed for delayed-count
        /// reconciliation.
        delayed_count: usize,
    },
    /// Sparse quota observation from a supported prompt transport.
    QuotaRolling {
        /// Exact model whose in-band route established applicability.
        model: ModelId,
        /// Secret-free hash of the profile used by this prompt.
        profile_identity: QuotaProfileIdentity,
        /// Provider-normalized sparse rolling observation.
        observation: tau_provider_codex::RollingQuotaObservation,
        /// Original wall-clock observation time.
        observed_at_unix_ms: UnixMillis,
    },
    /// Result of one coalesced full account-usage fetch.
    QuotaFetchFinished {
        /// Provider profile fetched.
        provider: ProviderName,
        /// Epoch captured before starting I/O.
        profile_epoch: tau_proto::ProviderQuotaEpoch,
        /// State sequence captured before starting I/O.
        fetch_start_sequence: tau_proto::ProviderQuotaSequence,
        /// Wall-clock completion time sampled by the acquisition worker.
        observed_at_unix_ms: UnixMillis,
        /// Sanitized full-fetch result.
        result: Result<tau_provider_codex::FullQuotaSnapshot, tau_provider_codex::UsageFetchError>,
    },
    /// Coarse full-refresh wake for a still-current profile epoch.
    QuotaRefreshDue {
        /// Provider profile to refresh.
        provider: ProviderName,
        /// Epoch that scheduled this wake.
        profile_epoch: tau_proto::ProviderQuotaEpoch,
        /// Coalescing generation; only the latest deadline may act.
        refresh_generation: u64,
    },
}

/// Direct typed report destination for provider main-loop helpers.
struct HandleReportSink<'a> {
    /// Client handle that owns admission, ordering, and wire publication.
    handle: &'a ClientHandle,
}

impl ProviderReportSink for HandleReportSink<'_> {
    fn send_report(&mut self, message: HarnessInputMessage) -> ClientResult<()> {
        self.handle.send(message)?;
        Ok(())
    }
}

fn handle_report_sink(handle: &ClientHandle) -> HandleReportSink<'_> {
    HandleReportSink { handle }
}

#[derive(Default)]
struct CancellationState {
    inner: Mutex<CancellationInner>,
    changed: Condvar,
}

#[derive(Default)]
struct CancellationInner {
    canceled_apids: HashSet<tau_proto::AgentPromptId>,
    abort_wakers: HashMap<tau_proto::AgentPromptId, Vec<AbortWakerEntry>>,
    next_abort_waker_id: u64,
    retry_cancel_generation: u64,
    shutdown: bool,
}

#[derive(Clone)]
struct AbortWakerEntry {
    id: u64,
    waker: Arc<dyn Fn() + Send + Sync + 'static>,
}

impl CancellationState {
    fn cancel(&self, apid: tau_proto::AgentPromptId) {
        let wakers = if let Ok(mut inner) = self.inner.lock() {
            inner.canceled_apids.insert(apid.clone());
            inner.abort_wakers.get(&apid).cloned().unwrap_or_default()
        } else {
            Vec::new()
        };
        for waker in wakers {
            (waker.waker)();
        }
        self.changed.notify_all();
    }

    /// Advances broadcast cancellation and wakes every currently registered
    /// backend.
    ///
    /// Registration compares its captured generation under this same mutex, so
    /// either this snapshot contains a waker or a later registration invokes it
    /// immediately. Callbacks run after unlocking to permit safe reentrancy.
    fn cancel_all(&self) {
        let wakers = if let Ok(mut inner) = self.inner.lock() {
            inner.retry_cancel_generation = inner.retry_cancel_generation.saturating_add(1);
            inner
                .abort_wakers
                .values()
                .flat_map(|entries| entries.iter().cloned())
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        for waker in wakers {
            (waker.waker)();
        }
        self.changed.notify_all();
    }

    fn shutdown(&self) {
        let wakers = if let Ok(mut inner) = self.inner.lock() {
            inner.shutdown = true;
            inner
                .abort_wakers
                .values()
                .flat_map(|entries| entries.iter().cloned())
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        for waker in wakers {
            (waker.waker)();
        }
        self.changed.notify_all();
    }

    fn take_canceled(&self, apid: &tau_proto::AgentPromptId) -> bool {
        self.inner
            .lock()
            .map(|mut inner| inner.canceled_apids.remove(apid) || inner.shutdown)
            .unwrap_or(true)
    }

    fn is_canceled(&self, apid: &tau_proto::AgentPromptId) -> bool {
        self.inner
            .lock()
            .map(|inner| inner.shutdown || inner.canceled_apids.contains(apid))
            .unwrap_or(true)
    }

    fn retry_generation(&self) -> u64 {
        self.inner
            .lock()
            .map(|inner| inner.retry_cancel_generation)
            .unwrap_or(u64::MAX)
    }

    fn register_abort_waker(
        self: &Arc<Self>,
        current_apid: &tau_proto::AgentPromptId,
        cancel_generation: u64,
        waker: Arc<dyn Fn() + Send + Sync + 'static>,
    ) -> CancellationAbortWaker {
        let (id, call_now) = if let Ok(mut inner) = self.inner.lock() {
            let id = inner.next_abort_waker_id;
            inner.next_abort_waker_id = inner.next_abort_waker_id.saturating_add(1);
            let call_now = inner.shutdown
                || inner.retry_cancel_generation != cancel_generation
                || inner.canceled_apids.iter().any(|apid| apid == current_apid);
            inner
                .abort_wakers
                .entry(current_apid.clone())
                .or_default()
                .push(AbortWakerEntry {
                    id,
                    waker: Arc::clone(&waker),
                });
            (id, call_now)
        } else {
            (0, true)
        };
        if call_now {
            waker();
        }
        CancellationAbortWaker {
            cancellation: Arc::clone(self),
            apid: current_apid.clone(),
            id,
        }
    }

    fn unregister_abort_waker(&self, apid: &tau_proto::AgentPromptId, id: u64) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if let Some(entries) = inner.abort_wakers.get_mut(apid) {
            entries.retain(|entry| entry.id != id);
            if entries.is_empty() {
                inner.abort_wakers.remove(apid);
            }
        }
    }
}

struct CancellationAbortWaker {
    cancellation: Arc<CancellationState>,
    apid: tau_proto::AgentPromptId,
    id: u64,
}

impl Drop for CancellationAbortWaker {
    fn drop(&mut self) {
        self.cancellation
            .unregister_abort_waker(&self.apid, self.id);
    }
}

impl TurnAbortWaker for CancellationAbortWaker {}

fn prompt_concurrency_limit() -> usize {
    std::env::var(PROMPT_CONCURRENCY_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|&value| 0 < value)
        .unwrap_or(DEFAULT_PROMPT_CONCURRENCY)
}

fn debug_provider_requests_for(
    session_id: &tau_proto::SessionId,
    session_debug_allowed: &BTreeMap<tau_proto::SessionId, bool>,
) -> bool {
    session_debug_allowed
        .get(session_id)
        .copied()
        .unwrap_or(false)
}

fn production_prompt_executor() -> PromptExecutor {
    Arc::new(|execution| {
        let agent_prompt_id = execution.job.agent_prompt_id.clone();
        let model = execution.job.prompt.model.clone();
        let profile_identity = match &execution.job.backend {
            PromptBackend::Responses(config) => Some(quota_profile_identity(config)),
            _ => None,
        };
        let quota_tx = execution.output_tx.clone();
        let quota_waker = execution.output_waker.clone();
        let mut last_quota = None;
        let mut on_quota = |observation: &tau_provider_codex::RollingQuotaObservation| {
            if last_quota.as_ref() == Some(observation) {
                return;
            }
            last_quota = Some(observation.clone());
            let Some(profile_identity) = profile_identity else {
                return;
            };
            let _ = send_worker_message(
                &quota_tx,
                &quota_waker,
                WorkerMessage::QuotaRolling {
                    model: model.clone(),
                    profile_identity,
                    observation: observation.clone(),
                    observed_at_unix_ms: UnixMillis::new(now_ms()),
                },
            );
        };
        let result = {
            let mut writer = execution.frame_writer();
            let mut retry_ctx = SharedRetryContext {
                cancellation: execution.cancellation.clone(),
                current_apid: agent_prompt_id.clone(),
                cancel_generation: execution.job.cancel_generation,
            };
            let prompt_context = ChatGptPromptExecutionContext {
                debug_provider_requests: execution.job.debug_provider_requests,
                runtime: &execution.codex_runtime,
                prior_backend: execution.job.observed_backend.as_ref(),
                logical_attempt: tau_provider_codex::LogicalAttempt::new(
                    execution.job.retry_state.attempts.saturating_add(1),
                ),
                compact_route_unavailable: &|identity| {
                    let _ = send_worker_message(
                        &execution.output_tx,
                        &execution.output_waker,
                        WorkerMessage::CompactRouteUnavailable { identity },
                    );
                },
            };
            handle_prompt_backend(
                &agent_prompt_id,
                &execution.job.backend,
                &execution.job.prompt,
                &mut writer,
                &mut retry_ctx,
                prompt_context,
                &mut on_quota,
            )
        };
        match result {
            Ok(Some(retry)) => {
                let _ = send_worker_message(
                    &execution.output_tx,
                    &execution.output_waker,
                    WorkerMessage::Retry {
                        job: execution.job,
                        decision: retry.decision,
                        live_detail: retry.live_detail,
                        canonical_unauthorized: retry.canonical_unauthorized,
                        terminal_backend: retry.terminal_backend,
                    },
                );
            }
            Ok(None) => {}
            Err(_error) => {
                tracing::warn!(
                    target: LOG_TARGET,
                    "prompt worker failed to emit provider response"
                );
            }
        }
    })
}

fn production_prewarm_executor() -> PrewarmExecutor {
    Arc::new(|mut execution| {
        handle_resolved_prewarm(
            &execution.request,
            &execution.config,
            &execution.runtime,
            execution.debug_provider_requests,
            &mut execution.abort,
        )
    })
}

fn start_prompt_job(mut job: PromptJob, active_prompts: &mut usize, context: &PromptWorkerContext) {
    *active_prompts += 1;
    if let Some(observation) = job.receipt_observation.as_mut() {
        observation.spawning();
    }
    let cooldown_probe = job.cooldown_probe.take();
    let mut execution = PromptExecution {
        job,
        cooldown_probe,
        output_tx: context.worker_tx.clone(),
        output_waker: context.worker_waker.clone(),
        worker_output_depth: context.worker_output_depth.clone(),
        cancellation: context.cancellation.clone(),
        codex_runtime: context.codex_runtime.clone(),
    };
    let executor = context.prompt_executor.clone();
    let done_tx = context.worker_tx.clone();
    let done_waker = context.worker_waker.clone();
    let dispatcher = tracing::dispatcher::get_default(Clone::clone);
    thread::spawn(move || {
        tracing::dispatcher::with_default(&dispatcher, || {
            if let Some(observation) = execution.job.receipt_observation.take() {
                observation.worker_started();
            }
            executor(execution);
            let _ = send_worker_message(&done_tx, &done_waker, WorkerMessage::PromptDone);
        });
    });
}

/// Closes and removes one enabled-only observation on a canceled ownership
/// path.
fn finish_receipt_canceled(observation: &mut Option<ReceiptObservation>) {
    if let Some(observation) = observation.take() {
        observation.finished_before_worker(ReceiptOutcome::Canceled);
    }
}

fn send_worker_message(
    tx: &Sender<WorkerMessage>,
    waker: &ManualRuntimeWaker,
    message: WorkerMessage,
) -> Result<(), ()> {
    // Helper-based worker messages preserve the same enqueue-before-wake order
    // as `WorkerReportSink`, so the main loop may block only after checking the
    // corresponding channel.
    tx.send(message).map_err(|_| ())?;
    waker.wake();
    Ok(())
}

fn start_queued_prompts(
    prompt_queue: &mut VecDeque<PromptJob>,
    active_prompts: &mut usize,
    prompt_concurrency_limit: usize,
    context: &PromptWorkerContext,
    handle: &ClientHandle,
) -> ClientResult<()> {
    while *active_prompts < prompt_concurrency_limit {
        let Some(mut job) = prompt_queue.pop_front() else {
            return Ok(());
        };
        if context.cancellation.take_canceled(&job.agent_prompt_id) {
            if let Some(observation) = job.receipt_observation.take() {
                observation.finished_before_worker(ReceiptOutcome::Canceled);
            }
            let mut frame_writer = handle_report_sink(handle);
            emit_canceled_correlated(
                &job.agent_prompt_id,
                &job.prompt,
                &mut frame_writer,
                job.observed_backend,
                tau_proto::ProviderAttempt::ONE,
            )
            .map_err(|error| ClientError::handler(error.to_string()))?;
            continue;
        }
        job.manual_cooldown_bypass = false;
        start_prompt_job(job, active_prompts, context);
    }
    Ok(())
}

fn finish_queued_canceled(
    apid: &tau_proto::AgentPromptId,
    prompt_queue: &mut VecDeque<PromptJob>,
    handle: &ClientHandle,
) -> ClientResult<bool> {
    let Some(index) = prompt_queue
        .iter()
        .position(|job| job.agent_prompt_id.as_str() == apid.as_str())
    else {
        return Ok(false);
    };
    let Some(mut job) = prompt_queue.remove(index) else {
        return Ok(false);
    };
    finish_receipt_canceled(&mut job.receipt_observation);
    let mut frame_writer = handle_report_sink(handle);
    emit_canceled_correlated(
        &job.agent_prompt_id,
        &job.prompt,
        &mut frame_writer,
        job.observed_backend,
        tau_proto::ProviderAttempt::ONE,
    )
    .map_err(|error| ClientError::handler(error.to_string()))?;
    Ok(true)
}

fn emit_retry_status(
    job: &PromptJob,
    class: RetryClass,
    due: Instant,
    now: Instant,
    live_detail: Option<&str>,
    handle: &ClientHandle,
) -> ClientResult<()> {
    let text = retry_status_text(job, class, due, now, live_detail);
    handle.send(HarnessInputMessage::emit_transient(
        Event::ProviderResponseUpdatedReported(ProviderResponseUpdated {
            agent_prompt_id: job.agent_prompt_id.clone(),
            agent_id: job.prompt.agent_id.clone(),
            deltas: Vec::new(),
            compaction: None,
            status: Some(ProviderResponseStatusUpdate {
                text,
                clear_response: true,
                retry: Some(tau_proto::ProviderRetryStatus {
                    category: retry_class_provider_category(class),
                    attempt: saturating_retry_attempt(job.retry_state.attempts),
                    next_retry_delay_secs: saturating_retry_delay(
                        due.checked_duration_since(now).unwrap_or(Duration::ZERO),
                    ),
                }),
                native_tool: None,
            }),
            response_stats: None,
            originator: job.prompt.originator.clone(),
        }),
    ))
}

/// Builds the initiating user's transient status for one scheduled retry.
fn retry_status_text(
    job: &PromptJob,
    class: RetryClass,
    due: Instant,
    now: Instant,
    live_detail: Option<&str>,
) -> String {
    let delay = due.checked_duration_since(now).unwrap_or(Duration::ZERO);
    let delay_text = tau_proto::format_approximate_duration_secs(delay.as_secs());
    let reason = match &job.backend {
        PromptBackend::Unavailable {
            login_required: Some(provider),
        } => format!("provider {provider} is not logged in; run tau provider login {provider}"),
        PromptBackend::Unavailable {
            login_required: None,
        }
        | PromptBackend::Responses(_)
        | PromptBackend::ChatCompletions { .. }
        | PromptBackend::PublicResponses { .. } => live_detail
            .map(|detail| format!("{}: {detail}", class.public_reason()))
            .unwrap_or_else(|| class.public_reason().to_owned()),
    };
    format!(
        "{}; next attempt in about {} (attempt {}). Tau will keep trying; cancel the prompt to stop.",
        reason, delay_text, job.retry_state.attempts,
    )
}

fn retry_class_provider_category(class: RetryClass) -> tau_proto::ProviderRetryCategory {
    match class {
        RetryClass::Transport => tau_proto::ProviderRetryCategory::Transport,
        RetryClass::Overload => tau_proto::ProviderRetryCategory::Overload,
        RetryClass::Throttle => tau_proto::ProviderRetryCategory::Throttle,
        RetryClass::UsageWindow => tau_proto::ProviderRetryCategory::UsageWindow,
        RetryClass::Account => tau_proto::ProviderRetryCategory::Account,
        RetryClass::Auth => tau_proto::ProviderRetryCategory::Auth,
        RetryClass::Unknown => tau_proto::ProviderRetryCategory::Unknown,
    }
}

fn saturating_retry_attempt(attempt: u64) -> u32 {
    u32::try_from(attempt).unwrap_or(u32::MAX)
}

fn saturating_retry_delay(delay: Duration) -> u32 {
    u32::try_from(delay.as_secs()).unwrap_or(u32::MAX)
}

/// Consume one provider work envelope and discard its transport-only tool
/// reuse reference without cloning the owned prompt payload.
fn materialize_prompt(mut prompt: tau_proto::AgentPromptCreated) -> tau_proto::AgentPromptCreated {
    prompt.tools_ref = None;
    prompt
}

fn trace_provider_prompt(
    prompt: &tau_proto::AgentPromptCreated,
    agent_prompt_id: &tau_proto::AgentPromptId,
) {
    if !tracing::enabled!(target: LOG_TARGET, tracing::Level::TRACE) {
        return;
    }
    let context_items: usize = prompt
        .context
        .blocks
        .iter()
        .map(|block| match block {
            tau_proto::ContextBlock::UserInput(block) => block.items.len(),
            tau_proto::ContextBlock::AssistantResponse(block) => block.output_items.len(),
            tau_proto::ContextBlock::ToolResults(block) => block.items.len(),
        })
        .sum();
    tracing::trace!(
        target: LOG_TARGET,
        agent_prompt_id = %agent_prompt_id,
        system_prompt_present = !prompt.system_prompt.is_empty(),
        context_blocks = prompt.context.blocks.len(),
        context_items,
        tools = prompt.tools.len(),
        tools_ref_present = prompt.tools_ref.is_some(),
        "provider prompt received; content omitted"
    );
}

fn write_prompt_submitted<S: ProviderReportSink>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    originator: &tau_proto::PromptOriginator,
    writer: &mut S,
) -> Result<(), Box<dyn Error>> {
    writer.send_report(HarnessInputMessage::emit_transient(
        Event::ProviderPromptSubmittedReported(ProviderPromptSubmitted {
            agent_prompt_id: agent_prompt_id.clone(),
            originator: originator.clone(),
        }),
    ))?;
    Ok(())
}

fn finish_canceled<S: ProviderReportSink>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    writer: &mut S,
) -> Result<(), Box<dyn Error>> {
    tracing::debug!(
        target: LOG_TARGET,
        agent_prompt_id = %agent_prompt_id,
        "skipping provider request — already canceled by harness",
    );
    writer.send_report(HarnessInputMessage::emit_transient(
        Event::ProviderResponseFinishedReported(simple_finished(
            agent_prompt_id.clone(),
            prompt.agent_id.clone(),
            prompt.originator.clone(),
            "(cancelled by harness)",
        )),
    ))?;
    Ok(())
}

fn simple_finished(
    agent_prompt_id: tau_proto::AgentPromptId,
    agent_id: tau_proto::AgentId,
    originator: tau_proto::PromptOriginator,
    text: impl Into<String>,
) -> ProviderResponseFinished {
    ProviderResponseFinished {
        automatic_compaction_decision: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id,
        agent_id,
        output_items: Vec::new(),
        stop_reason: ProviderStopReason::Error,
        error: Some(text.into()),
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        originator,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    }
}

fn stop_reason_from_output_items(output_items: &[ContextItem]) -> ProviderStopReason {
    if output_items
        .iter()
        .any(|item| matches!(item, ContextItem::ToolCall(_)))
    {
        ProviderStopReason::ToolCalls
    } else {
        ProviderStopReason::EndTurn
    }
}

struct SharedRetryContext {
    cancellation: Arc<CancellationState>,
    current_apid: tau_proto::AgentPromptId,
    cancel_generation: u64,
}

impl TurnAbort for SharedRetryContext {
    fn is_aborted(&mut self) -> bool {
        self.cancellation.retry_generation() != self.cancel_generation
            || self.cancellation.is_canceled(&self.current_apid)
    }

    fn register_waker(
        &mut self,
        waker: Arc<dyn Fn() + Send + Sync + 'static>,
    ) -> Box<dyn TurnAbortWaker> {
        Box::new(self.cancellation.register_abort_waker(
            &self.current_apid,
            self.cancel_generation,
            waker,
        ))
    }
}

fn resolve_prompt_backend(
    model: &ModelId,
    profiles: &mut BuiltinProviderProfiles,
    refresh_rejections: &mut OAuthRefreshRejectionCache,
    network: &tau_provider::OutboundNetworkPolicy,
    extension_data_client: Option<&ExtensionDataClient>,
) -> Option<PromptBackend> {
    let credential_reference = profiles
        .chatgpt_credential_reference(&model.provider)
        .cloned();
    let Some(profile) = profiles.providers.get_mut(&model.provider) else {
        refresh_rejections.clear(&model.provider);
        return None;
    };
    match profile {
        BuiltinProviderProfile::Chatgpt(profile) => {
            let mode = profile.responses_mode();
            let credential_reference = credential_reference?;
            resolve_chatgpt_backend(
                model,
                &credential_reference,
                &mut profile.auth,
                mode,
                refresh_rejections,
                network,
                extension_data_client,
            )
            .map(PromptBackend::Responses)
        }
        BuiltinProviderProfile::ChatCompletions(provider) => {
            refresh_rejections.clear(&model.provider);
            let model_index = provider
                .models
                .iter()
                .position(|configured| configured.id == model.model)?;
            let provider = Arc::new(std::mem::take(provider));
            Some(PromptBackend::ChatCompletions {
                provider,
                model_index,
            })
        }
        BuiltinProviderProfile::OpenRouter(profile) => {
            refresh_rejections.clear(&model.provider);
            let provider = std::mem::take(profile).into_chat_completions();
            let model_index = provider
                .models
                .iter()
                .position(|configured| configured.id == model.model)?;
            Some(PromptBackend::ChatCompletions {
                provider: Arc::new(provider),
                model_index,
            })
        }
        BuiltinProviderProfile::Responses(provider) => {
            refresh_rejections.clear(&model.provider);
            let model_index = provider
                .models
                .iter()
                .position(|configured| configured.id == model.model)?;
            let provider = Arc::new(std::mem::take(provider));
            Some(PromptBackend::PublicResponses {
                provider,
                model_index,
            })
        }
    }
}

/// Resolves an already-hydrated prompt profile without performing OAuth I/O.
fn resolve_prompt_backend_without_refresh(
    model: &ModelId,
    profiles: &mut BuiltinProviderProfiles,
    refresh_rejections: &mut OAuthRefreshRejectionCache,
) -> Option<PromptBackend> {
    let Some(profile) = profiles.providers.get_mut(&model.provider) else {
        refresh_rejections.clear(&model.provider);
        return None;
    };
    match profile {
        BuiltinProviderProfile::Chatgpt(profile) => {
            if profile.auth.access_token.trim().is_empty()
                || oauth_token_is_expired(profile.auth.expires_at_ms)
            {
                return None;
            }
            Some(PromptBackend::Responses(
                tau_provider_codex::resolved_config_for_provider_model(
                    &model.provider,
                    &model.model,
                    tau_provider_codex::ResolvedCredentials::new(
                        profile.auth.access_token.clone(),
                        profile.auth.account_id.clone(),
                    ),
                    profile.responses_mode(),
                ),
            ))
        }
        BuiltinProviderProfile::ChatCompletions(provider) => {
            refresh_rejections.clear(&model.provider);
            let model_index = provider
                .models
                .iter()
                .position(|configured| configured.id == model.model)?;
            let provider = Arc::new(std::mem::take(provider));
            Some(PromptBackend::ChatCompletions {
                provider,
                model_index,
            })
        }
        BuiltinProviderProfile::OpenRouter(profile) => {
            refresh_rejections.clear(&model.provider);
            let provider = std::mem::take(profile).into_chat_completions();
            let model_index = provider
                .models
                .iter()
                .position(|configured| configured.id == model.model)?;
            Some(PromptBackend::ChatCompletions {
                provider: Arc::new(provider),
                model_index,
            })
        }
        BuiltinProviderProfile::Responses(provider) => {
            refresh_rejections.clear(&model.provider);
            let model_index = provider
                .models
                .iter()
                .position(|configured| configured.id == model.model)?;
            let provider = Arc::new(std::mem::take(provider));
            Some(PromptBackend::PublicResponses {
                provider,
                model_index,
            })
        }
    }
}

fn resolve_responses_backend(
    model: &ModelId,
    profiles: &mut BuiltinProviderProfiles,
    refresh_rejections: &mut OAuthRefreshRejectionCache,
    network: &tau_provider::OutboundNetworkPolicy,
    extension_data_client: Option<&ExtensionDataClient>,
) -> Option<ResolvedConfig> {
    let credential_reference = profiles
        .chatgpt_credential_reference(&model.provider)
        .cloned();
    let Some(profile) = profiles.providers.get_mut(&model.provider) else {
        refresh_rejections.clear(&model.provider);
        return None;
    };
    match profile {
        BuiltinProviderProfile::Chatgpt(profile) => {
            let mode = profile.responses_mode();
            let credential_reference = credential_reference?;
            resolve_chatgpt_backend(
                model,
                &credential_reference,
                &mut profile.auth,
                mode,
                refresh_rejections,
                network,
                extension_data_client,
            )
        }
        BuiltinProviderProfile::ChatCompletions(_)
        | BuiltinProviderProfile::OpenRouter(_)
        | BuiltinProviderProfile::Responses(_) => {
            refresh_rejections.clear(&model.provider);
            None
        }
    }
}

fn resolve_chatgpt_backend(
    model: &ModelId,
    credential_reference: &ProviderCredentialReference,
    auth_store: &mut OpenAiAuth,
    mode: CodexMode,
    refresh_rejections: &mut OAuthRefreshRejectionCache,
    network: &tau_provider::OutboundNetworkPolicy,
    extension_data_client: Option<&ExtensionDataClient>,
) -> Option<ResolvedConfig> {
    resolve_chatgpt_backend_with_refresh(
        model,
        &model.provider,
        credential_reference,
        auth_store,
        mode,
        refresh_rejections,
        |provider, credential_reference, mode, rejections, force| {
            refresh_chatgpt_credentials_rpc(
                provider,
                credential_reference,
                mode,
                rejections,
                force,
                network,
                extension_data_client,
            )
        },
    )
}

fn resolve_chatgpt_backend_with_refresh(
    model: &ModelId,
    provider_name: &ProviderName,
    credential_reference: &ProviderCredentialReference,
    auth_store: &mut OpenAiAuth,
    mode: CodexMode,
    refresh_rejections: &mut OAuthRefreshRejectionCache,
    refresh: impl FnOnce(
        &ProviderName,
        &ProviderCredentialReference,
        CodexMode,
        &mut OAuthRefreshRejectionCache,
        bool,
    ) -> Result<OpenAiAuth, RefreshCredentialsError>,
) -> Option<ResolvedConfig> {
    let current_config = tau_provider_codex::resolved_config_for_provider_model(
        &model.provider,
        &model.model,
        tau_provider_codex::ResolvedCredentials::new(
            auth_store.access_token.clone(),
            auth_store.account_id.clone(),
        ),
        mode,
    );
    let current_identity = backend_profile_identity(&PromptBackend::Responses(current_config));
    if current_identity
        .is_some_and(|identity| refresh_rejections.unauthorized_exhausted(provider_name, identity))
    {
        return None;
    }
    let forced = current_identity
        .is_some_and(|identity| refresh_rejections.take_unauthorized(provider_name, identity));
    let refresh_due =
        oauth_token_should_refresh(&auth_store.access_token, auth_store.expires_at_ms)
            && !auth_store.refresh_token.trim().is_empty();
    if forced || refresh_due || refresh_rejections.contains(provider_name) {
        match refresh(
            provider_name,
            credential_reference,
            mode,
            refresh_rejections,
            forced,
        ) {
            Ok(refreshed) => {
                *auth_store = refreshed;
            }
            Err(error @ RefreshCredentialsError::Storage(_)) => {
                tracing::debug!(
                    target: LOG_TARGET,
                    provider = %provider_name,
                    %error,
                    "ChatGPT credential refresh details"
                );
                tracing::warn!(
                    target: LOG_TARGET,
                    "failed to refresh ChatGPT credentials"
                );
                if forced {
                    auth_store.access_token.clear();
                }
            }
            Err(
                error @ (RefreshCredentialsError::IdentityMismatch
                | RefreshCredentialsError::RejectedGeneration),
            ) => {
                tracing::debug!(
                    target: LOG_TARGET,
                    provider = %provider_name,
                    %error,
                    "ChatGPT credential refresh details"
                );
                tracing::warn!(
                    target: LOG_TARGET,
                    "failed to refresh ChatGPT credentials"
                );
                auth_store.access_token.clear();
            }
            Err(
                RefreshCredentialsError::OAuth { credentials, error }
                | RefreshCredentialsError::Suppressed { credentials, error },
            ) => {
                *auth_store = *credentials;
                tracing::debug!(
                    target: LOG_TARGET,
                    provider = %provider_name,
                    %error,
                    "ChatGPT credential refresh details"
                );
                tracing::warn!(
                    target: LOG_TARGET,
                    "failed to refresh ChatGPT credentials"
                );
                if forced {
                    auth_store.access_token.clear();
                }
            }
        }
    }
    if auth_store.access_token.trim().is_empty() || oauth_token_is_expired(auth_store.expires_at_ms)
    {
        return None;
    }

    Some(tau_provider_codex::resolved_config_for_provider_model(
        &model.provider,
        &model.model,
        tau_provider_codex::ResolvedCredentials::new(
            auth_store.access_token.clone(),
            auth_store.account_id.clone(),
        ),
        mode,
    ))
}

/// Closed result category for one Secret RPC used by OAuth credential refresh.
enum OAuthCredentialStorageError {
    /// The Secret RPC failed for a reason other than a CAS race.
    Unavailable,
    /// Another refresher replaced the credential after its initial read.
    GenerationMismatch,
}

fn refresh_chatgpt_credentials_rpc(
    provider_name: &ProviderName,
    credential_reference: &ProviderCredentialReference,
    mode: CodexMode,
    refresh_rejections: &mut OAuthRefreshRejectionCache,
    force: bool,
    network: &tau_provider::OutboundNetworkPolicy,
    extension_data_client: Option<&ExtensionDataClient>,
) -> Result<OpenAiAuth, RefreshCredentialsError> {
    let client = extension_data_client.ok_or_else(|| {
        RefreshCredentialsError::Storage(path_std_io::Error::new(
            path_std_io::ErrorKind::PermissionDenied,
            "Secret RPC is unavailable",
        ))
    })?;
    refresh_chatgpt_credentials_with(
        provider_name,
        credential_reference,
        mode,
        refresh_rejections,
        force,
        |operation| {
            client
                .request(tau_proto::ExtensionDataScope::Secret, operation)
                .map_err(|error| match error {
                    tau_client::ExtensionDataRpcError::Harness {
                        kind: tau_proto::ExtensionDataErrorKind::GenerationMismatch,
                        ..
                    } => OAuthCredentialStorageError::GenerationMismatch,
                    tau_client::ExtensionDataRpcError::Client(_)
                    | tau_client::ExtensionDataRpcError::Harness { .. }
                    | tau_client::ExtensionDataRpcError::InputClosed
                    | tau_client::ExtensionDataRpcError::Disconnect(_)
                    | tau_client::ExtensionDataRpcError::Timeout => {
                        OAuthCredentialStorageError::Unavailable
                    }
                })
        },
        |refresh_token| tau_provider_codex::oauth::openai_codex_refresh(refresh_token, network),
    )
}

fn refresh_chatgpt_credentials_with(
    provider_name: &ProviderName,
    credential_reference: &ProviderCredentialReference,
    mode: CodexMode,
    refresh_rejections: &mut OAuthRefreshRejectionCache,
    force: bool,
    mut request: impl FnMut(
        tau_proto::ExtensionDataRequestOp,
    ) -> Result<tau_proto::ExtensionDataValue, OAuthCredentialStorageError>,
    refresh: impl FnOnce(
        &str,
    ) -> Result<
        tau_provider_codex::oauth::OAuthTokenRefresh,
        tau_provider_codex::oauth::OAuthError,
    >,
) -> Result<OpenAiAuth, RefreshCredentialsError> {
    let path = credential_reference.path().clone();
    let value = request(tau_proto::ExtensionDataRequestOp::ReadFile { path: path.clone() })
        .map_err(|_| {
            RefreshCredentialsError::Storage(path_std_io::Error::other(
                "could not read OAuth credential through Secret RPC",
            ))
        })?;
    let tau_proto::ExtensionDataValue::ReadFile { contents } = value else {
        return Err(RefreshCredentialsError::Storage(path_std_io::Error::other(
            "unexpected OAuth credential read result",
        )));
    };
    let record: credential_record::ChatGptOAuthCredential = serde_json::from_slice(&contents)
        .map_err(|_| {
            RefreshCredentialsError::Storage(path_std_io::Error::new(
                path_std_io::ErrorKind::InvalidData,
                "invalid version-zero OAuth credential",
            ))
        })?;
    let current = record.into_auth();
    if let Some(error) = refresh_rejections.rejection(provider_name, &current, mode) {
        return Err(RefreshCredentialsError::Suppressed {
            credentials: Box::new(current),
            error,
        });
    }
    if !refresh_required(&current, force)? {
        refresh_rejections.clear_refresh_rejection(provider_name);
        return Ok(current);
    }
    let tokens = match refresh(&current.refresh_token) {
        Ok(tokens) => tokens,
        Err(error) => {
            refresh_rejections.record_if_permanent(provider_name, &current, mode, &error);
            return Err(RefreshCredentialsError::OAuth {
                credentials: Box::new(current),
                error,
            });
        }
    };
    let refreshed = merge_chatgpt_refresh(&current, tokens)?;
    let replacement = serde_json::to_vec(&credential_record::ChatGptOAuthCredential::from(
        refreshed.clone(),
    ))
    .map_err(|_| {
        RefreshCredentialsError::Storage(path_std_io::Error::other(
            "could not encode refreshed OAuth credential",
        ))
    })?;
    let expected_generation = blake3::hash(&contents).to_hex().to_string();
    match request(tau_proto::ExtensionDataRequestOp::CompareAndSwapFile {
        path: path.clone(),
        expected_generation,
        contents: replacement,
    }) {
        Ok(tau_proto::ExtensionDataValue::CompareAndSwapFile) => {
            if force && refreshed.access_token == current.access_token {
                return Err(RefreshCredentialsError::RejectedGeneration);
            }
            refresh_rejections.clear_refresh_rejection(provider_name);
            Ok(refreshed)
        }
        Ok(_) => Err(RefreshCredentialsError::Storage(path_std_io::Error::other(
            "unexpected OAuth credential CAS result",
        ))),
        Err(OAuthCredentialStorageError::GenerationMismatch) => {
            // A concurrent rotating refresh may have won CAS. Reload and use its
            // complete generation rather than retrying the now-consumed token.
            let value =
                request(tau_proto::ExtensionDataRequestOp::ReadFile { path }).map_err(|_| {
                    RefreshCredentialsError::Storage(path_std_io::Error::other(
                        "OAuth credential CAS failed and reload was unavailable",
                    ))
                })?;
            let tau_proto::ExtensionDataValue::ReadFile { contents } = value else {
                return Err(RefreshCredentialsError::Storage(path_std_io::Error::other(
                    "unexpected OAuth credential reload result",
                )));
            };
            let record: credential_record::ChatGptOAuthCredential =
                serde_json::from_slice(&contents).map_err(|_| {
                    RefreshCredentialsError::Storage(path_std_io::Error::new(
                        path_std_io::ErrorKind::InvalidData,
                        "invalid reloaded version-zero OAuth credential",
                    ))
                })?;
            let authoritative = record.into_auth();
            validate_authoritative_rotation(&current, &authoritative, force)?;
            refresh_rejections.clear_refresh_rejection(provider_name);
            Ok(authoritative)
        }
        Err(OAuthCredentialStorageError::Unavailable) => Err(RefreshCredentialsError::Storage(
            path_std_io::Error::other("OAuth credential CAS failed"),
        )),
    }
}

fn refresh_required(current: &OpenAiAuth, force: bool) -> Result<bool, RefreshCredentialsError> {
    if current.refresh_token.trim().is_empty() {
        return if force {
            Err(RefreshCredentialsError::RejectedGeneration)
        } else {
            Ok(false)
        };
    }
    Ok(force || oauth_token_should_refresh(&current.access_token, current.expires_at_ms))
}

fn validate_authoritative_rotation(
    current: &OpenAiAuth,
    authoritative: &OpenAiAuth,
    force: bool,
) -> Result<(), RefreshCredentialsError> {
    let current_identity = chatgpt_account_identity(current)?;
    let authoritative_identity = chatgpt_account_identity(authoritative)?;
    if current_identity.is_none() || authoritative_identity != current_identity {
        return Err(RefreshCredentialsError::IdentityMismatch);
    }
    if force && authoritative.access_token == current.access_token {
        return Err(RefreshCredentialsError::RejectedGeneration);
    }
    Ok(())
}

fn chatgpt_account_identity(auth: &OpenAiAuth) -> Result<Option<String>, RefreshCredentialsError> {
    let stored = auth
        .account_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned);
    let claim = tau_provider_codex::oauth::jwt_openai_account_id(&auth.access_token)
        .filter(|value| !value.trim().is_empty());
    if stored.is_some() && claim.is_some() && stored != claim {
        return Err(RefreshCredentialsError::IdentityMismatch);
    }
    Ok(stored.or(claim))
}

fn merge_chatgpt_refresh(
    current: &OpenAiAuth,
    tokens: tau_provider_codex::oauth::OAuthTokenRefresh,
) -> Result<OpenAiAuth, RefreshCredentialsError> {
    let expected_account = chatgpt_account_identity(current)?;
    let replaced_access = tokens.access_token.is_some();
    let access_token = tokens
        .access_token
        .unwrap_or_else(|| current.access_token.clone());
    let account_id = if replaced_access {
        tokens
            .account_id
            .or_else(|| tau_provider_codex::oauth::jwt_openai_account_id(&access_token))
    } else {
        expected_account.clone()
    };
    if expected_account.is_none() || account_id != expected_account {
        return Err(RefreshCredentialsError::IdentityMismatch);
    }
    Ok(OpenAiAuth {
        access_token,
        refresh_token: tokens
            .refresh_token
            .unwrap_or_else(|| current.refresh_token.clone()),
        expires_at_ms: tokens.expires_at_ms.unwrap_or(current.expires_at_ms),
        account_id,
    })
}

fn oauth_token_should_refresh(access_token: &str, expires_at_ms: u64) -> bool {
    let now_ms = now_ms();
    if let Some(issued_at_ms) = jwt_issued_at_ms(access_token) {
        let lifetime_ms = expires_at_ms.saturating_sub(issued_at_ms);
        let refresh_at_ms = issued_at_ms.saturating_add(lifetime_ms / 2);
        if refresh_at_ms <= now_ms {
            return true;
        }
    }
    expires_at_ms <= now_ms.saturating_add(duration_millis_u64(Duration::from_secs(5 * 60)))
}

fn oauth_token_is_expired(expires_at_ms: u64) -> bool {
    expires_at_ms <= now_ms()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn duration_millis_u64(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

fn jwt_issued_at_ms(jwt: &str) -> Option<u64> {
    tau_provider_codex::oauth::jwt_issued_at_ms(jwt)
}

#[cfg(test)]
fn emit_retry_banner<S: ProviderReportSink>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    agent_id: &tau_proto::AgentId,
    originator: &tau_proto::PromptOriginator,
    writer: &mut S,
    error: &str,
    delay: Duration,
    attempt: usize,
) {
    let banner = format!(
        "provider error — retrying in {}s (attempt {}). Tau will keep trying; cancel to stop.\n\n> {}",
        delay.as_secs(),
        attempt,
        error,
    );
    let _ = writer.send_report(HarnessInputMessage::emit_transient(
        Event::ProviderResponseUpdatedReported(ProviderResponseUpdated {
            agent_prompt_id: agent_prompt_id.clone(),
            agent_id: agent_id.clone(),
            deltas: Vec::new(),
            compaction: None,
            status: Some(ProviderResponseStatusUpdate {
                text: banner,
                clear_response: true,
                retry: None,
                native_tool: None,
            }),
            response_stats: None,
            originator: originator.clone(),
        }),
    ));
}

fn resolve_prewarm_backend(
    prewarm: &tau_proto::AgentPromptPrewarmRequested,
    profiles: &mut BuiltinProviderProfiles,
    refresh_rejections: &mut OAuthRefreshRejectionCache,
    network: &tau_provider::OutboundNetworkPolicy,
    extension_data_client: Option<&ExtensionDataClient>,
) -> Option<(ModelId, ResolvedConfig)> {
    let Some(model) = prewarm.model.as_ref() else {
        tracing::debug!(
            target: LOG_TARGET,
            agent_id = %prewarm.agent_id,
            "skipping prompt prewarm: no selected model",
        );
        return None;
    };
    let Some(config) = resolve_responses_backend(
        model,
        profiles,
        refresh_rejections,
        network,
        extension_data_client,
    ) else {
        tracing::debug!(
            target: LOG_TARGET,
            agent_id = %prewarm.agent_id,
            model = %model,
            "skipping prompt prewarm: unsupported backend",
        );
        return None;
    };
    Some((model.clone(), config))
}

fn handle_resolved_prewarm(
    prewarm: &tau_proto::AgentPromptPrewarmRequested,
    config: &ResolvedConfig,
    codex_runtime: &CodexRuntime,
    debug_provider_requests: bool,
    abort: &mut impl TurnAbort,
) -> tau_proto::ProviderCacheRefreshStatus {
    let session_id_str = prewarm.session_id.as_str();
    let request = CodexPrompt {
        system_prompt: &prewarm.system_prompt,
        context: &prewarm.context,
        tools: &prewarm.tools,
        hosted_tools: &[],
        params: prewarm.model_params,
        tool_choice: prewarm.tool_choice,
        compaction: None,
        originator: &prewarm.originator,
        share_user_cache_key: prewarm.share_user_cache_key,
        session_id: &prewarm.session_id,
        agent_id: &prewarm.agent_id,
        debug_provider_requests,
    };
    tracing::debug!(target: LOG_TARGET, session_id = session_id_str, "starting prompt prewarm");
    match codex_runtime.prewarm(config, session_id_str, &request, abort) {
        PrewarmOutcome::Installed => {
            tracing::debug!(target: LOG_TARGET, session_id = session_id_str, "completed prompt prewarm");
            tau_proto::ProviderCacheRefreshStatus::Succeeded
        }
        PrewarmOutcome::SkippedBusy => {
            tracing::debug!(target: LOG_TARGET, session_id = session_id_str, "skipped prompt prewarm: websocket key is busy");
            tau_proto::ProviderCacheRefreshStatus::Failed
        }
        PrewarmOutcome::Retry(decision) => {
            tracing::debug!(target: LOG_TARGET, session_id = session_id_str, retry_class = ?decision.class, "prompt prewarm ended with retryable provider failure");
            tau_proto::ProviderCacheRefreshStatus::Failed
        }
        PrewarmOutcome::Canceled => {
            tracing::debug!(target: LOG_TARGET, session_id = session_id_str, "prompt prewarm canceled");
            tau_proto::ProviderCacheRefreshStatus::Cancelled
        }
        PrewarmOutcome::Terminal(error) => {
            tracing::debug!(target: LOG_TARGET, session_id = session_id_str, "prompt prewarm failed: {error}");
            tau_proto::ProviderCacheRefreshStatus::Failed
        }
    }
}

fn handle_prompt_backend<R, S: ProviderReportSink>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    backend: &PromptBackend,
    prompt: &tau_proto::AgentPromptCreated,
    writer: &mut S,
    retry_ctx: &mut R,
    context: ChatGptPromptExecutionContext<'_>,
    on_quota: &mut impl FnMut(&tau_provider_codex::RollingQuotaObservation),
) -> Result<Option<PromptAttemptRetry>, Box<dyn Error>>
where
    R: TurnAbort,
{
    match backend {
        PromptBackend::Unavailable { .. } => Ok(Some(PromptAttemptRetry {
            decision: RetryDecision::new(RetryClass::Auth),
            live_detail: None,
            canonical_unauthorized: false,
            terminal_backend: None,
        })),
        PromptBackend::Responses(config) => handle_prompt(
            agent_prompt_id,
            config,
            prompt,
            writer,
            retry_ctx,
            context,
            on_quota,
        ),
        PromptBackend::ChatCompletions {
            provider,
            model_index,
        } => handle_chat_completions_backend(
            agent_prompt_id,
            prompt,
            provider,
            &provider.models[*model_index],
            writer,
            retry_ctx,
            context,
        ),
        PromptBackend::PublicResponses {
            provider,
            model_index,
        } => handle_public_responses_backend(
            agent_prompt_id,
            prompt,
            provider,
            &provider.models[*model_index],
            writer,
            retry_ctx,
            context,
        ),
    }
}

/// Runs one Chat Completions attempt and reports its terminal or retry outcome.
fn handle_chat_completions_backend<R, S: ProviderReportSink>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    provider: &ChatCompletionsProvider,
    model: &ChatCompletionsModel,
    writer: &mut S,
    retry_ctx: &mut R,
    context: ChatGptPromptExecutionContext<'_>,
) -> Result<Option<PromptAttemptRetry>, Box<dyn Error>>
where
    R: TurnAbort,
{
    if TurnAbort::is_aborted(retry_ctx) {
        return finish_canceled_attempt(
            agent_prompt_id,
            prompt,
            writer,
            false,
            context.prior_backend.cloned(),
            context.logical_attempt.provider_attempt(),
        );
    }
    let outcome = run_prompt_attempt(
        agent_prompt_id,
        prompt,
        provider,
        model,
        context.debug_provider_requests,
        writer,
        &mut || TurnAbort::is_aborted(retry_ctx),
        context.runtime.network(),
        context.logical_attempt.provider_attempt(),
    );
    match outcome {
        ChatCompletionsAttemptOutcome::Finished(finished) => finish_backend_attempt(
            agent_prompt_id,
            prompt,
            writer,
            retry_ctx,
            *finished,
            true,
            CancellationFinishPolicy {
                detail: "request canceled; discarding tentative provider output",
                retain_correlation: true,
            },
        ),
        ChatCompletionsAttemptOutcome::Terminal {
            mut finished,
            progress,
        } => {
            finished.backend = observed_backend(finished.backend.take(), context.prior_backend);
            finish_terminal_attempt(
                agent_prompt_id,
                prompt,
                writer,
                *finished,
                progress == tau_provider_chat_completions::SemanticProgress::Parsed,
            )
        }
        ChatCompletionsAttemptOutcome::Retry {
            decision,
            progress,
            backend_reached,
        } => finish_retry_attempt(
            agent_prompt_id,
            prompt,
            writer,
            decision,
            progress == tau_provider_chat_completions::SemanticProgress::Parsed,
            observed_backend(
                backend_reached.then(|| chat_completions_backend(provider)),
                context.prior_backend,
            ),
        ),
        ChatCompletionsAttemptOutcome::Canceled { progress, facts } => finish_canceled_attempt(
            agent_prompt_id,
            prompt,
            writer,
            progress == tau_provider_chat_completions::SemanticProgress::Parsed,
            observed_backend(
                facts
                    .backend_reached
                    .then(|| chat_completions_backend(provider)),
                context.prior_backend,
            ),
            facts.provider_attempt,
        ),
    }
}
/// Runs one public Responses attempt and reports its terminal or retry outcome.
fn handle_public_responses_backend<R, S: ProviderReportSink>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    provider: &ResponsesProvider,
    model: &ResponsesModel,
    writer: &mut S,
    retry_ctx: &mut R,
    context: ChatGptPromptExecutionContext<'_>,
) -> Result<Option<PromptAttemptRetry>, Box<dyn Error>>
where
    R: TurnAbort,
{
    if TurnAbort::is_aborted(retry_ctx) {
        return finish_canceled_attempt(
            agent_prompt_id,
            prompt,
            writer,
            false,
            context.prior_backend.cloned(),
            context.logical_attempt.provider_attempt(),
        );
    }
    match run_responses_prompt_attempt(
        agent_prompt_id,
        prompt,
        provider,
        model,
        context.debug_provider_requests,
        writer,
        &mut || TurnAbort::is_aborted(retry_ctx),
        context.runtime.network(),
        context.logical_attempt.provider_attempt(),
    ) {
        ResponsesAttemptOutcome::Finished(finished) => finish_backend_attempt(
            agent_prompt_id,
            prompt,
            writer,
            retry_ctx,
            *finished,
            true,
            CancellationFinishPolicy {
                detail: "request canceled; discarding tentative provider output",
                retain_correlation: true,
            },
        ),
        ResponsesAttemptOutcome::Terminal {
            mut finished,
            progress,
        } => {
            finished.backend = observed_backend(finished.backend.take(), context.prior_backend);
            finish_terminal_attempt(
                agent_prompt_id,
                prompt,
                writer,
                *finished,
                progress.has_timed_semantic_output,
            )
        }
        ResponsesAttemptOutcome::Retry {
            decision,
            progress,
            backend_reached,
        } => finish_retry_attempt(
            agent_prompt_id,
            prompt,
            writer,
            decision,
            progress.has_timed_semantic_output,
            observed_backend(
                backend_reached.then(|| responses_backend(provider)),
                context.prior_backend,
            ),
        ),
        ResponsesAttemptOutcome::Canceled {
            progress,
            backend_reached,
        } => finish_canceled_attempt(
            agent_prompt_id,
            prompt,
            writer,
            progress.has_timed_semantic_output,
            observed_backend(
                backend_reached.then(|| responses_backend(provider)),
                context.prior_backend,
            ),
            context.logical_attempt.provider_attempt(),
        ),
    }
}
/// Emits a successful final response unless a concurrent cancellation won.
struct CancellationFinishPolicy {
    /// Transient detail emitted when tentative output must be cleared.
    detail: &'static str,
    /// Whether the cancellation terminal retains backend/attempt correlation.
    retain_correlation: bool,
}

/// Emits a successful final response unless a concurrent cancellation won.
fn finish_backend_attempt<R, S: ProviderReportSink>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    writer: &mut S,
    retry_ctx: &mut R,
    finished: ProviderResponseFinished,
    has_partial_output: bool,
    cancellation: CancellationFinishPolicy,
) -> Result<Option<PromptAttemptRetry>, Box<dyn Error>>
where
    R: TurnAbort,
{
    if TurnAbort::is_aborted(retry_ctx) {
        clear_partial_backend_response(
            agent_prompt_id,
            prompt,
            writer,
            has_partial_output,
            cancellation.detail,
        )?;
        if cancellation.retain_correlation {
            emit_canceled_correlated(
                agent_prompt_id,
                prompt,
                writer,
                finished.backend,
                finished.provider_attempt,
            )?;
        } else {
            finish_canceled(agent_prompt_id, prompt, writer)?;
        }
        return Ok(None);
    }
    emit_finished_backend_response(writer, finished)?;
    Ok(None)
}

/// Emits a terminal backend result after clearing any rendered partial
/// response.
fn finish_terminal_attempt<S: ProviderReportSink>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    writer: &mut S,
    finished: ProviderResponseFinished,
    has_partial_output: bool,
) -> Result<Option<PromptAttemptRetry>, Box<dyn Error>> {
    clear_partial_backend_response(
        agent_prompt_id,
        prompt,
        writer,
        has_partial_output,
        "provider stream ended with an error; discarding partial output",
    )?;
    emit_finished_backend_response(writer, finished)?;
    Ok(None)
}

/// Returns retry evidence after clearing any rendered partial response.
fn finish_retry_attempt<S: ProviderReportSink>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    writer: &mut S,
    decision: RetryDecision,
    has_partial_output: bool,
    terminal_backend: Option<ProviderBackend>,
) -> Result<Option<PromptAttemptRetry>, Box<dyn Error>> {
    clear_partial_backend_response(
        agent_prompt_id,
        prompt,
        writer,
        has_partial_output,
        "provider stream interrupted after partial output; preparing retry",
    )?;
    Ok(Some(PromptAttemptRetry {
        decision,
        live_detail: None,
        canonical_unauthorized: false,
        terminal_backend,
    }))
}

/// Finishes a cancellation after clearing any rendered partial response.
fn finish_canceled_attempt<S: ProviderReportSink>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    writer: &mut S,
    has_partial_output: bool,
    backend: Option<ProviderBackend>,
    provider_attempt: tau_proto::ProviderAttempt,
) -> Result<Option<PromptAttemptRetry>, Box<dyn Error>> {
    clear_partial_backend_response(
        agent_prompt_id,
        prompt,
        writer,
        has_partial_output,
        "request canceled; discarding partial provider output",
    )?;
    emit_canceled_correlated(agent_prompt_id, prompt, writer, backend, provider_attempt)?;
    Ok(None)
}

/// Emit one cancellation terminal while retaining attempt correlation.
fn emit_canceled_correlated<S: ProviderReportSink>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    writer: &mut S,
    backend: Option<ProviderBackend>,
    provider_attempt: tau_proto::ProviderAttempt,
) -> Result<(), Box<dyn Error>> {
    let mut finished = simple_finished(
        agent_prompt_id.clone(),
        prompt.agent_id.clone(),
        prompt.originator.clone(),
        "(cancelled by harness)",
    );
    finished.backend = backend;
    finished.provider_attempt = provider_attempt;
    emit_finished_backend_response(writer, finished)?;
    Ok(())
}

/// Clears partial provider text only when the backend reported semantic output.
fn clear_partial_backend_response<S: ProviderReportSink>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    writer: &mut S,
    has_partial_output: bool,
    detail: &str,
) -> Result<(), Box<dyn Error>> {
    if has_partial_output {
        emit_chat_completions_partial_clear(agent_prompt_id, prompt, detail, writer)?;
    }
    Ok(())
}

/// Submits one terminal provider response report.
fn emit_finished_backend_response<S: ProviderReportSink>(
    writer: &mut S,
    finished: ProviderResponseFinished,
) -> Result<(), Box<dyn Error>> {
    writer.send_report(HarnessInputMessage::emit_transient(
        Event::ProviderResponseFinishedReported(finished),
    ))?;
    Ok(())
}

fn emit_chat_completions_partial_clear<S: ProviderReportSink>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    text: &str,
    writer: &mut S,
) -> Result<(), Box<dyn Error>> {
    writer.send_report(HarnessInputMessage::emit_transient(
        Event::ProviderResponseUpdatedReported(ProviderResponseUpdated {
            agent_prompt_id: agent_prompt_id.clone(),
            agent_id: prompt.agent_id.clone(),
            deltas: Vec::new(),
            compaction: None,
            status: Some(ProviderResponseStatusUpdate {
                text: text.to_owned(),
                clear_response: true,
                retry: None,
                native_tool: None,
            }),
            response_stats: None,
            originator: prompt.originator.clone(),
        }),
    ))?;
    Ok(())
}

/// Shared immutable inputs for one ChatGPT provider prompt attempt.
#[derive(Clone, Copy)]
struct ChatGptPromptExecutionContext<'a> {
    /// Whether durable-session policy permits provider debug captures.
    debug_provider_requests: bool,
    /// Shared ChatGPT transport runtime and WebSocket pool.
    runtime: &'a CodexRuntime,
    /// Backend reached by an earlier finite attempt in this logical turn.
    prior_backend: Option<&'a ProviderBackend>,
    /// One-based finite-attempt ordinal owned by this prompt execution.
    logical_attempt: tau_provider_codex::LogicalAttempt,
    /// Publishes a process-local capability downgrade to the main loop.
    compact_route_unavailable: &'a dyn Fn(InferenceProfileIdentity),
}

/// Retry evidence returned by one finite provider attempt.
struct PromptAttemptRetry {
    /// Closed scheduler decision.
    decision: RetryDecision,
    /// Bounded provider detail for ordinary live status only.
    live_detail: Option<tau_provider_codex::RedactedProviderDetail>,
    /// Canonical provider 401 authority for exact-generation credential
    /// recovery.
    canonical_unauthorized: bool,
    /// Backend reached by this attempt for a synthesized retry-budget terminal.
    terminal_backend: Option<ProviderBackend>,
}

fn handle_compact_prompt<R, S: ProviderReportSink>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    config: &ResolvedConfig,
    prompt: &tau_proto::AgentPromptCreated,
    request: &CodexPrompt<'_>,
    writer: &mut S,
    retry_ctx: &mut R,
    execution: ChatGptPromptExecutionContext<'_>,
) -> Result<Option<PromptAttemptRetry>, Box<dyn Error>>
where
    R: TurnAbort,
{
    // Standalone compaction deliberately has no inline fallback.
    match execution.runtime.compact_numbered(
        agent_prompt_id,
        execution.logical_attempt,
        config,
        request,
        retry_ctx,
    ) {
        CompactOutcome::Finished {
            output_items,
            usage,
        } => {
            writer.send_report(HarnessInputMessage::emit_transient(
                Event::ProviderResponseFinishedReported(compact_finished_response(
                    agent_prompt_id,
                    prompt,
                    backend_descriptor(config, ProviderBackendTransport::Websocket, false),
                    output_items,
                    usage,
                    execution.logical_attempt.provider_attempt(),
                )),
            ))?;
            Ok(None)
        }
        CompactOutcome::Retry {
            decision,
            backend_reached,
        } => Ok(Some(PromptAttemptRetry {
            decision,
            live_detail: None,
            canonical_unauthorized: false,
            terminal_backend: observed_backend(
                backend_reached.then(|| {
                    backend_descriptor(config, ProviderBackendTransport::Websocket, false)
                }),
                execution.prior_backend,
            ),
        })),
        CompactOutcome::Canceled { backend_reached } => finish_canceled_attempt(
            agent_prompt_id,
            prompt,
            writer,
            false,
            observed_backend(
                backend_reached.then(|| {
                    backend_descriptor(config, ProviderBackendTransport::Websocket, false)
                }),
                execution.prior_backend,
            ),
            tau_proto::ProviderAttempt::ONE,
        ),
        CompactOutcome::Terminal {
            error,
            backend_reached,
        } => {
            let backend = backend_descriptor(config, ProviderBackendTransport::Websocket, false);
            let observed =
                observed_backend(backend_reached.then_some(backend), execution.prior_backend);
            finish_error(
                agent_prompt_id,
                prompt,
                observed.as_ref(),
                error,
                None,
                execution.debug_provider_requests,
                execution.logical_attempt.provider_attempt(),
                writer,
            )?;
            Ok(None)
        }
        CompactOutcome::RouteUnavailable {
            error,
            newly_downgraded,
            profile_identity,
            backend_reached,
        } => {
            if newly_downgraded {
                (execution.compact_route_unavailable)(profile_identity);
            }
            let backend = backend_descriptor(config, ProviderBackendTransport::Websocket, false);
            let observed =
                observed_backend(backend_reached.then_some(backend), execution.prior_backend);
            finish_error(
                agent_prompt_id,
                prompt,
                observed.as_ref(),
                error,
                None,
                execution.debug_provider_requests,
                execution.logical_attempt.provider_attempt(),
                writer,
            )?;
            Ok(None)
        }
    }
}

fn compact_finished_response(
    agent_prompt_id: &tau_proto::AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    backend: tau_proto::ProviderBackend,
    output_items: Vec<tau_proto::ContextItem>,
    usage: Option<tau_proto::ProviderTokenUsage>,
    provider_attempt: tau_proto::ProviderAttempt,
) -> ProviderResponseFinished {
    ProviderResponseFinished {
        automatic_compaction_decision: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,
        agent_prompt_id: agent_prompt_id.clone(),
        agent_id: prompt.agent_id.clone(),
        output_items,
        stop_reason: ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        originator: prompt.originator.clone(),
        usage,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: Some(backend),
        provider_attempt,
        provider_response_id: None,
        ws_pool_delta: None,
    }
}

fn handle_prompt<R, S: ProviderReportSink>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    config: &ResolvedConfig,
    prompt: &tau_proto::AgentPromptCreated,
    writer: &mut S,
    retry_ctx: &mut R,
    execution: ChatGptPromptExecutionContext<'_>,
    on_quota: &mut impl FnMut(&tau_provider_codex::RollingQuotaObservation),
) -> Result<Option<PromptAttemptRetry>, Box<dyn Error>>
where
    R: TurnAbort,
{
    let request = CodexPrompt {
        system_prompt: &prompt.system_prompt,
        context: &prompt.context,
        tools: &prompt.tools,
        hosted_tools: &prompt.hosted_tools,
        params: prompt.model_params,
        tool_choice: prompt.tool_choice,
        compaction: prompt.compaction,
        originator: &prompt.originator,
        share_user_cache_key: prompt.share_user_cache_key,
        session_id: &prompt.session_id,
        agent_id: &prompt.agent_id,
        debug_provider_requests: execution.debug_provider_requests,
    };

    if prompt.operation == tau_proto::PromptOperation::StandaloneCompaction {
        return handle_compact_prompt(
            agent_prompt_id,
            config,
            prompt,
            &request,
            writer,
            retry_ctx,
            execution,
        );
    }

    let originator = prompt.originator.clone();
    let transport_taken = ProviderBackendTransport::Websocket;
    let mut backend_reached = false;
    let mut ws_pool_delta = None;
    let mut response_update_emitter = RateLimitedResponseUpdateEmitter::new();
    let mut on_update = |update: StreamUpdate<'_>| match update {
        StreamUpdate::Connecting => {
            emit_chatgpt_connecting_update(agent_prompt_id, &prompt.agent_id, &originator, writer);
        }
        StreamUpdate::Dispatched(at) => {
            backend_reached = true;
            response_update_emitter.mark_dispatched(at);
        }
        StreamUpdate::Response(state) => {
            if let Some(observation) = state.quota_observation() {
                on_quota(observation);
            }
            response_update_emitter.emit_if_due(
                agent_prompt_id,
                &prompt.agent_id,
                &originator,
                state,
                writer,
            );
        }
    };
    let outcome = execution.runtime.run_attempt_numbered(
        agent_prompt_id,
        execution.logical_attempt,
        config,
        &request,
        retry_ctx,
        &mut on_update,
    );
    if let CodexAttemptOutcome::Finished(dispatch) = &outcome {
        response_update_emitter.emit_terminal_flush(
            agent_prompt_id,
            &prompt.agent_id,
            &originator,
            &dispatch.state,
            writer,
        );
    }
    match outcome {
        CodexAttemptOutcome::Finished(dispatch) => {
            ws_pool_delta = dispatch.ws_pool_delta;
            let backend = backend_descriptor(
                config,
                transport_taken,
                dispatch.state.stale_chain_fallback(),
            );
            finish_stream(
                &prompt.session_id,
                agent_prompt_id,
                prompt,
                &request,
                &backend,
                dispatch.state,
                dispatch.debug_capture,
                ws_pool_delta,
                execution.debug_provider_requests,
                execution.logical_attempt.provider_attempt(),
                writer,
            )?
        }
        CodexAttemptOutcome::Canceled { progress } => {
            if progress == CodexSemanticProgress::Parsed {
                emit_chat_completions_partial_clear(
                    agent_prompt_id,
                    prompt,
                    "request canceled; discarding partial provider output",
                    writer,
                )?;
            }
            return finish_canceled_attempt(
                agent_prompt_id,
                prompt,
                writer,
                false,
                observed_backend(
                    backend_reached.then(|| {
                        backend_descriptor(config, ProviderBackendTransport::Websocket, false)
                    }),
                    execution.prior_backend,
                ),
                tau_proto::ProviderAttempt::ONE,
            );
        }
        CodexAttemptOutcome::Terminal {
            error,
            progress,
            backend_reached,
        } if error.repetition().is_some() => {
            if progress == CodexSemanticProgress::Parsed {
                emit_chat_completions_partial_clear(
                    agent_prompt_id,
                    prompt,
                    "provider stream ended with an error; discarding partial output",
                    writer,
                )?;
            }
            let repetition = error.repetition().expect("guarded repetition");
            emit_repetition_detected_update(
                agent_prompt_id,
                &prompt.agent_id,
                &originator,
                repetition,
                writer,
            );
            let backend = backend_descriptor(config, transport_taken, false);
            let observed =
                observed_backend(backend_reached.then_some(backend), execution.prior_backend);
            finish_error(
                agent_prompt_id,
                prompt,
                observed.as_ref(),
                error,
                ws_pool_delta,
                execution.debug_provider_requests,
                execution.logical_attempt.provider_attempt(),
                writer,
            )?
        }
        CodexAttemptOutcome::Retry {
            decision,
            progress,
            live_detail,
            canonical_unauthorized,
        } => {
            if progress == CodexSemanticProgress::Parsed {
                emit_chat_completions_partial_clear(
                    agent_prompt_id,
                    prompt,
                    "provider stream interrupted after partial output; preparing retry",
                    writer,
                )?;
            }
            return Ok(Some(PromptAttemptRetry {
                decision,
                live_detail,
                canonical_unauthorized,
                terminal_backend: observed_backend(
                    backend_reached.then(|| {
                        backend_descriptor(config, ProviderBackendTransport::Websocket, false)
                    }),
                    execution.prior_backend,
                ),
            }));
        }
        CodexAttemptOutcome::Terminal {
            error,
            progress,
            backend_reached,
        } => {
            if progress == CodexSemanticProgress::Parsed {
                emit_chat_completions_partial_clear(
                    agent_prompt_id,
                    prompt,
                    "provider stream ended with an error; discarding partial output",
                    writer,
                )?;
            }
            let backend = backend_descriptor(config, transport_taken, false);
            let observed =
                observed_backend(backend_reached.then_some(backend), execution.prior_backend);
            finish_error(
                agent_prompt_id,
                prompt,
                observed.as_ref(),
                error,
                ws_pool_delta,
                execution.debug_provider_requests,
                execution.logical_attempt.provider_attempt(),
                writer,
            )?
        }
    }
    Ok(None)
}

fn emit_chatgpt_connecting_update<S: ProviderReportSink>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    agent_id: &tau_proto::AgentId,
    originator: &tau_proto::PromptOriginator,
    writer: &mut S,
) {
    let update = ProviderResponseUpdated {
        agent_prompt_id: agent_prompt_id.clone(),
        agent_id: agent_id.clone(),
        deltas: Vec::new(),
        compaction: None,
        status: Some(ProviderResponseStatusUpdate {
            text: "Connecting to provider…".to_owned(),
            clear_response: false,
            retry: None,
            native_tool: None,
        }),
        response_stats: None,
        originator: originator.clone(),
    };
    let _ = writer.send_report(HarnessInputMessage::emit_transient(
        Event::ProviderResponseUpdatedReported(update),
    ));
}

/// Samples ChatGPT streaming progress according to
/// `SPEC-provider-response-streaming`.
struct RateLimitedResponseUpdateEmitter {
    delta_emitter: CodexStreamDeltaEmitter,
    started_at: Instant,
    last_update_emitted_at: Option<Instant>,
    last_stats_sample: tau_proto::ProviderResponseStatsSample,
    emitted_non_empty_sample: bool,
    /// Immutable elapsed duration captured at the first qualifying parser
    /// state.
    first_semantic_output_elapsed: Option<Duration>,
    /// Last hosted-search activity state successfully published.
    web_search_active: Option<bool>,
    /// Last hosted-search lifecycle revision successfully published.
    web_search_lifecycle_revision: u64,
}

struct ResponseUpdateTarget<'a> {
    agent_prompt_id: &'a tau_proto::AgentPromptId,
    agent_id: &'a tau_proto::AgentId,
    originator: &'a tau_proto::PromptOriginator,
}

impl RateLimitedResponseUpdateEmitter {
    fn new() -> Self {
        Self::new_at(Instant::now())
    }

    fn new_at(started_at: Instant) -> Self {
        Self {
            delta_emitter: CodexStreamDeltaEmitter::default(),
            started_at,
            last_update_emitted_at: None,
            last_stats_sample: tau_proto::ProviderResponseStatsSample::default(),
            emitted_non_empty_sample: false,
            first_semantic_output_elapsed: None,
            web_search_active: None,
            web_search_lifecycle_revision: 0,
        }
    }

    /// Aligns public elapsed time to the backend's typed dispatch boundary.
    fn mark_dispatched(&mut self, dispatched_at: Instant) {
        if self.last_update_emitted_at.is_none() {
            self.started_at = dispatched_at;
        }
    }

    fn emit_if_due<S: ProviderReportSink>(
        &mut self,
        agent_prompt_id: &tau_proto::AgentPromptId,
        agent_id: &tau_proto::AgentId,
        originator: &tau_proto::PromptOriginator,
        state: &CodexStreamState,
        writer: &mut S,
    ) {
        let target = ResponseUpdateTarget {
            agent_prompt_id,
            agent_id,
            originator,
        };
        self.emit_at(&target, state, writer, Instant::now(), false);
    }

    fn emit_terminal_flush<S: ProviderReportSink>(
        &mut self,
        agent_prompt_id: &tau_proto::AgentPromptId,
        agent_id: &tau_proto::AgentId,
        originator: &tau_proto::PromptOriginator,
        state: &CodexStreamState,
        writer: &mut S,
    ) {
        let target = ResponseUpdateTarget {
            agent_prompt_id,
            agent_id,
            originator,
        };
        self.emit_at(&target, state, writer, Instant::now(), true);
    }

    fn emit_at<S: ProviderReportSink>(
        &mut self,
        target: &ResponseUpdateTarget<'_>,
        state: &CodexStreamState,
        writer: &mut S,
        now: Instant,
        terminal_flush: bool,
    ) {
        if self.first_semantic_output_elapsed.is_none() && state.has_timed_semantic_output() {
            self.first_semantic_output_elapsed =
                Some(now.saturating_duration_since(self.started_at));
        }
        let response_stats = self.response_stats_at(state, now);
        let first_non_empty_sample =
            !self.emitted_non_empty_sample && response_stats.current.response_bytes_received > 0;
        let native_tool = state
            .web_search_lifecycle()
            .and_then(|(revision, call_id, active)| {
                (self.web_search_lifecycle_revision < revision).then(|| {
                    tau_proto::ProviderNativeToolStatusUpdate {
                        call_id: call_id.to_owned(),
                        tool_name: tau_proto::ToolName::new("web_search"),
                        display: tau_proto::ToolUseState {
                            status: if active {
                                tau_proto::ToolUseStatus::InProgress
                            } else {
                                tau_proto::ToolUseStatus::Success
                            },
                            status_text: if active {
                                "pending".to_owned()
                            } else {
                                "ok".to_owned()
                            },
                            ..Default::default()
                        },
                        phase: if active {
                            tau_proto::ProviderNativeToolPhase::Started
                        } else {
                            tau_proto::ProviderNativeToolPhase::Completed
                        },
                    }
                })
            });
        if !terminal_flush
            && !first_non_empty_sample
            && native_tool.is_none()
            && self.last_update_emitted_at.map_or_else(
                || {
                    response_stats.current.response_bytes_received == 0
                        && now.saturating_duration_since(self.started_at)
                            < PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL
                },
                |last| now.saturating_duration_since(last) < PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL,
            )
        {
            return;
        }
        let activity_transition = if state.web_search_active() {
            (self.web_search_active != Some(true)).then_some(true)
        } else {
            (self.web_search_active == Some(true)).then_some(false)
        };
        let status = (activity_transition.is_some() || native_tool.is_some()).then(|| {
            ProviderResponseStatusUpdate {
                text: if state.web_search_active() {
                    "Searching web…".to_owned()
                } else {
                    "Web search complete…".to_owned()
                },
                clear_response: false,
                retry: None,
                native_tool: native_tool.clone(),
            }
        });
        if emit_chatgpt_stream_update(
            target,
            state,
            &mut self.delta_emitter,
            response_stats,
            status,
            writer,
        ) {
            self.last_stats_sample = response_stats.current;
            self.last_update_emitted_at = Some(now);
            self.emitted_non_empty_sample |= response_stats.current.response_bytes_received > 0;
            if let Some(active) = activity_transition {
                self.web_search_active = Some(active);
            }
            if native_tool.is_some()
                && let Some((revision, _, _)) = state.web_search_lifecycle()
            {
                self.web_search_lifecycle_revision = revision;
            }
        }
    }

    fn response_stats_at(&self, state: &CodexStreamState, now: Instant) -> ProviderResponseStats {
        let current = tau_proto::ProviderResponseStatsSample {
            response_bytes_received: state.response_bytes_received(),
            elapsed_micros: now
                .saturating_duration_since(self.started_at)
                .as_micros()
                .min(u128::from(u64::MAX)) as u64,
        };
        ProviderResponseStats {
            current,
            previous: self.last_stats_sample,
            first_semantic_output_elapsed_micros: self
                .first_semantic_output_elapsed
                .map(|duration| duration.as_micros().min(u128::from(u64::MAX)) as u64),
        }
    }
}

fn emit_chatgpt_stream_update<S: ProviderReportSink>(
    target: &ResponseUpdateTarget<'_>,
    state: &CodexStreamState,
    delta_emitter: &mut CodexStreamDeltaEmitter,
    response_stats: ProviderResponseStats,
    status: Option<ProviderResponseStatusUpdate>,
    writer: &mut S,
) -> bool {
    // RATE-LIMIT GUARDRAIL — DO NOT CALL THIS DIRECTLY FROM UPSTREAM CHUNKS.
    // provider.response_updated is a bus/event-log event, not a per-chunk
    // callback. After the first prompt update, progress/byte updates MUST be
    // batched and emitted no faster than PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL
    // (1s) per prompt. A byte change is NOT a reason to emit early. Only
    // `RateLimitedResponseUpdateEmitter` may bypass this for the first non-empty
    // progress sample and for a terminal flush immediately before the turn is
    // closed.
    let deltas = delta_emitter.deltas(state);
    let compaction = state.compaction_update();
    if deltas.is_empty()
        && compaction.is_none()
        && response_stats.current == response_stats.previous
        && status.is_none()
    {
        return false;
    }
    let Ok(()) = writer.send_report(HarnessInputMessage::emit_transient(
        Event::ProviderResponseUpdatedReported(ProviderResponseUpdated {
            agent_prompt_id: target.agent_prompt_id.clone(),
            agent_id: target.agent_id.clone(),
            deltas,
            compaction,
            status,
            response_stats: Some(response_stats),
            originator: target.originator.clone(),
        }),
    )) else {
        return false;
    };
    true
}

fn maybe_debug_submit_provider_response(
    session_id: &tau_proto::SessionId,
    response: &ProviderResponseFinished,
    debug_provider_requests: bool,
    capture: Option<&tau_provider_codex::CodexDebugCapture>,
) {
    tau_provider_codex::submit_response_debug(
        session_id,
        debug_provider_requests,
        response,
        capture,
    );
}

#[allow(clippy::too_many_arguments)]
fn finish_stream<S: ProviderReportSink>(
    session_id: &tau_proto::SessionId,
    agent_prompt_id: &tau_proto::AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    request: &CodexPrompt<'_>,
    backend: &ProviderBackend,
    state: CodexStreamState,
    debug_capture: tau_provider_codex::CodexDebugCapture,
    ws_pool_delta: Option<tau_proto::WsPoolDelta>,
    debug_provider_requests: bool,
    provider_attempt: tau_proto::ProviderAttempt,
    writer: &mut S,
) -> Result<(), Box<dyn Error>> {
    let token_counts = state.token_counts();
    let input_tokens = token_counts.input;
    let cached_tokens = token_counts.cached;
    let output_tokens = token_counts.output;
    tracing::debug!(
        target: LOG_TARGET,
        agent_prompt_id = %agent_prompt_id,
        input_tokens,
        cached_tokens,
        output_tokens,
        "provider response token usage"
    );
    let usage = state.usage();
    let provider_response_id = state.response_id().map(str::to_owned);
    let output_items = state.into_output_items();
    let finished = ProviderResponseFinished {
        automatic_compaction_decision: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: agent_prompt_id.clone(),
        agent_id: prompt.agent_id.clone(),
        stop_reason: stop_reason_from_output_items(&output_items),
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        output_items,
        originator: prompt.originator.clone(),
        usage,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: Some(backend.clone()),
        provider_response_id,
        ws_pool_delta,
        provider_attempt,
    };
    maybe_debug_submit_provider_response(
        session_id,
        &finished,
        debug_provider_requests,
        Some(&debug_capture),
    );
    let diagnostic = cache_miss_diagnostic(prompt, request, &finished);
    if let Some(diagnostic) = diagnostic {
        writer.send_report(HarnessInputMessage::emit_transient(
            Event::ProviderCacheMissDiagnosticReported(diagnostic),
        ))?;
    }
    writer.send_report(HarnessInputMessage::emit_transient(
        Event::ProviderResponseFinishedReported(finished),
    ))?;
    Ok(())
}

fn cache_miss_diagnostic(
    prompt: &tau_proto::AgentPromptCreated,
    request: &CodexPrompt<'_>,
    response: &ProviderResponseFinished,
) -> Option<ProviderCacheMissDiagnostic> {
    let previous_input_tokens = request.context.blocks.iter().rev().find_map(|block| {
        let tau_proto::ContextBlock::AssistantResponse(block) = block else {
            return None;
        };
        block
            .provider_response_id
            .as_ref()
            .and(block.usage.as_ref())
            .map(|usage| usage.prompt_sent_tokens)
    })?;
    let input_tokens = response.usage.as_ref()?.prompt_sent_tokens;
    let cached_tokens = response.usage.as_ref()?.prompt_cached_tokens;
    const PROMPT_CACHE_CHUNK_TOKENS: u64 = 512;
    let cacheable_input_tokens = previous_input_tokens.min(input_tokens);
    let cacheable_input_tokens =
        cacheable_input_tokens / PROMPT_CACHE_CHUNK_TOKENS * PROMPT_CACHE_CHUNK_TOKENS;
    if cacheable_input_tokens == 0 || cacheable_input_tokens < cached_tokens.saturating_mul(2) {
        return None;
    }
    Some(ProviderCacheMissDiagnostic {
        agent_prompt_id: response.agent_prompt_id.clone(),
        model: prompt.model.clone(),
        originator: response.originator.clone(),
        tool_choice: request.tool_choice,
        ws_pool_delta: response.ws_pool_delta,
        input_tokens,
        cached_tokens,
        previous_input_tokens,
        cacheable_input_tokens,
        corrected_cache_efficiency: cached_tokens as f32 / cacheable_input_tokens as f32,
    })
}

#[allow(clippy::too_many_arguments)]
fn finish_error<S: ProviderReportSink>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    backend: Option<&ProviderBackend>,
    error: CodexError,
    ws_pool_delta: Option<tau_proto::WsPoolDelta>,
    debug_provider_requests: bool,
    provider_attempt: tau_proto::ProviderAttempt,
    writer: &mut S,
) -> Result<(), Box<dyn Error>> {
    let finished = ProviderResponseFinished {
        automatic_compaction_decision: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: agent_prompt_id.clone(),
        agent_id: prompt.agent_id.clone(),
        output_items: Vec::new(),
        stop_reason: if error.repetition().is_some() {
            ProviderStopReason::RepetitionDetected
        } else {
            ProviderStopReason::Error
        },
        error: Some(bounded_provider_error(&format!("LLM error: {error}"))),
        failure_kind: error.failure_kind(),
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        originator: prompt.originator.clone(),
        usage: None,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: backend.cloned(),
        provider_attempt,
        provider_response_id: None,
        ws_pool_delta,
    };
    maybe_debug_submit_provider_response(
        &prompt.session_id,
        &finished,
        debug_provider_requests,
        None,
    );
    writer.send_report(HarnessInputMessage::emit_transient(
        Event::ProviderResponseFinishedReported(finished),
    ))?;
    Ok(())
}

fn emit_repetition_detected_update<S: ProviderReportSink>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    agent_id: &tau_proto::AgentId,
    originator: &tau_proto::PromptOriginator,
    repetition: &tau_provider::StreamRepetition,
    writer: &mut S,
) {
    let text = bounded_provider_error(&format!(
        "provider stream repetition detected; aborting response ({repetition})"
    ));
    let _ = writer.send_report(HarnessInputMessage::emit_transient(
        Event::ProviderResponseUpdatedReported(ProviderResponseUpdated {
            agent_prompt_id: agent_prompt_id.clone(),
            agent_id: agent_id.clone(),
            deltas: Vec::new(),
            compaction: None,
            status: Some(ProviderResponseStatusUpdate {
                text,
                clear_response: true,
                retry: None,
                native_tool: None,
            }),
            response_stats: None,
            originator: originator.clone(),
        }),
    ));
}

fn bounded_provider_error(text: &str) -> String {
    const MAX_CHARS: usize = 512;
    let mut out = text.chars().take(MAX_CHARS).collect::<String>();
    if text.chars().nth(MAX_CHARS).is_some() {
        out.push('…');
    }
    out
}

#[cfg(test)]
fn models_for_auth(auth: &OpenAiAuth) -> Vec<ProviderModelInfo> {
    models_for_profiles(&profiles_with_chatgpt_auth(auth.clone()))
}

fn models_for_profiles(profiles: &BuiltinProviderProfiles) -> Vec<ProviderModelInfo> {
    let mut models = Vec::new();
    for (provider_name, profile) in &profiles.providers {
        match profile {
            BuiltinProviderProfile::Chatgpt(profile) => {
                models.extend(tau_provider_codex::models_for_provider_mode(
                    provider_name,
                    profile.responses_mode(),
                ));
            }
            BuiltinProviderProfile::ChatCompletions(provider) => {
                models.extend(chat_models_for_provider(provider_name, provider));
            }
            BuiltinProviderProfile::OpenRouter(profile) => {
                let provider = profile.to_chat_completions();
                models.extend(chat_models_for_provider(provider_name, &provider));
            }
            BuiltinProviderProfile::Responses(provider) => {
                models.extend(responses_models_for_provider(provider_name, provider));
            }
        }
    }
    models
}

/// Replaces one provider's models while preserving every sibling contribution
/// and deterministic provider/model ordering from a complete declaration.
fn replace_provider_models(
    previous: &[ProviderModelInfo],
    provider: &ProviderName,
    selected_profiles: &BuiltinProviderProfiles,
) -> Vec<ProviderModelInfo> {
    let mut models = previous
        .iter()
        .filter(|model| &model.id.provider != provider)
        .cloned()
        .collect::<Vec<_>>();
    models.extend(models_for_profiles(selected_profiles));
    models.sort_by(|left, right| left.id.provider.cmp(&right.id.provider));
    models
}

fn apply_compact_route_downgrades(
    models: &mut [ProviderModelInfo],
    identities: &HashMap<ProviderName, InferenceProfileIdentity>,
    unavailable: &HashSet<InferenceProfileIdentity>,
) {
    for model in models {
        if identities
            .get(&model.id.provider)
            .is_some_and(|identity| unavailable.contains(identity))
            && model.supports_standalone_compaction
        {
            model.supports_standalone_compaction = false;
            model.standalone_compaction_generation_negative = true;
            model.standalone_compaction_threshold = None;
        }
    }
}

#[cfg(test)]
mod openai_tests;
#[cfg(test)]
mod scheduler_model_tests;
#[cfg(test)]
mod tests;
