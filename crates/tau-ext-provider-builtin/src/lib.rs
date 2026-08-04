//! Built-in provider registry extension.
//!
//! This crate owns Tau's built-in provider process, registration CLI, scoped
//! credential hydration, model publication, and dispatch across built-in
//! provider backends. Individual backend crates own provider-specific wire
//! formats. Component responsibilities and trust boundaries are summarized in
//! `ARCH-tau-ext-provider-builtin`.

use std::collections::hash_map as path_std_collections_hash_map;
use std::io as path_std_io;

mod chat_completions;
mod credential_record;
mod oauth_refresh_rejection;
mod prewarm;
#[cfg(feature = "quota-test-support")]
mod quota_test_support;
mod responses;
mod setup_store;

use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet, VecDeque};
use std::error::Error;
use std::hash::{Hash, Hasher};
use std::io::{BufWriter, Cursor, Read, Write};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub use chat_completions::{
    ChatCompletionsCompat, ChatCompletionsModel, ChatCompletionsProvider,
    LocalSummaryCompactionConfig, LocalSummaryCompactionSerializationProfile,
    OpenRouterDiscoveryError, OpenRouterProfile,
};
use chat_completions::{
    PromptAttemptOutcome as ChatCompletionsAttemptOutcome, fetch_openrouter_models,
    models_for_provider as chat_models_for_provider, run_prompt_attempt,
};
use dialoguer::{Confirm, Input, Password, Select};
use oauth_refresh_rejection::{OAuthRefreshRejectionCache, RefreshCredentialsError};
use prewarm::{PrewarmAbort, PrewarmKey, PrewarmSupervisor};
#[cfg(feature = "quota-test-support")]
pub use quota_test_support::run_quota_recovery_fixture;
use responses::{
    PromptAttemptOutcome as ResponsesAttemptOutcome,
    models_for_provider as responses_models_for_provider,
    run_prompt_attempt as run_responses_prompt_attempt,
};
pub use responses::{ResponsesEfforts, ResponsesModel, ResponsesProvider};
use serde::{Deserialize, Serialize};
use tau_client::{
    ClientError, ClientHandle, ClientResult, DispatchOutcome, ExtensionBuilder,
    ExtensionDataClient, ManualExtensionRuntime, ManualRuntimePoll, ManualRuntimeWaker,
    RawEventContext, TauExtension, TauExtensionRunner,
};
use tau_config::provider_settings::{
    ProviderCredentialReference, ProviderCredentialSlot, parse_provider_credential_reference,
};
use tau_config::settings::BuiltinComponentIdentity;
use tau_proto::{
    ClientKind, ContextItem, Event, EventName, HarnessInputMessage, HarnessInputReader, ModelId,
    ModelName, PeerOutputWriter, ProviderBackend, ProviderBackendKind, ProviderBackendTransport,
    ProviderCacheMissDiagnostic, ProviderModelInfo, ProviderModelsDeclared, ProviderName,
    ProviderPromptSubmitted, ProviderResponseFinished, ProviderResponseStats,
    ProviderResponseStatusUpdate, ProviderResponseUpdated, ProviderStopReason, SecretValue,
};
use tau_provider::retry_policy::{RetryClass, RetryDecision};
use tau_provider_codex::{
    AttemptOutcome as CodexAttemptOutcome, CodexError, CodexMode, CodexRuntime, CompactOutcome,
    InferenceProfileIdentity, PrewarmOutcome, Prompt as CodexPrompt, QuotaProfileIdentity,
    ResolvedConfig, SemanticProgress as CodexSemanticProgress,
    StreamDeltaEmitter as CodexStreamDeltaEmitter, StreamState as CodexStreamState, StreamUpdate,
    TurnAbort, TurnAbortWaker,
};
pub use tau_provider_responses::Transport as ResponsesTransport;

/// `tracing` target for events emitted from this extension.
pub const LOG_TARGET: &str = "provider-builtin";

const EXTENSION_NAME: &str = "tau-ext-provider-builtin";
const CHATGPT_PROVIDER_NAME: &str = "chatgpt";
const DEFAULT_RESPONSES_LITE_COMPATIBILITY: bool = false;

/// Immutable credential-free provider settings captured from initial Configure.
type SettingsSnapshot = Arc<Mutex<BTreeMap<String, Vec<u8>>>>;

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

/// Registered built-in provider profiles keyed by filename-derived namespace.
#[derive(Clone, Debug, Default)]
pub struct BuiltinProviderProfiles {
    providers: BTreeMap<ProviderName, BuiltinProviderProfile>,
    credential_paths: BTreeMap<ProviderName, tau_proto::ExtensionDataPath>,
    /// API-key profiles whose empty record means an unavailable named source,
    /// rather than an intentionally keyless direct-entry profile.
    named_api_key_profiles: HashSet<ProviderName>,
}

impl BuiltinProviderProfiles {
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
const PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL: Duration = Duration::from_secs(1);
const QUOTA_FETCH_MIN_INTERVAL: Duration = Duration::from_secs(60);
const QUOTA_REFRESH_INTERVAL: Duration = Duration::from_secs(15 * 60);

#[derive(Clone)]
struct QuotaWindowRecord {
    window: tau_proto::ProviderQuotaWindow,
    updated_sequence: u64,
}

struct QuotaProfileState {
    identity: QuotaProfileIdentity,
    epoch: tau_proto::ProviderQuotaEpoch,
    sequence: u64,
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
                sequence: 1,
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
                sequence: 1,
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
                sequence: current.sequence.saturating_add(1),
            },
        ))
    }

    fn refresh_delay(&self, provider: &ProviderName) -> Duration {
        let now = now_ms();
        self.profiles
            .get(provider)
            .into_iter()
            .flat_map(|current| current.windows.values())
            .filter_map(|record| {
                let (remaining, anchor) = (
                    record.window.remaining_seconds_at_timing_anchor?,
                    record.window.timing_anchor_observed_at_unix_ms?,
                );
                let age = i64::try_from(now.saturating_sub(anchor).div_ceil(1_000)).ok()?;
                u64::try_from(remaining.saturating_sub(age)).ok()
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
    ) -> Option<(tau_proto::ProviderQuotaEpoch, u64)> {
        let current = self.profiles.get_mut(provider)?;
        let reset_due = current.windows.values().any(|record| {
            record
                .window
                .remaining_seconds_at_timing_anchor
                .zip(record.window.timing_anchor_observed_at_unix_ms)
                .is_some_and(|(remaining, anchor)| {
                    let age_seconds = now_ms().saturating_sub(anchor).div_ceil(1_000);
                    u64::try_from(remaining)
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
        fetch_start_sequence: u64,
        snapshot: tau_provider_codex::FullQuotaSnapshot,
        observed_at_unix_ms: u64,
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
                    updated_sequence: current.sequence.saturating_add(1),
                },
            );
        }
        if candidate.len() > tau_proto::MAX_PROVIDER_QUOTA_WINDOWS {
            current.failure_attempt = current.failure_attempt.saturating_add(1);
            return None;
        }
        current.sequence = current.sequence.saturating_add(1);
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
        observed_at_unix_ms: u64,
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
        current.sequence = current.sequence.saturating_add(1);
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
    debug_assert!(matches!(
        event,
        Event::ProviderQuotaReplaceReported(_)
            | Event::ProviderQuotaPatchReported(_)
            | Event::ProviderQuotaClearReported(_)
    ));
    HarnessInputMessage::emit_with_persist(event, false)
}

fn quota_profile_identity(config: &ResolvedConfig) -> QuotaProfileIdentity {
    config.profile_identity()
}

fn responses_profile_identity(config: &ResolvedConfig) -> InferenceProfileIdentity {
    config.inference_identity()
}

fn backend_profile_identity(backend: &PromptBackend) -> Option<u64> {
    let mut hasher = path_std_collections_hash_map::DefaultHasher::new();
    match backend {
        PromptBackend::Unavailable => return None,
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
    Some(hasher.finish())
}

fn full_quota_window(
    observation: tau_provider_codex::QuotaWindowObservation,
    observed_at_unix_ms: u64,
) -> Option<tau_proto::ProviderQuotaWindow> {
    let window_seconds = observation.window_seconds?;
    let server_offset_ms = match (
        observation.reset_at_unix_seconds,
        observation.remaining_seconds,
    ) {
        (Some(reset), Some(remaining)) => i128::from(reset)
            .checked_sub(i128::from(remaining))?
            .checked_mul(1_000)?
            .checked_sub(i128::from(observed_at_unix_ms))?
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
        server_offset_ms,
        server_offset_observed_at_unix_ms: server_offset_ms.map(|_| observed_at_unix_ms),
    })
}

fn merge_sparse_quota_window(
    previous: Option<&tau_proto::ProviderQuotaWindow>,
    sparse: tau_provider_codex::QuotaWindowObservation,
    observed_at_unix_ms: u64,
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
        if old.abs_diff(new) <= 60 {
            return true;
        }
        if new < old {
            return false;
        }
        let old_remaining = previous
            .remaining_seconds_at_timing_anchor
            .zip(previous.timing_anchor_observed_at_unix_ms)
            .map(|(remaining, anchor)| {
                let age_seconds =
                    i64::try_from(observed_at_unix_ms.saturating_sub(anchor).div_ceil(1_000))
                        .unwrap_or(i64::MAX);
                remaining.saturating_sub(age_seconds)
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
const DEFAULT_PROMPT_CONCURRENCY: usize = 4;

/// Environment override for prompt execution concurrency.
const PROMPT_CONCURRENCY_ENV: &str = "TAU_BUILTIN_PROVIDER_PROMPT_CONCURRENCY";

/// Runs setup commands for registered built-in provider profiles.
pub fn run_provider_cli(args: &[String]) -> Result<(), Box<dyn Error>> {
    let (extension_instance, args) = provider_cli_target(args)?;
    let network = Arc::new(tau_provider::OutboundNetworkPolicy::from_env());
    match args.first().map(String::as_str).unwrap_or("help") {
        "add" => cmd_add(&args[1..], &network, &extension_instance)?,
        "remove" | "delete" => cmd_remove(args.get(1).map(String::as_str), &extension_instance)?,
        "list" | "status" => cmd_list(&extension_instance)?,
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
  add [KIND]                     Add or replace a provider profile
  remove <name>                  Remove a provider profile
  list                           List provider profiles

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
    let kind = match args {
        [] => {
            let labels = PROVIDER_KINDS
                .iter()
                .map(|kind| kind.label)
                .collect::<Vec<_>>();
            PROVIDER_KINDS[Select::new()
                .with_prompt("Provider kind")
                .items(&labels)
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
        "chatgpt" => cmd_add_chatgpt(network, extension_instance)?,
        "chat-completions" => cmd_add_chat_completions(extension_instance)?,
        "responses" => cmd_add_responses(extension_instance)?,
        "openrouter" => cmd_add_openrouter(network, extension_instance)?,
        _ => unreachable!("provider kind came from the closed descriptor table"),
    }
    Ok(())
}

fn cmd_add_responses(extension_instance: &tau_proto::ExtensionName) -> Result<(), Box<dyn Error>> {
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
    let transport_options = ["sse", "websocket"];
    let default_transport =
        usize::from(recommended_responses_transport(&base_url) == ResponsesTransport::Websocket);
    let transport = Select::new()
        .with_prompt("Transport")
        .items(&transport_options)
        .default(default_transport)
        .interact()?;
    let transport = match transport {
        0 => ResponsesTransport::Sse,
        1 => ResponsesTransport::Websocket,
        _ => unreachable!("dialoguer returns an offered transport"),
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
        }),
        api_key_source,
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
) -> Result<(), Box<dyn Error>> {
    let name = prompt_provider_name("chatgpt")?;
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
        ApiKeySource::Keyless,
    )?;
    Ok(())
}

fn cmd_add_chat_completions(
    extension_instance: &tau_proto::ExtensionName,
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
        api_key_source,
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
        api_key_source,
    )?;
    Ok(())
}

fn cmd_remove(
    name_arg: Option<&str>,
    extension_instance: &tau_proto::ExtensionName,
) -> Result<(), Box<dyn Error>> {
    let name = match name_arg {
        Some(name) => ProviderName::try_new(name.trim().to_owned())
            .map_err(|error| format!("invalid provider namespace '{name}': {error}"))?,
        None => prompt_provider_name(CHATGPT_PROVIDER_NAME)?,
    };
    if setup_store::SetupStore::open_default()?.remove(extension_instance, &name)? {
        eprintln!("Removed provider profile '{name}'.");
    } else {
        eprintln!("Provider profile '{name}' was not configured.");
    }
    Ok(())
}

fn cmd_list(extension_instance: &tau_proto::ExtensionName) -> Result<(), Box<dyn Error>> {
    let store = setup_store::SetupStore::open_default()?;
    let setup_store::SetupSnapshot {
        settings,
        credentials,
    } = store.snapshot(extension_instance)?;
    let profiles = load_settings_profiles(settings);
    if profiles.providers.is_empty() {
        println!("No provider profiles configured.");
        return Ok(());
    }
    for (name, profile) in profiles.providers {
        match profile {
            BuiltinProviderProfile::Chatgpt(profile) => {
                let status = match credentials
                    .get(&(name.clone(), ProviderCredentialSlot::OAuth))
                    .and_then(|bytes| {
                        serde_json::from_slice::<credential_record::ChatGptOAuthCredential>(bytes)
                            .ok()
                    }) {
                    Some(record) if record.is_unexpired(now_ms()) => "logged-in",
                    Some(_) => "expired",
                    _ => "not-configured",
                };
                let mode = if profile.responses_lite_compatibility {
                    "responses-lite-compatibility"
                } else {
                    "responses-standard"
                };
                println!("{name}\tchatgpt\t{status}\t{mode}");
            }
            BuiltinProviderProfile::ChatCompletions(provider) => {
                let auth_status = setup_api_key_status(&credentials, &name);
                let models = provider
                    .models
                    .iter()
                    .map(|model| model.id.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                println!(
                    "{name}\tchat_completions\t{}\t{models}\t{auth_status}",
                    provider.base_url
                );
            }
            BuiltinProviderProfile::OpenRouter(profile) => {
                let auth_status = setup_api_key_status(&credentials, &name);
                let models = profile
                    .models
                    .iter()
                    .map(|model| model.id.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                println!(
                    "{name}\topenrouter\thttps://openrouter.ai/api/v1\t{models}\t{auth_status}"
                );
            }
            BuiltinProviderProfile::Responses(provider) => {
                let auth_status = setup_api_key_status(&credentials, &name);
                let models = provider
                    .models
                    .iter()
                    .map(|model| model.id.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                println!(
                    "{name}\tresponses\t{}\t{models}\t{auth_status}",
                    provider.base_url
                );
            }
        }
    }
    Ok(())
}

fn setup_api_key_status(
    credentials: &std::collections::BTreeMap<(ProviderName, ProviderCredentialSlot), Vec<u8>>,
    provider: &ProviderName,
) -> &'static str {
    match credentials
        .get(&(provider.clone(), ProviderCredentialSlot::ApiKey))
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
            context_window: 128_000,
            compat: None,
            tags: Vec::new(),
            supports_parallel_tool_calls: true,
            local_summary_compaction: None,
            est_uncached_input_cost_1m_usd: None,
            est_cached_input_cost_1m_usd: None,
            est_output_cost_1m_usd: None,
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
                    efforts: None,
                    display_name: None,
                    context_window: 128_000,
                    tags: Vec::new(),
                    supports_parallel_tool_calls: true,
                    est_uncached_input_cost_1m_usd: None,
                    est_cached_input_cost_1m_usd: None,
                    est_output_cost_1m_usd: None,
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
    Named {
        /// Exact configured source name serialized into provider settings.
        name: String,
        /// Declaration captured from the targeted extension configuration.
        declaration: tau_config::settings::ExtensionSecretEntry,
    },
}

impl ApiKeySource {
    /// Return the setup-time value or the empty materialization placeholder.
    fn value(&self) -> String {
        match self {
            Self::Direct(value) => value.expose_secret().to_owned(),
            Self::Keyless | Self::Named { .. } => String::new(),
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
    if !named_secrets.is_empty() {
        choices.push("Use configured named secret");
    }
    if allow_keyless {
        choices.push("No API key");
    }
    let selected = choices[Select::new()
        .with_prompt("API key source")
        .items(&choices)
        .interact()?];
    match selected {
        "Enter API key now" => Ok(ApiKeySource::Direct(SecretValue::new(
            Password::new().with_prompt("API key").interact()?,
        ))),
        "Use configured named secret" => {
            let names = named_secrets
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>();
            let index = Select::new()
                .with_prompt("Configured named secret")
                .items(&names)
                .interact()?;
            let (name, declaration) = named_secrets[index].clone();
            Ok(ApiKeySource::Named { name, declaration })
        }
        "No API key" => Ok(ApiKeySource::Keyless),
        _ => unreachable!("API-key source came from the offered picker choices"),
    }
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
    api_key_source: ApiKeySource,
) -> Result<(), Box<dyn Error>> {
    let ProviderSetupPayload {
        settings,
        slot,
        secret,
        named_source,
    } = provider_setup_payload(name, profile, api_key_source)?;
    let settings_path =
        setup_store::SetupStore::open_default()?.apply(&setup_store::ProviderSetupPlan {
            extension_instance: extension_instance.clone(),
            provider: name.clone(),
            settings,
            secret: setup_store::SecretWrite {
                path: slot.path(name),
                contents: setup_store::SecretBytes::new(secret),
            },
            named_source,
        })?;
    eprintln!(
        "Provider '{name}' registered for extension '{extension_instance}'. Settings: {}",
        settings_path.display()
    );
    eprintln!("Restart Tau for settings changes to take effect.");
    Ok(())
}

/// Credential-free settings and one closed typed-secret publication plan.
struct ProviderSetupPayload {
    /// Credential-free provider settings.
    settings: Vec<u8>,
    /// Closed credential family and exact path authority.
    slot: ProviderCredentialSlot,
    /// Complete serialized typed credential record.
    secret: Vec<u8>,
    /// Named source resolved only after the instance lifecycle lock is held.
    named_source: Option<setup_store::NamedSecretSource>,
}

fn provider_setup_payload(
    name: &ProviderName,
    profile: &BuiltinProviderProfile,
    api_key_source: ApiKeySource,
) -> Result<ProviderSetupPayload, Box<dyn Error>> {
    use credential_record::{ApiKeyCredential, ChatGptOAuthCredential};

    let mut settings = serde_json::to_value(profile)?;
    let object = settings
        .as_object_mut()
        .ok_or("provider settings must serialize as an object")?;
    let (slot, secret) = match profile {
        BuiltinProviderProfile::Chatgpt(profile) => {
            object.remove("auth");
            (
                ProviderCredentialSlot::OAuth,
                serde_json::to_vec(&ChatGptOAuthCredential::from(profile.auth.clone()))?,
            )
        }
        BuiltinProviderProfile::ChatCompletions(profile) => {
            object.remove("api_key");
            object.remove("api_key_secret");
            (
                ProviderCredentialSlot::ApiKey,
                serde_json::to_vec(&ApiKeyCredential::new(profile.api_key.clone()))?,
            )
        }
        BuiltinProviderProfile::OpenRouter(profile) => {
            object.remove("api_key");
            object.remove("api_key_secret");
            (
                ProviderCredentialSlot::ApiKey,
                serde_json::to_vec(&ApiKeyCredential::new(profile.api_key.clone()))?,
            )
        }
        BuiltinProviderProfile::Responses(profile) => {
            object.remove("api_key");
            object.remove("api_key_secret");
            (
                ProviderCredentialSlot::ApiKey,
                serde_json::to_vec(&ApiKeyCredential::new(profile.api_key.clone()))?,
            )
        }
    };
    let reference = ProviderCredentialReference::new(
        name,
        slot,
        match &api_key_source {
            ApiKeySource::Named { name, .. } => Some(name.as_str()),
            ApiKeySource::Direct(_) | ApiKeySource::Keyless => None,
        },
    )?;
    object.insert("credential".to_owned(), reference.to_value());
    Ok(ProviderSetupPayload {
        settings: serde_json::to_vec_pretty(&settings)?,
        slot,
        secret,
        named_source: match api_key_source {
            ApiKeySource::Named { name, declaration } => {
                Some(setup_store::NamedSecretSource { name, declaration })
            }
            ApiKeySource::Direct(_) | ApiKeySource::Keyless => None,
        },
    })
}

fn load_settings_profiles(files: Vec<(ProviderName, Vec<u8>)>) -> BuiltinProviderProfiles {
    let mut profiles = BuiltinProviderProfiles::default();
    for (name, contents) in files {
        match parse_settings_profile(&name, &contents) {
            Ok((profile, credential_path, named_api_key)) => {
                profiles
                    .credential_paths
                    .insert(name.clone(), credential_path);
                if named_api_key {
                    profiles.named_api_key_profiles.insert(name.clone());
                }
                profiles.providers.insert(name, profile);
            }
            Err(error) => {
                tracing::warn!(
                    target: LOG_TARGET,
                    provider = %name,
                    error = %error,
                    "skipping invalid credential-free provider settings"
                );
            }
        }
    }
    profiles
}

fn parse_settings_profile(
    name: &ProviderName,
    contents: &[u8],
) -> Result<(BuiltinProviderProfile, tau_proto::ExtensionDataPath, bool), Box<dyn Error>> {
    let mut value: serde_json::Value = serde_json::from_slice(contents)?;
    let object = value
        .as_object_mut()
        .ok_or("provider settings must be an object")?;
    let reference = parse_provider_credential_reference(name, object)?;
    object
        .remove("credential")
        .expect("validated reference must be present");
    match reference.slot() {
        ProviderCredentialSlot::OAuth => {
            object.insert("auth".to_owned(), serde_json::json!({}));
        }
        ProviderCredentialSlot::ApiKey => {
            object.insert("api_key".to_owned(), serde_json::json!(""));
        }
    }
    let profile: BuiltinProviderProfile = serde_json::from_value(value)?;
    let profile_matches_kind = matches!(
        (&profile, reference.slot()),
        (
            BuiltinProviderProfile::Chatgpt(_),
            ProviderCredentialSlot::OAuth
        ) | (
            BuiltinProviderProfile::ChatCompletions(_)
                | BuiltinProviderProfile::OpenRouter(_)
                | BuiltinProviderProfile::Responses(_),
            ProviderCredentialSlot::ApiKey
        )
    );
    if !profile_matches_kind {
        return Err("provider credential kind does not match provider settings kind".into());
    }
    Ok((
        profile,
        reference.path().clone(),
        reference.named_source().is_some(),
    ))
}

fn hydrate_profile_credentials(
    client: &ExtensionDataClient,
    profiles: &mut BuiltinProviderProfiles,
) {
    let names = profiles.providers.keys().cloned().collect::<Vec<_>>();
    for name in names {
        let Some(path) = profiles.credential_paths.get(&name).cloned() else {
            profiles.providers.remove(&name);
            continue;
        };
        let result = client.request(
            tau_proto::ExtensionDataScope::Secret,
            tau_proto::ExtensionDataRequestOp::ReadFile { path },
        );
        let tau_proto::ExtensionDataValue::ReadFile { contents } = (match result {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(
                    target: LOG_TARGET,
                    provider = %name,
                    error = %error,
                    "skipping provider with unavailable credential"
                );
                profiles.providers.remove(&name);
                continue;
            }
        }) else {
            tracing::warn!(
                target: LOG_TARGET,
                provider = %name,
                "skipping provider after unexpected credential result"
            );
            profiles.providers.remove(&name);
            continue;
        };
        let valid = match profiles.providers.get_mut(&name) {
            Some(BuiltinProviderProfile::Chatgpt(profile)) => {
                serde_json::from_slice::<credential_record::ChatGptOAuthCredential>(&contents)
                    .map_err(|_| ())
                    .map(credential_record::ChatGptOAuthCredential::into_auth)
                    .map(|auth| profile.auth = auth)
            }
            Some(BuiltinProviderProfile::ChatCompletions(profile)) => serde_json::from_slice::<
                credential_record::ApiKeyCredential,
            >(&contents)
            .map_err(|_| ())
            .map(credential_record::ApiKeyCredential::into_value)
            .and_then(|value| {
                (!profiles.named_api_key_profiles.contains(&name) || !value.trim().is_empty())
                    .then_some(value)
                    .ok_or(())
            })
            .map(|value| profile.api_key = value),
            Some(BuiltinProviderProfile::OpenRouter(profile)) => serde_json::from_slice::<
                credential_record::ApiKeyCredential,
            >(&contents)
            .map_err(|_| ())
            .map(credential_record::ApiKeyCredential::into_value)
            .and_then(|value| {
                (!profiles.named_api_key_profiles.contains(&name) || !value.trim().is_empty())
                    .then_some(value)
                    .ok_or(())
            })
            .map(|value| profile.api_key = value),
            Some(BuiltinProviderProfile::Responses(profile)) => serde_json::from_slice::<
                credential_record::ApiKeyCredential,
            >(&contents)
            .map_err(|_| ())
            .map(credential_record::ApiKeyCredential::into_value)
            .and_then(|value| {
                (!profiles.named_api_key_profiles.contains(&name) || !value.trim().is_empty())
                    .then_some(value)
                    .ok_or(())
            })
            .map(|value| profile.api_key = value),
            None => continue,
        };
        if valid.is_err() {
            tracing::warn!(
                target: LOG_TARGET,
                provider = %name,
                "skipping provider with invalid version-zero credential"
            );
            profiles.providers.remove(&name);
        }
    }
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
    let settings_snapshot = Arc::new(Mutex::new(BTreeMap::new()));
    let profile_snapshot = Arc::clone(&settings_snapshot);
    run_inner_with_configured_settings(
        reader,
        writer,
        BuiltinProviderProfiles::default(),
        move || {
            let settings = profile_snapshot
                .lock()
                .expect("lock provider settings snapshot");
            let files = settings
                .iter()
                .filter_map(|(name, contents)| {
                    let stem = name.strip_suffix(".json")?;
                    let name = ProviderName::try_new(stem.to_owned()).ok()?;
                    Some((name, contents.clone()))
                })
                .collect();
            load_settings_profiles(files)
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
    run_inner(reader, writer, profiles, move || prompt_profiles.clone())
}

#[cfg(test)]
fn profiles_with_chatgpt_auth(auth: OpenAiAuth) -> BuiltinProviderProfiles {
    let mut providers = BTreeMap::new();
    providers.insert(
        ProviderName::new(CHATGPT_PROVIDER_NAME),
        BuiltinProviderProfile::Chatgpt(ChatGptProfile {
            auth,
            responses_lite_compatibility: false,
        }),
    );
    BuiltinProviderProfiles {
        providers,
        ..Default::default()
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
    F: FnMut() -> BuiltinProviderProfiles + 'static,
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
    F: FnMut() -> BuiltinProviderProfiles + 'static,
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
    F: FnMut() -> BuiltinProviderProfiles + 'static,
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
    F: FnMut() -> BuiltinProviderProfiles + 'static,
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
            settings_snapshot: Arc::new(Mutex::new(BTreeMap::new())),
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
    /// Immutable credential-free settings snapshot from initial Configure.
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
    F: FnMut() -> BuiltinProviderProfiles + 'static,
{
    run_inner_with_executors_and_clock_with_settings(
        reader,
        writer,
        load_prompt_profiles,
        prompt_concurrency_limit,
        executors,
        RuntimeStartup {
            profiles: startup_profiles,
            settings_snapshot: Arc::new(Mutex::new(BTreeMap::new())),
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
    F: FnMut() -> BuiltinProviderProfiles + 'static,
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
        retry_clock: executors.retry_clock,
        shared_cooldowns: BTreeMap::new(),
        shared_cooldown_generation: 0,
        codex_runtime: Arc::new(CodexRuntime::new(network)),
        prewarm_supervisor: PrewarmSupervisor::default(),
        provider_profile_identities: BTreeMap::new(),
        prewarm_profile_identities: BTreeMap::new(),
        cancellation: Arc::new(CancellationState::default()),
        prompt_queue: VecDeque::new(),
        session_debug_allowed: BTreeMap::new(),
        active_prompts: 0,
        input_closed: false,
        cancel_generation: 0,
        quota: QuotaCoordinator::default(),
        oauth_refresh_rejections: OAuthRefreshRejectionCache::default(),
        extension_data_client: None,
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
    F: FnMut() -> BuiltinProviderProfiles + 'static,
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
                *settings_snapshot
                    .lock()
                    .expect("lock provider settings snapshot") =
                    cx.configure.settings_files.clone();
                if !publish_models_after_configure {
                    return Ok(());
                }
                let profiles = cx.state.load_profiles();
                cx.state
                    .set_startup_responses_modes(profiles.startup_responses_modes());
                cx.handle
                    .emit_transient(Event::ProviderModelsDeclared(ProviderModelsDeclared {
                        models: models_for_profiles(&profiles),
                    }))
            })
            .on_raw_live(
                tau_proto::EventSelector::Exact(EventName::AGENT_PROMPT_PREWARM_REQUESTED),
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
    F: FnMut() -> BuiltinProviderProfiles + 'static,
{
    cx.state.handle_event(cx.event().clone(), &cx.handle())
}

fn run_provider_loop<F>(
    mut runtime: ManualExtensionRuntime<ProviderRuntime<F>>,
) -> Result<(), Box<dyn Error>>
where
    F: FnMut() -> BuiltinProviderProfiles + 'static,
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

        if !handled_input {
            runtime.wait_for_wake();
        }
    }
}

/// Live provider event loop state after the Tau extension handshake completes.
struct ProviderRuntime<F> {
    /// Reloads provider profiles for prompt-time auth/model resolution.
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
    provider_profile_identities: BTreeMap<ProviderName, Option<u64>>,
    /// Last Responses identity used to supervise prewarm transport state.
    prewarm_profile_identities: BTreeMap<ProviderName, InferenceProfileIdentity>,
    /// Cooperative cancellation state shared with prompt workers.
    cancellation: Arc<CancellationState>,
    /// Prompt jobs accepted while all worker slots were occupied.
    prompt_queue: VecDeque<PromptJob>,
    /// Per-session decision on whether provider debug captures may be written.
    session_debug_allowed: BTreeMap<tau_proto::SessionId, bool>,
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
    /// Runtime Secret-scope RPC client, installed after startup transport
    /// setup.
    extension_data_client: Option<ExtensionDataClient>,
}

impl<F> ProviderRuntime<F>
where
    F: FnMut() -> BuiltinProviderProfiles + 'static,
{
    fn load_profiles(&mut self) -> BuiltinProviderProfiles {
        let mut profiles = (self.load_prompt_profiles)();
        profiles.apply_startup_responses_modes(&self.startup_responses_modes);
        if let Some(client) = &self.extension_data_client {
            hydrate_profile_credentials(client, &mut profiles);
        }
        profiles
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
        self.worker_waker = Some(waker);
    }

    #[cfg(not(test))]
    fn initialize_quota(&mut self, handle: &ClientHandle) -> ClientResult<()> {
        let mut profiles = self.load_profiles();
        let backends = profiles.resolve_initial_quota_backends(|model, profiles| {
            resolve_responses_backend(
                model,
                profiles,
                &mut self.oauth_refresh_rejections,
                self.codex_runtime.network(),
                self.extension_data_client.as_ref(),
            )
        });
        for (provider, config) in backends {
            self.ensure_quota_profile(&provider, &config, handle)?;
        }
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
                    observed_at_unix_ms: now_ms(),
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
        let backend = resolve_prompt_backend(
            model,
            profiles,
            &mut self.oauth_refresh_rejections,
            self.codex_runtime.network(),
            self.extension_data_client.as_ref(),
        )
        .unwrap_or(PromptBackend::Unavailable);
        self.reconcile_provider_profile(&model.provider, backend_profile_identity(&backend));
        if let PromptBackend::Responses(config) = &backend {
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
        if !self.input_closed {
            self.park_cooled_queued_prompts(handle)?;
        }
        let prompt_worker_context = self.prompt_worker_context();
        start_queued_prompts(
            &mut self.prompt_queue,
            &mut self.active_prompts,
            self.prompt_concurrency_limit,
            &prompt_worker_context,
            handle,
        )
    }

    fn is_finished(&self) -> bool {
        self.input_closed
            && self.active_prompts == 0
            && self.prewarm_supervisor.is_empty()
            && self.prompt_queue.is_empty()
            && self
                .retry_scheduler
                .as_ref()
                .is_none_or(RetryScheduler::is_empty)
    }

    fn begin_input_shutdown(&mut self) {
        self.input_closed = true;
        self.cancel_all_prewarms();
        self.cancellation.shutdown();
        if let Some(scheduler) = &self.retry_scheduler {
            scheduler.cancel_all();
        }
    }

    fn handle_event(&mut self, event: Event, handle: &ClientHandle) -> ClientResult<()> {
        match event {
            Event::HarnessSessionDir(session_dir) => self.record_session_debug_policy(session_dir),
            Event::AgentPromptPrewarmRequested(prewarm) => self.prewarm_backend(prewarm)?,
            Event::AgentPromptCreated(prompt) => self.handle_prompt_created(prompt, handle)?,
            Event::UiCancelPrompt(cancel) => self.handle_cancel_prompt(cancel, handle)?,
            Event::UiRetryPrompt(retry) => self.handle_retry_prompt(retry)?,
            Event::SessionShutdown(_) => self.handle_session_shutdown(handle)?,
            _ => {}
        }
        Ok(())
    }

    fn record_session_debug_policy(&mut self, session_dir: tau_proto::HarnessSessionDir) {
        self.session_debug_allowed.insert(
            session_dir.session_id,
            !matches!(session_dir.status, tau_proto::SessionDirStatus::Ephemeral),
        );
    }

    fn prewarm_backend(
        &mut self,
        prewarm: tau_proto::AgentPromptPrewarmRequested,
    ) -> ClientResult<()> {
        let mut profiles = self.load_profiles();
        let requested_provider = prewarm.model.as_ref().map(|model| model.provider.clone());
        let Some((model, config)) = resolve_prewarm_backend(
            &prewarm,
            &mut profiles,
            &mut self.oauth_refresh_rejections,
            self.codex_runtime.network(),
            self.extension_data_client.as_ref(),
        ) else {
            if let Some(provider) = requested_provider {
                self.clear_prewarm_profile(&provider);
            }
            return Ok(());
        };
        self.reconcile_provider_profile(
            &model.provider,
            backend_profile_identity(&PromptBackend::Responses(config.clone())),
        );
        self.reconcile_prewarm_profile(&model.provider, &config);
        let key = PrewarmKey {
            provider: model.provider,
            agent_id: prewarm.agent_id.clone(),
        };
        let Some((generation, abort)) = self.prewarm_supervisor.begin(key.clone()) else {
            tracing::debug!(
                target: LOG_TARGET,
                session_id = %prewarm.session_id,
                "skipping prompt prewarm: duplicate or supervisor capacity reached",
            );
            return Ok(());
        };
        let debug_provider_requests =
            debug_provider_requests_for(&prewarm.session_id, &self.session_debug_allowed);
        let executor = self.prewarm_executor.clone();
        let runtime = self.codex_runtime.clone();
        let tx = self.worker_tx.clone();
        let waker = self
            .worker_waker
            .as_ref()
            .expect("provider runtime worker waker is installed before dispatch")
            .clone();
        thread::spawn(move || {
            executor(PrewarmExecution {
                runtime,
                config,
                request: prewarm,
                debug_provider_requests,
                abort,
            });
            let _ =
                send_worker_message(&tx, &waker, WorkerMessage::PrewarmDone { key, generation });
        });
        Ok(())
    }

    fn reconcile_provider_profile(&mut self, provider: &ProviderName, identity: Option<u64>) {
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
        let prompt = materialize_prompt(&prompt);
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
        let mut frame_writer = handle_frame_writer(handle);
        finish_canceled(agent_prompt_id, prompt, &mut frame_writer)
            .map_err(|error| ClientError::handler(error.to_string()))
    }

    fn start_or_reject_prompt(
        &mut self,
        agent_prompt_id: tau_proto::AgentPromptId,
        prompt: tau_proto::AgentPromptCreated,
        handle: &ClientHandle,
    ) -> ClientResult<()> {
        self.prewarm_supervisor.cancel_key(&PrewarmKey {
            provider: prompt.model.provider.clone(),
            agent_id: prompt.agent_id.clone(),
        });
        let mut profiles = self.load_profiles();
        let backend = self.resolve_backend_with_quota(&prompt.model, &mut profiles, handle)?;
        let profile_identity = backend_profile_identity(&backend);
        let mut frame_writer = handle_frame_writer(handle);
        write_prompt_submitted(&agent_prompt_id, &prompt.originator, &mut frame_writer)
            .map_err(|error| ClientError::handler(error.to_string()))?;
        let job = PromptJob {
            agent_prompt_id,
            debug_provider_requests: debug_provider_requests_for(
                &prompt.session_id,
                &self.session_debug_allowed,
            ),
            prompt,
            backend,
            profile_identity,
            retry_state: PromptRetryState::default(),
            cancel_generation: self.cancel_generation,
            manual_cooldown_bypass: false,
            cooldown_probe: None,
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

    fn enqueue_or_start_prompt(&mut self, job: PromptJob) {
        if self.active_prompts >= self.prompt_concurrency_limit {
            self.prompt_queue.push_back(job);
            return;
        }
        let prompt_worker_context = self.prompt_worker_context();
        start_prompt_job(job, &mut self.active_prompts, &prompt_worker_context);
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
            while let Some(job) = self.prompt_queue.pop_front() {
                self.finish_canceled_prompt(&job.agent_prompt_id, &job.prompt, handle)?;
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

    /// Cancels every old-session job before the provider accepts work for a
    /// replacement session.
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
        loop {
            match self.worker_rx.try_recv() {
                Ok(WorkerMessage::Output {
                    message,
                    cancel_generation,
                    agent_prompt_id,
                    cooldown_probe,
                }) => {
                    if let Some((message, released_provider)) =
                        validate_worker_output_and_probe_for_commit(
                            message,
                            (cancel_generation, self.cancel_generation, self.input_closed),
                            &agent_prompt_id,
                            &self.cancellation,
                            cooldown_probe.as_ref(),
                            &self.shared_cooldowns,
                        )
                    {
                        if let Some(provider) = released_provider {
                            self.release_shared_cooldown(&provider);
                        }
                        handle.send(message)?;
                    }
                }
                Ok(WorkerMessage::PromptDone) => {
                    self.active_prompts = self.active_prompts.saturating_sub(1);
                }
                Ok(WorkerMessage::PrewarmDone { key, generation }) => {
                    self.prewarm_supervisor.complete(&key, generation);
                }
                Ok(WorkerMessage::Retry {
                    mut job,
                    decision,
                    live_detail,
                }) => {
                    if self.input_closed
                        || job.cancel_generation != self.cancel_generation
                        || self.cancellation.is_canceled(&job.agent_prompt_id)
                    {
                        self.cancellation.take_canceled(&job.agent_prompt_id);
                        self.finish_canceled_prompt(&job.agent_prompt_id, &job.prompt, handle)?;
                        continue;
                    }
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
                            provider,
                            common_due,
                            decision.class,
                        );
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
                    if let Some(scheduler) = &self.retry_scheduler {
                        scheduler
                            .delayed_count
                            .fetch_sub(1, AtomicOrdering::Relaxed);
                    }
                    if self.input_closed
                        || job.cancel_generation != self.cancel_generation
                        || self.cancellation.take_canceled(&job.agent_prompt_id)
                    {
                        self.finish_canceled_prompt(&job.agent_prompt_id, &job.prompt, handle)?;
                        continue;
                    }
                    let mut profiles = self.load_profiles();
                    job.backend =
                        self.resolve_backend_with_quota(&job.prompt.model, &mut profiles, handle)?;
                    job.profile_identity = backend_profile_identity(&job.backend);
                    self.prompt_queue.push_back(job);
                }
                Ok(WorkerMessage::ManualRetry {
                    mut job,
                    request_id,
                    agent_prompt_id,
                }) => {
                    let status = if let Some(mut owned_job) = job.take() {
                        if let Some(scheduler) = &self.retry_scheduler {
                            scheduler
                                .delayed_count
                                .fetch_sub(1, AtomicOrdering::Relaxed);
                        }
                        if self.input_closed
                            || owned_job.cancel_generation != self.cancel_generation
                            || self.cancellation.take_canceled(&owned_job.agent_prompt_id)
                        {
                            self.finish_canceled_prompt(
                                &owned_job.agent_prompt_id,
                                &owned_job.prompt,
                                handle,
                            )?;
                            tau_proto::RetryPromptStatus::NotParked
                        } else {
                            let mut profiles = self.load_profiles();
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
                            self.prompt_queue.push_back(owned_job);
                            tau_proto::RetryPromptStatus::Accepted
                        }
                    } else {
                        tau_proto::RetryPromptStatus::NotParked
                    };
                    let mut frame_writer = handle_frame_writer(handle);
                    frame_writer.write_message(&HarnessInputMessage::emit_transient(
                        Event::ProviderRetryPromptResultReported(
                            tau_proto::ProviderRetryPromptResult {
                                request_id,
                                agent_prompt_id,
                                status,
                            },
                        ),
                    ))?;
                    frame_writer.flush()?;
                }
                Ok(WorkerMessage::DelayedCanceled { job, delayed_count }) => {
                    if let Some(scheduler) = &self.retry_scheduler {
                        scheduler
                            .delayed_count
                            .fetch_sub(delayed_count, AtomicOrdering::Relaxed);
                    }
                    self.cancellation.take_canceled(&job.agent_prompt_id);
                    self.finish_canceled_prompt(&job.agent_prompt_id, &job.prompt, handle)?;
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
                        let mut profiles = self.load_profiles();
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
        let mut index = 0;
        while index < self.prompt_queue.len() {
            if self.prompt_queue[index].manual_cooldown_bypass {
                index += 1;
                continue;
            }
            let Some(cooldown) = self
                .prompt_queue
                .get(index)
                .and_then(|job| self.shared_cooldowns.get(&job.prompt.model.provider))
                .copied()
                .filter(|cooldown| cooldown.not_before > self.retry_clock.now())
            else {
                index += 1;
                continue;
            };
            let Some(job) = self.prompt_queue.remove(index) else {
                continue;
            };
            let now = self.retry_clock.now();
            let due = cooldown_due_for_job(cooldown.not_before, &job);
            emit_retry_status(&job, cooldown.class, due, now, None, handle)?;
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
        }
        Ok(())
    }

    fn prompt_worker_context(&self) -> PromptWorkerContext {
        PromptWorkerContext {
            worker_tx: self.worker_tx.clone(),
            worker_waker: self
                .worker_waker
                .as_ref()
                .expect("provider runtime worker waker is installed before dispatch")
                .clone(),
            prompt_executor: self.prompt_executor.clone(),
            cancellation: self.cancellation.clone(),
            codex_runtime: self.codex_runtime.clone(),
        }
    }
}

/// Reconciles one material inference identity and removes only that provider's
/// obsolete shared cooldown when the identity changes or disappears.
fn reconcile_inference_identity(
    identities: &mut BTreeMap<ProviderName, Option<u64>>,
    cooldowns: &mut BTreeMap<ProviderName, SharedCooldown>,
    provider: &ProviderName,
    identity: Option<u64>,
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
    message: Box<HarnessInputMessage>,
    dispatch_generation: u64,
    current_generation: u64,
    input_closed: bool,
    agent_prompt_id: &tau_proto::AgentPromptId,
    cancellation: &CancellationState,
) -> Option<HarnessInputMessage> {
    let targeted = cancellation.is_canceled(agent_prompt_id);
    if dispatch_generation == current_generation && !input_closed && !targeted {
        return Some(*message);
    }
    if is_intentional_partial_clear_for(message.as_ref(), agent_prompt_id) {
        return Some(*message);
    }
    let HarnessInputMessage::Emit(emit) = message.as_ref() else {
        return None;
    };
    let Event::ProviderResponseFinishedReported(finished) = emit.event.as_ref() else {
        return None;
    };
    cancellation.take_canceled(agent_prompt_id);
    Some(HarnessInputMessage::emit_transient(
        Event::ProviderResponseFinishedReported(simple_finished(
            finished.agent_prompt_id.clone(),
            finished.agent_id.clone(),
            finished.originator.clone(),
            "(cancelled by harness)",
        )),
    ))
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
    message: Box<HarnessInputMessage>,
    commit_state: (u64, u64, bool),
    agent_prompt_id: &tau_proto::AgentPromptId,
    cancellation: &CancellationState,
    probe: Option<&CooldownProbe>,
    cooldowns: &BTreeMap<ProviderName, SharedCooldown>,
) -> Option<(HarnessInputMessage, Option<ProviderName>)> {
    let (dispatch_generation, current_generation, input_closed) = commit_state;
    let message = validate_worker_output_for_commit(
        message,
        dispatch_generation,
        current_generation,
        input_closed,
        agent_prompt_id,
        cancellation,
    )?;
    let released_provider = probe
        .filter(|probe| successful_probe_matches(&message, agent_prompt_id, probe, cooldowns))
        .map(|probe| probe.provider.clone());
    Some((message, released_provider))
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
type PrewarmExecutor = Arc<dyn Fn(PrewarmExecution) + Send + Sync + 'static>;

struct PromptJob {
    agent_prompt_id: tau_proto::AgentPromptId,
    debug_provider_requests: bool,
    prompt: tau_proto::AgentPromptCreated,
    backend: PromptBackend,
    /// Inference profile identity used by the next finite attempt.
    profile_identity: Option<u64>,
    retry_state: PromptRetryState,
    /// Runtime global-cancel generation at logical prompt creation.
    cancel_generation: u64,
    /// Lets one manually released job pass a still-active shared cooldown once.
    manual_cooldown_bypass: bool,
    /// Shared cooldown generation this job was manually admitted to probe.
    cooldown_probe: Option<CooldownProbe>,
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
    /// Stable FIFO tie-breaker for equal deadlines.
    sequence: u64,
}

impl RetryScheduleQueue {
    /// Adds one logical prompt at its current eligible deadline.
    fn schedule(
        &mut self,
        independent_due: Instant,
        cooldown: Option<CooldownConstraint>,
        job: PromptJob,
    ) -> Result<(), Box<PromptJob>> {
        if self
            .prompts
            .iter()
            .any(|scheduled| scheduled.job.agent_prompt_id == job.agent_prompt_id)
        {
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
        self.prompts.pop().map(|scheduled| scheduled.job)
    }

    /// Returns the earliest deadline, if any.
    fn next_due(&self) -> Option<Instant> {
        self.prompts.peek().map(|scheduled| scheduled.due)
    }

    /// Removes all delayed instances of one logical prompt.
    fn cancel(&mut self, prompt_id: &tau_proto::AgentPromptId) -> Vec<PromptJob> {
        self.remove_matching(|scheduled| scheduled.job.agent_prompt_id == *prompt_id)
    }

    /// Removes every delayed logical prompt.
    fn cancel_all(&mut self) -> Vec<PromptJob> {
        self.prompts
            .drain()
            .map(|scheduled| scheduled.job)
            .collect()
    }

    /// Monotonically moves same-provider prompts beyond a shared cooldown.
    fn extend_cooldown(&mut self, provider: &ProviderName, due: Instant, generation: u64) {
        let mut updated = BinaryHeap::new();
        while let Some(mut scheduled) = self.prompts.pop() {
            if scheduled.job.prompt.model.provider == *provider {
                if scheduled.cooldown_generation.is_none() {
                    scheduled.independent_due = scheduled.due;
                }
                scheduled.cooldown_generation = Some(generation);
                scheduled.due = scheduled
                    .independent_due
                    .max(cooldown_due_for_job(due, &scheduled.job));
            }
            updated.push(scheduled);
        }
        self.prompts = updated;
    }

    /// Advances only matching provider prompts after an authoritative probe.
    fn release_cooldown(&mut self, provider: &ProviderName, generation: u64, now: Instant) {
        let mut updated = BinaryHeap::new();
        while let Some(mut scheduled) = self.prompts.pop() {
            if scheduled.job.prompt.model.provider == *provider
                && scheduled.cooldown_generation == Some(generation)
            {
                scheduled.cooldown_generation = None;
                scheduled.due = scheduled
                    .independent_due
                    .max(cooldown_due_for_job(now, &scheduled.job));
            }
            updated.push(scheduled);
        }
        self.prompts = updated;
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

    /// Removes entries matching a scheduler command while retaining heap order.
    fn remove_matching(
        &mut self,
        mut predicate: impl FnMut(&ScheduledPrompt) -> bool,
    ) -> Vec<PromptJob> {
        let mut removed = Vec::new();
        let mut retained = BinaryHeap::new();
        while let Some(scheduled) = self.prompts.pop() {
            if predicate(&scheduled) {
                removed.push(scheduled.job);
            } else {
                retained.push(scheduled);
            }
        }
        self.prompts = retained;
        removed
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

/// Monotonic retry clock, injectable so long quota windows need no wall wait.
trait RetryClock: Send + Sync {
    /// Returns the current monotonic scheduler instant.
    fn now(&self) -> Instant;

    /// Receives the actor command channel for virtual-time wakeups.
    fn attach_scheduler(&self, _commands: std::sync::Weak<SyncSender<SchedulerCommand>>) {}
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

/// RAII owner of the delayed-work scheduler actor.
///
/// Dropping the last strong command sender disconnects the actor, then joins
/// its thread. `delayed_count` tracks jobs owned by either the bounded command
/// channel or synchronous scheduler state until the provider consumes an
/// action.
struct RetryScheduler {
    /// Last strong sender whose drop terminates the scheduler actor.
    commands: Arc<SyncSender<SchedulerCommand>>,
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
        let (commands, receiver) = mpsc::sync_channel(1_024);
        let commands = Arc::new(commands);
        clock.attach_scheduler(Arc::downgrade(&commands));
        let delayed_count = Arc::new(AtomicUsize::new(0));
        let actor = thread::spawn(move || {
            run_retry_scheduler(receiver, worker_tx, worker_waker, clock);
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
            .commands
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
        let _ = self.commands.send(SchedulerCommand::Cancel(prompt_id));
    }

    /// Requests cancellation of every delayed retry job owned by the scheduler.
    fn cancel_all(&self) {
        let _ = self.commands.send(SchedulerCommand::CancelAll);
    }

    fn retry_now(
        &self,
        request_id: tau_proto::RetryPromptRequestId,
        agent_prompt_id: tau_proto::AgentPromptId,
    ) {
        let _ = self.commands.send(SchedulerCommand::RetryNow {
            request_id,
            agent_prompt_id,
        });
    }

    fn extend_cooldown(&self, provider: ProviderName, due: Instant, generation: u64) {
        let _ = self.commands.send(SchedulerCommand::ExtendCooldown {
            provider,
            due,
            generation,
        });
    }

    fn release_cooldown(&self, provider: ProviderName, generation: u64, now: Instant) {
        let _ = self.commands.send(SchedulerCommand::ReleaseCooldown {
            provider,
            generation,
            now,
        });
    }

    fn is_empty(&self) -> bool {
        self.delayed_count.load(AtomicOrdering::Relaxed) == 0
    }
}

fn run_retry_scheduler(
    commands: Receiver<SchedulerCommand>,
    worker_tx: Sender<WorkerMessage>,
    worker_waker: ManualRuntimeWaker,
    clock: Arc<dyn RetryClock>,
) {
    let mut state = RetrySchedulerState::default();
    loop {
        if !send_scheduler_actions(state.advance(clock.now()), &worker_tx, &worker_waker) {
            return;
        }
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
            Ok(command) => {
                let acknowledged = match &command {
                    SchedulerCommand::Wake { acknowledged } => acknowledged.clone(),
                    _ => None,
                };
                if !send_scheduler_actions(state.step(command), &worker_tx, &worker_waker) {
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
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

impl Drop for RetryScheduler {
    fn drop(&mut self) {
        // Disconnect the actor before joining; virtual clocks retain only Weak.
        let (replacement, _) = mpsc::sync_channel(0);
        self.commands = Arc::new(replacement);
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
    Unavailable,
    Responses(ResolvedConfig),
    ChatCompletions {
        provider: ChatCompletionsProvider,
        model: ChatCompletionsModel,
    },
    /// Generic public Responses API request using profile-selected SSE or
    /// WebSocket.
    PublicResponses {
        provider: ResponsesProvider,
        model: ResponsesModel,
    },
}

struct PromptExecution {
    job: PromptJob,
    /// Cooldown generation this exact finite attempt may invalidate.
    cooldown_probe: Option<CooldownProbe>,
    output_tx: Sender<WorkerMessage>,
    output_waker: ManualRuntimeWaker,
    cancellation: Arc<CancellationState>,
    codex_runtime: Arc<CodexRuntime>,
}

struct PromptWorkerContext {
    worker_tx: Sender<WorkerMessage>,
    worker_waker: ManualRuntimeWaker,
    prompt_executor: PromptExecutor,
    cancellation: Arc<CancellationState>,
    codex_runtime: Arc<CodexRuntime>,
}

impl PromptExecution {
    fn frame_writer(&self) -> PeerOutputWriter<BufWriter<HarnessInputMessageWrite>> {
        PeerOutputWriter::new(BufWriter::new(HarnessInputMessageWrite::worker(
            self.output_tx.clone(),
            self.output_waker.clone(),
            self.job.cancel_generation,
            self.job.agent_prompt_id.clone(),
            self.cooldown_probe.clone(),
        )))
    }
}

enum WorkerMessage {
    /// One typed provider frame produced by a prompt worker and awaiting main
    /// loop serialization.
    Output {
        message: Box<HarnessInputMessage>,
        cancel_generation: u64,
        agent_prompt_id: tau_proto::AgentPromptId,
        /// Cooldown generation carried by the manually admitted attempt.
        cooldown_probe: Option<CooldownProbe>,
    },
    /// Marker that one prompt worker finished and freed a concurrency slot.
    PromptDone,
    /// Exact supervised prewarm worker completion.
    PrewarmDone {
        /// Cache owner whose work finished.
        key: PrewarmKey,
        /// Generation captured when the main loop admitted the work.
        generation: u64,
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
        observed_at_unix_ms: u64,
    },
    /// Result of one coalesced full account-usage fetch.
    QuotaFetchFinished {
        /// Provider profile fetched.
        provider: ProviderName,
        /// Epoch captured before starting I/O.
        profile_epoch: tau_proto::ProviderQuotaEpoch,
        /// State sequence captured before starting I/O.
        fetch_start_sequence: u64,
        /// Wall-clock completion time sampled by the acquisition worker.
        observed_at_unix_ms: u64,
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

/// Destination for decoded provider output frames.
enum HarnessInputMessageTarget {
    /// Synchronous output path used by main-loop helper code.
    Handle(ClientHandle),
    /// Worker-to-main-loop path used by prompt workers.
    Worker {
        /// Channel that carries decoded worker messages to the main loop.
        tx: Sender<WorkerMessage>,
        /// Wake handle signaled after the worker message is queued.
        waker: ManualRuntimeWaker,
        /// Global-cancel generation captured synchronously at dispatch.
        cancel_generation: u64,
        /// Prompt identity used for targeted-cancel commit validation.
        agent_prompt_id: tau_proto::AgentPromptId,
        /// Exact cooldown probe authority attached to this finite attempt.
        cooldown_probe: Option<CooldownProbe>,
    },
}

/// `Write` adapter that preserves existing `PeerOutputWriter` call sites while
/// converting completed frame bytes back into typed `HarnessInputMessage`s.
///
/// Bytes are buffered until `flush`, decoded FIFO, and forwarded either to the
/// main-loop client handle or the worker output channel. Partial or invalid
/// frames become `InvalidData` so the caller observes a normal output failure.
struct HarnessInputMessageWrite {
    /// Destination that receives decoded frames on flush.
    target: HarnessInputMessageTarget,
    /// Encoded bytes accumulated since the previous flush.
    buf: Vec<u8>,
}

impl HarnessInputMessageWrite {
    fn handle(handle: ClientHandle) -> Self {
        Self {
            target: HarnessInputMessageTarget::Handle(handle),
            buf: Vec::new(),
        }
    }

    fn worker(
        tx: Sender<WorkerMessage>,
        waker: ManualRuntimeWaker,
        cancel_generation: u64,
        agent_prompt_id: tau_proto::AgentPromptId,
        cooldown_probe: Option<CooldownProbe>,
    ) -> Self {
        Self {
            target: HarnessInputMessageTarget::Worker {
                tx,
                waker,
                cancel_generation,
                agent_prompt_id,
                cooldown_probe,
            },
            buf: Vec::new(),
        }
    }

    fn send_decoded(&self, message: HarnessInputMessage) -> std::io::Result<()> {
        match &self.target {
            HarnessInputMessageTarget::Handle(handle) => handle.send(message).map_err(|error| {
                path_std_io::Error::new(path_std_io::ErrorKind::BrokenPipe, error)
            }),
            HarnessInputMessageTarget::Worker {
                tx,
                waker,
                cancel_generation,
                agent_prompt_id,
                cooldown_probe,
            } => send_worker_message(
                tx,
                waker,
                WorkerMessage::Output {
                    message: Box::new(message),
                    cancel_generation: *cancel_generation,
                    agent_prompt_id: agent_prompt_id.clone(),
                    cooldown_probe: cooldown_probe.clone(),
                },
            )
            .map_err(|_| {
                path_std_io::Error::new(path_std_io::ErrorKind::BrokenPipe, "writer closed")
            }),
        }
    }
}

impl Write for HarnessInputMessageWrite {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let bytes = std::mem::take(&mut self.buf);
        let mut reader = HarnessInputReader::new(Cursor::new(bytes));
        while let Some(message) = reader
            .read_message()
            .map_err(|error| path_std_io::Error::new(path_std_io::ErrorKind::InvalidData, error))?
        {
            self.send_decoded(message)?;
        }
        Ok(())
    }
}

fn handle_frame_writer(
    handle: &ClientHandle,
) -> PeerOutputWriter<BufWriter<HarnessInputMessageWrite>> {
    PeerOutputWriter::new(BufWriter::new(HarnessInputMessageWrite::handle(
        handle.clone(),
    )))
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
                    observed_at_unix_ms: now_ms(),
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
                logical_attempt: tau_provider_codex::LogicalAttempt::new(
                    execution.job.retry_state.attempts.saturating_add(1),
                ),
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
                    },
                );
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    target: LOG_TARGET,
                    agent_prompt_id = %agent_prompt_id,
                    "prompt worker failed to emit provider response: {error}"
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
        );
    })
}

fn start_prompt_job(mut job: PromptJob, active_prompts: &mut usize, context: &PromptWorkerContext) {
    *active_prompts += 1;
    let cooldown_probe = job.cooldown_probe.take();
    let execution = PromptExecution {
        job,
        cooldown_probe,
        output_tx: context.worker_tx.clone(),
        output_waker: context.worker_waker.clone(),
        cancellation: context.cancellation.clone(),
        codex_runtime: context.codex_runtime.clone(),
    };
    let executor = context.prompt_executor.clone();
    let done_tx = context.worker_tx.clone();
    let done_waker = context.worker_waker.clone();
    thread::spawn(move || {
        executor(execution);
        let _ = send_worker_message(&done_tx, &done_waker, WorkerMessage::PromptDone);
    });
}

fn send_worker_message(
    tx: &Sender<WorkerMessage>,
    waker: &ManualRuntimeWaker,
    message: WorkerMessage,
) -> Result<(), ()> {
    // All worker-to-loop messages must be enqueued through this helper so the
    // main loop can rely on enqueue-before-wake ordering before blocking in
    // `ManualExtensionRuntime::wait_for_wake`.
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
            let mut frame_writer = handle_frame_writer(handle);
            finish_canceled(&job.agent_prompt_id, &job.prompt, &mut frame_writer)
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
    let Some(job) = prompt_queue.remove(index) else {
        return Ok(false);
    };
    let mut frame_writer = handle_frame_writer(handle);
    finish_canceled(&job.agent_prompt_id, &job.prompt, &mut frame_writer)
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
    let delay = due.checked_duration_since(now).unwrap_or(Duration::ZERO);
    let delay_text = tau_proto::format_approximate_duration_secs(delay.as_secs());
    let reason = live_detail
        .map(|detail| format!("{}: {detail}", class.public_reason()))
        .unwrap_or_else(|| class.public_reason().to_owned());
    let text = format!(
        "{}; next attempt in about {} (attempt {}). Tau will keep trying; cancel the prompt to stop.",
        reason, delay_text, job.retry_state.attempts,
    );
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
                    next_retry_delay_secs: saturating_retry_delay(delay),
                }),
            }),
            response_stats: None,
            originator: job.prompt.originator.clone(),
        }),
    ))
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

fn materialize_prompt(prompt: &tau_proto::AgentPromptCreated) -> tau_proto::AgentPromptCreated {
    let mut materialized = prompt.clone();
    materialized.tools_ref = None;
    materialized
}

fn trace_provider_prompt(
    prompt: &tau_proto::AgentPromptCreated,
    agent_prompt_id: &tau_proto::AgentPromptId,
) {
    if !tracing::enabled!(target: LOG_TARGET, tracing::Level::TRACE) {
        return;
    }
    let mut redacted = prompt.clone();
    redacted.context.clear_provider_image_bytes();
    trace_prompt_like("provider prompt", &redacted, agent_prompt_id);
}

fn trace_prompt_like<T: serde::Serialize>(
    label: &str,
    value: &T,
    agent_prompt_id: &tau_proto::AgentPromptId,
) {
    if !tracing::enabled!(target: LOG_TARGET, tracing::Level::TRACE) {
        return;
    }
    match serde_json::to_string_pretty(value) {
        Ok(json) => tracing::trace!(
            target: LOG_TARGET,
            agent_prompt_id = %agent_prompt_id,
            "{label}:\n{json}"
        ),
        Err(error) => tracing::trace!(
            target: LOG_TARGET,
            agent_prompt_id = %agent_prompt_id,
            "{label} (failed to serialize for log: {error})"
        ),
    }
}

fn write_prompt_submitted<W: Write>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    originator: &tau_proto::PromptOriginator,
    writer: &mut PeerOutputWriter<W>,
) -> Result<(), Box<dyn Error>> {
    writer.write_message(&HarnessInputMessage::emit_transient(
        Event::ProviderPromptSubmittedReported(ProviderPromptSubmitted {
            agent_prompt_id: agent_prompt_id.clone(),
            originator: originator.clone(),
        }),
    ))?;
    writer.flush()?;
    Ok(())
}

fn finish_canceled<W: Write>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    writer: &mut PeerOutputWriter<W>,
) -> Result<(), Box<dyn Error>> {
    tracing::info!(
        target: LOG_TARGET,
        agent_prompt_id = %agent_prompt_id,
        "skipping provider request — already canceled by harness",
    );
    writer.write_message(&HarnessInputMessage::emit_transient(
        Event::ProviderResponseFinishedReported(simple_finished(
            agent_prompt_id.clone(),
            prompt.agent_id.clone(),
            prompt.originator.clone(),
            "(cancelled by harness)",
        )),
    ))?;
    writer.flush()?;
    Ok(())
}

fn simple_finished(
    agent_prompt_id: tau_proto::AgentPromptId,
    agent_id: tau_proto::AgentId,
    originator: tau_proto::PromptOriginator,
    text: impl Into<String>,
) -> ProviderResponseFinished {
    ProviderResponseFinished {
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
        originator,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
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
    let Some(profile) = profiles.providers.get_mut(&model.provider) else {
        refresh_rejections.clear(&model.provider);
        return None;
    };
    match profile {
        BuiltinProviderProfile::Chatgpt(profile) => {
            let mode = profile.responses_mode();
            resolve_chatgpt_backend(
                model,
                &model.provider,
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
            let configured_model = provider
                .models
                .iter()
                .find(|configured| configured.id == model.model)?
                .clone();
            Some(PromptBackend::ChatCompletions {
                provider: provider.clone(),
                model: configured_model,
            })
        }
        BuiltinProviderProfile::OpenRouter(profile) => {
            refresh_rejections.clear(&model.provider);
            let provider = profile.to_chat_completions();
            let configured_model = provider
                .models
                .iter()
                .find(|configured| configured.id == model.model)?
                .clone();
            Some(PromptBackend::ChatCompletions {
                provider,
                model: configured_model,
            })
        }
        BuiltinProviderProfile::Responses(provider) => {
            refresh_rejections.clear(&model.provider);
            let configured_model = provider
                .models
                .iter()
                .find(|configured| configured.id == model.model)?
                .clone();
            Some(PromptBackend::PublicResponses {
                provider: provider.clone(),
                model: configured_model,
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
    let Some(profile) = profiles.providers.get_mut(&model.provider) else {
        refresh_rejections.clear(&model.provider);
        return None;
    };
    match profile {
        BuiltinProviderProfile::Chatgpt(profile) => {
            let mode = profile.responses_mode();
            resolve_chatgpt_backend(
                model,
                &model.provider,
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
    provider_name: &ProviderName,
    auth_store: &mut OpenAiAuth,
    mode: CodexMode,
    refresh_rejections: &mut OAuthRefreshRejectionCache,
    network: &tau_provider::OutboundNetworkPolicy,
    extension_data_client: Option<&ExtensionDataClient>,
) -> Option<ResolvedConfig> {
    resolve_chatgpt_backend_with_refresh(
        model,
        provider_name,
        auth_store,
        mode,
        refresh_rejections,
        |provider, mode, rejections| {
            refresh_chatgpt_credentials_rpc(
                provider,
                mode,
                rejections,
                network,
                extension_data_client,
            )
        },
    )
}

fn resolve_chatgpt_backend_with_refresh(
    model: &ModelId,
    provider_name: &ProviderName,
    auth_store: &mut OpenAiAuth,
    mode: CodexMode,
    refresh_rejections: &mut OAuthRefreshRejectionCache,
    refresh: impl FnOnce(
        &ProviderName,
        CodexMode,
        &mut OAuthRefreshRejectionCache,
    ) -> Result<OpenAiAuth, RefreshCredentialsError>,
) -> Option<ResolvedConfig> {
    let refresh_due =
        oauth_token_should_refresh(&auth_store.access_token, auth_store.expires_at_ms)
            && !auth_store.refresh_token.trim().is_empty();
    if refresh_due || refresh_rejections.contains(provider_name) {
        match refresh(provider_name, mode, refresh_rejections) {
            Ok(refreshed) => {
                *auth_store = refreshed;
            }
            Err(error @ RefreshCredentialsError::Storage(_)) => {
                tracing::warn!(
                    target: LOG_TARGET,
                    provider = %provider_name,
                    "failed to refresh ChatGPT credentials: {error}"
                );
            }
            Err(
                RefreshCredentialsError::OAuth { credentials, error }
                | RefreshCredentialsError::Suppressed { credentials, error },
            ) => {
                *auth_store = *credentials;
                tracing::warn!(
                    target: LOG_TARGET,
                    provider = %provider_name,
                    "failed to refresh ChatGPT credentials: {error}"
                );
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

fn refresh_chatgpt_credentials_rpc(
    provider_name: &ProviderName,
    mode: CodexMode,
    refresh_rejections: &mut OAuthRefreshRejectionCache,
    network: &tau_provider::OutboundNetworkPolicy,
    extension_data_client: Option<&ExtensionDataClient>,
) -> Result<OpenAiAuth, RefreshCredentialsError> {
    let client = extension_data_client.ok_or_else(|| {
        RefreshCredentialsError::Storage(path_std_io::Error::new(
            path_std_io::ErrorKind::PermissionDenied,
            "Secret RPC is unavailable",
        ))
    })?;
    let path = format!("providers/{provider_name}/oauth.json");
    let value = client
        .request(
            tau_proto::ExtensionDataScope::Secret,
            tau_proto::ExtensionDataRequestOp::ReadFile {
                path: tau_proto::ExtensionDataPath::new(path.clone()),
            },
        )
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
    if !oauth_token_should_refresh(&current.access_token, current.expires_at_ms)
        || current.refresh_token.trim().is_empty()
    {
        refresh_rejections.clear(provider_name);
        return Ok(current);
    }
    let tokens =
        match tau_provider_codex::oauth::openai_codex_refresh(&current.refresh_token, network) {
            Ok(tokens) => tokens,
            Err(error) => {
                refresh_rejections.record_if_permanent(provider_name, &current, mode, &error);
                return Err(RefreshCredentialsError::OAuth {
                    credentials: Box::new(current),
                    error,
                });
            }
        };
    let refreshed = OpenAiAuth {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_at_ms: tokens.expires_at_ms,
        account_id: tokens.account_id,
    };
    let replacement = serde_json::to_vec(&credential_record::ChatGptOAuthCredential::from(
        refreshed.clone(),
    ))
    .map_err(|_| {
        RefreshCredentialsError::Storage(path_std_io::Error::other(
            "could not encode refreshed OAuth credential",
        ))
    })?;
    let expected_generation = blake3::hash(&contents).to_hex().to_string();
    match client.request(
        tau_proto::ExtensionDataScope::Secret,
        tau_proto::ExtensionDataRequestOp::CompareAndSwapFile {
            path: tau_proto::ExtensionDataPath::new(path.clone()),
            expected_generation,
            contents: replacement,
        },
    ) {
        Ok(tau_proto::ExtensionDataValue::CompareAndSwapFile) => {
            refresh_rejections.clear(provider_name);
            Ok(refreshed)
        }
        Ok(_) => Err(RefreshCredentialsError::Storage(path_std_io::Error::other(
            "unexpected OAuth credential CAS result",
        ))),
        Err(tau_client::ExtensionDataRpcError::Harness {
            kind: tau_proto::ExtensionDataErrorKind::GenerationMismatch,
            ..
        }) => {
            // A concurrent rotating refresh may have won CAS. Reload and use its
            // complete generation rather than retrying the now-consumed token.
            let value = client
                .request(
                    tau_proto::ExtensionDataScope::Secret,
                    tau_proto::ExtensionDataRequestOp::ReadFile {
                        path: tau_proto::ExtensionDataPath::new(path),
                    },
                )
                .map_err(|_| {
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
            refresh_rejections.clear(provider_name);
            Ok(authoritative)
        }
        Err(_) => Err(RefreshCredentialsError::Storage(path_std_io::Error::other(
            "OAuth credential CAS failed",
        ))),
    }
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
fn emit_retry_banner<W: Write>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    agent_id: &tau_proto::AgentId,
    originator: &tau_proto::PromptOriginator,
    writer: &mut PeerOutputWriter<W>,
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
    let _ = writer.write_message(&HarnessInputMessage::emit_transient(
        Event::ProviderResponseUpdatedReported(ProviderResponseUpdated {
            agent_prompt_id: agent_prompt_id.clone(),
            agent_id: agent_id.clone(),
            deltas: Vec::new(),
            compaction: None,
            status: Some(ProviderResponseStatusUpdate {
                text: banner,
                clear_response: true,
                retry: None,
            }),
            response_stats: None,
            originator: originator.clone(),
        }),
    ));
    let _ = writer.flush();
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
) {
    let session_id_str = prewarm.session_id.as_str();
    let request = CodexPrompt {
        system_prompt: &prewarm.system_prompt,
        context: &prewarm.context,
        tools: &prewarm.tools,
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
            tracing::debug!(target: LOG_TARGET, session_id = session_id_str, "completed prompt prewarm")
        }
        PrewarmOutcome::SkippedBusy => tracing::debug!(
            target: LOG_TARGET,
            session_id = session_id_str,
            "skipped prompt prewarm: websocket key is busy",
        ),
        PrewarmOutcome::Retry(decision) => tracing::debug!(
            target: LOG_TARGET,
            session_id = session_id_str,
            retry_class = ?decision.class,
            "prompt prewarm ended with retryable provider failure",
        ),
        PrewarmOutcome::Canceled => tracing::debug!(
            target: LOG_TARGET,
            session_id = session_id_str,
            "prompt prewarm canceled",
        ),
        PrewarmOutcome::Terminal(error) => tracing::debug!(
            target: LOG_TARGET,
            session_id = session_id_str,
            "prompt prewarm failed: {error}",
        ),
    }
}

fn handle_prompt_backend<R, W: Write>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    backend: &PromptBackend,
    prompt: &tau_proto::AgentPromptCreated,
    writer: &mut PeerOutputWriter<W>,
    retry_ctx: &mut R,
    context: ChatGptPromptExecutionContext<'_>,
    on_quota: &mut impl FnMut(&tau_provider_codex::RollingQuotaObservation),
) -> Result<Option<PromptAttemptRetry>, Box<dyn Error>>
where
    R: TurnAbort,
{
    match backend {
        PromptBackend::Unavailable => Ok(Some(PromptAttemptRetry {
            decision: RetryDecision::new(RetryClass::Auth),
            live_detail: None,
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
        PromptBackend::ChatCompletions { provider, model } => handle_chat_completions_backend(
            agent_prompt_id,
            prompt,
            provider,
            model,
            writer,
            retry_ctx,
            context,
        ),
        PromptBackend::PublicResponses { provider, model } => handle_public_responses_backend(
            agent_prompt_id,
            prompt,
            provider,
            model,
            writer,
            retry_ctx,
            context,
        ),
    }
}

/// Runs one Chat Completions attempt and reports its terminal or retry outcome.
fn handle_chat_completions_backend<R, W: Write>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    provider: &ChatCompletionsProvider,
    model: &ChatCompletionsModel,
    writer: &mut PeerOutputWriter<W>,
    retry_ctx: &mut R,
    context: ChatGptPromptExecutionContext<'_>,
) -> Result<Option<PromptAttemptRetry>, Box<dyn Error>>
where
    R: TurnAbort,
{
    if TurnAbort::is_aborted(retry_ctx) {
        finish_canceled(agent_prompt_id, prompt, writer)?;
        return Ok(None);
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
    );
    match outcome {
        ChatCompletionsAttemptOutcome::Finished(finished) => finish_backend_attempt(
            agent_prompt_id,
            prompt,
            writer,
            retry_ctx,
            *finished,
            true,
            "request canceled; discarding tentative provider output",
        ),
        ChatCompletionsAttemptOutcome::Terminal { finished, progress } => finish_terminal_attempt(
            agent_prompt_id,
            prompt,
            writer,
            *finished,
            progress == tau_provider_chat_completions::SemanticProgress::Parsed,
        ),
        ChatCompletionsAttemptOutcome::Retry { decision, progress } => finish_retry_attempt(
            agent_prompt_id,
            prompt,
            writer,
            decision,
            progress == tau_provider_chat_completions::SemanticProgress::Parsed,
        ),
        ChatCompletionsAttemptOutcome::Canceled { progress } => finish_canceled_attempt(
            agent_prompt_id,
            prompt,
            writer,
            progress == tau_provider_chat_completions::SemanticProgress::Parsed,
        ),
    }
}

/// Runs one public Responses attempt and reports its terminal or retry outcome.
fn handle_public_responses_backend<R, W: Write>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    provider: &ResponsesProvider,
    model: &ResponsesModel,
    writer: &mut PeerOutputWriter<W>,
    retry_ctx: &mut R,
    context: ChatGptPromptExecutionContext<'_>,
) -> Result<Option<PromptAttemptRetry>, Box<dyn Error>>
where
    R: TurnAbort,
{
    if TurnAbort::is_aborted(retry_ctx) {
        finish_canceled(agent_prompt_id, prompt, writer)?;
        return Ok(None);
    }
    match run_responses_prompt_attempt(
        agent_prompt_id,
        prompt,
        provider,
        model,
        writer,
        &mut || TurnAbort::is_aborted(retry_ctx),
        context.runtime.network(),
    ) {
        ResponsesAttemptOutcome::Finished(finished) => finish_backend_attempt(
            agent_prompt_id,
            prompt,
            writer,
            retry_ctx,
            *finished,
            true,
            "request canceled; discarding tentative provider output",
        ),
        ResponsesAttemptOutcome::Terminal { finished, progress } => finish_terminal_attempt(
            agent_prompt_id,
            prompt,
            writer,
            *finished,
            progress.has_timed_semantic_output,
        ),
        ResponsesAttemptOutcome::Retry { decision, progress } => finish_retry_attempt(
            agent_prompt_id,
            prompt,
            writer,
            decision,
            progress.has_timed_semantic_output,
        ),
        ResponsesAttemptOutcome::Canceled { progress } => finish_canceled_attempt(
            agent_prompt_id,
            prompt,
            writer,
            progress.has_timed_semantic_output,
        ),
    }
}

/// Emits a successful final response unless a concurrent cancellation won.
fn finish_backend_attempt<R, W: Write>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    writer: &mut PeerOutputWriter<W>,
    retry_ctx: &mut R,
    finished: ProviderResponseFinished,
    has_partial_output: bool,
    cancellation_detail: &str,
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
            cancellation_detail,
        )?;
        finish_canceled(agent_prompt_id, prompt, writer)?;
        return Ok(None);
    }
    emit_finished_backend_response(writer, finished)?;
    Ok(None)
}

/// Emits a terminal backend result after clearing any rendered partial
/// response.
fn finish_terminal_attempt<W: Write>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    writer: &mut PeerOutputWriter<W>,
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
fn finish_retry_attempt<W: Write>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    writer: &mut PeerOutputWriter<W>,
    decision: RetryDecision,
    has_partial_output: bool,
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
    }))
}

/// Finishes a cancellation after clearing any rendered partial response.
fn finish_canceled_attempt<W: Write>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    writer: &mut PeerOutputWriter<W>,
    has_partial_output: bool,
) -> Result<Option<PromptAttemptRetry>, Box<dyn Error>> {
    clear_partial_backend_response(
        agent_prompt_id,
        prompt,
        writer,
        has_partial_output,
        "request canceled; discarding partial provider output",
    )?;
    finish_canceled(agent_prompt_id, prompt, writer)?;
    Ok(None)
}

/// Clears partial provider text only when the backend reported semantic output.
fn clear_partial_backend_response<W: Write>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    writer: &mut PeerOutputWriter<W>,
    has_partial_output: bool,
    detail: &str,
) -> Result<(), Box<dyn Error>> {
    if has_partial_output {
        emit_chat_completions_partial_clear(agent_prompt_id, prompt, detail, writer)?;
    }
    Ok(())
}

/// Serializes and flushes one terminal provider response report.
fn emit_finished_backend_response<W: Write>(
    writer: &mut PeerOutputWriter<W>,
    finished: ProviderResponseFinished,
) -> Result<(), Box<dyn Error>> {
    writer.write_message(&HarnessInputMessage::emit_transient(
        Event::ProviderResponseFinishedReported(finished),
    ))?;
    writer.flush()?;
    Ok(())
}

fn emit_chat_completions_partial_clear<W: Write>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    text: &str,
    writer: &mut PeerOutputWriter<W>,
) -> Result<(), Box<dyn Error>> {
    writer.write_message(&HarnessInputMessage::emit_transient(
        Event::ProviderResponseUpdatedReported(ProviderResponseUpdated {
            agent_prompt_id: agent_prompt_id.clone(),
            agent_id: prompt.agent_id.clone(),
            deltas: Vec::new(),
            compaction: None,
            status: Some(ProviderResponseStatusUpdate {
                text: text.to_owned(),
                clear_response: true,
                retry: None,
            }),
            response_stats: None,
            originator: prompt.originator.clone(),
        }),
    ))?;
    writer.flush()?;
    Ok(())
}

/// Shared immutable inputs for one ChatGPT provider prompt attempt.
#[derive(Clone, Copy)]
struct ChatGptPromptExecutionContext<'a> {
    /// Whether durable-session policy permits provider debug captures.
    debug_provider_requests: bool,
    /// Shared ChatGPT transport runtime and WebSocket pool.
    runtime: &'a CodexRuntime,
    /// One-based finite-attempt ordinal owned by this prompt execution.
    logical_attempt: tau_provider_codex::LogicalAttempt,
}

/// Retry evidence returned by one finite provider attempt.
struct PromptAttemptRetry {
    /// Closed scheduler decision.
    decision: RetryDecision,
    /// Bounded provider detail for ordinary live status only.
    live_detail: Option<tau_provider_codex::RedactedProviderDetail>,
}

fn handle_compact_prompt<R, W: Write>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    config: &ResolvedConfig,
    prompt: &tau_proto::AgentPromptCreated,
    request: &CodexPrompt<'_>,
    writer: &mut PeerOutputWriter<W>,
    retry_ctx: &mut R,
    execution: ChatGptPromptExecutionContext<'_>,
) -> Result<Option<PromptAttemptRetry>, Box<dyn Error>>
where
    R: TurnAbort,
{
    // Standalone compaction deliberately has no inline fallback.
    match execution
        .runtime
        .compact(agent_prompt_id, config, request, retry_ctx)
    {
        CompactOutcome::Finished(output_items) => {
            writer.write_message(&HarnessInputMessage::emit_transient(
                Event::ProviderResponseFinishedReported(ProviderResponseFinished {
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
                    originator: prompt.originator.clone(),
                    usage: None,
                    compaction_original_input_tokens: None,
                    compaction_compacted_input_tokens: None,
                    backend: Some(backend_descriptor(
                        config,
                        ProviderBackendTransport::HttpSse,
                        false,
                    )),
                    provider_response_id: None,
                    ws_pool_delta: None,
                }),
            ))?;
            writer.flush()?;
            Ok(None)
        }
        CompactOutcome::Retry(decision) => Ok(Some(PromptAttemptRetry {
            decision,
            live_detail: None,
        })),
        CompactOutcome::Canceled => {
            finish_canceled(agent_prompt_id, prompt, writer)?;
            Ok(None)
        }
        CompactOutcome::Terminal(error) => {
            let backend = backend_descriptor(config, ProviderBackendTransport::HttpSse, false);
            finish_error(
                agent_prompt_id,
                prompt,
                &backend,
                error,
                None,
                execution.debug_provider_requests,
                writer,
            )?;
            Ok(None)
        }
    }
}

fn handle_prompt<R, W: Write>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    config: &ResolvedConfig,
    prompt: &tau_proto::AgentPromptCreated,
    writer: &mut PeerOutputWriter<W>,
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
    let mut ws_pool_delta = None;
    let mut response_update_emitter = RateLimitedResponseUpdateEmitter::new();
    let mut on_update = |update: StreamUpdate<'_>| match update {
        StreamUpdate::Connecting => {
            emit_chatgpt_connecting_update(agent_prompt_id, &prompt.agent_id, &originator, writer);
        }
        StreamUpdate::Dispatched(at) => {
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
            finish_canceled(agent_prompt_id, prompt, writer)?
        }
        CodexAttemptOutcome::Terminal { error, progress } if error.repetition().is_some() => {
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
            finish_error(
                agent_prompt_id,
                prompt,
                &backend,
                error,
                ws_pool_delta,
                execution.debug_provider_requests,
                writer,
            )?
        }
        CodexAttemptOutcome::Retry {
            decision,
            progress,
            live_detail,
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
            }));
        }
        CodexAttemptOutcome::Terminal { error, progress } => {
            if progress == CodexSemanticProgress::Parsed {
                emit_chat_completions_partial_clear(
                    agent_prompt_id,
                    prompt,
                    "provider stream ended with an error; discarding partial output",
                    writer,
                )?;
            }
            let backend = backend_descriptor(config, transport_taken, false);
            finish_error(
                agent_prompt_id,
                prompt,
                &backend,
                error,
                ws_pool_delta,
                execution.debug_provider_requests,
                writer,
            )?
        }
    }
    Ok(None)
}

fn emit_chatgpt_connecting_update<W: Write>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    agent_id: &tau_proto::AgentId,
    originator: &tau_proto::PromptOriginator,
    writer: &mut PeerOutputWriter<W>,
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
        }),
        response_stats: None,
        originator: originator.clone(),
    };
    let _ = writer.write_message(&HarnessInputMessage::emit_transient(
        Event::ProviderResponseUpdatedReported(update),
    ));
    let _ = writer.flush();
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
        }
    }

    /// Aligns public elapsed time to the backend's typed dispatch boundary.
    fn mark_dispatched(&mut self, dispatched_at: Instant) {
        if self.last_update_emitted_at.is_none() {
            self.started_at = dispatched_at;
        }
    }

    fn emit_if_due<W: Write>(
        &mut self,
        agent_prompt_id: &tau_proto::AgentPromptId,
        agent_id: &tau_proto::AgentId,
        originator: &tau_proto::PromptOriginator,
        state: &CodexStreamState,
        writer: &mut PeerOutputWriter<W>,
    ) {
        let target = ResponseUpdateTarget {
            agent_prompt_id,
            agent_id,
            originator,
        };
        self.emit_at(&target, state, writer, Instant::now(), false);
    }

    fn emit_terminal_flush<W: Write>(
        &mut self,
        agent_prompt_id: &tau_proto::AgentPromptId,
        agent_id: &tau_proto::AgentId,
        originator: &tau_proto::PromptOriginator,
        state: &CodexStreamState,
        writer: &mut PeerOutputWriter<W>,
    ) {
        let target = ResponseUpdateTarget {
            agent_prompt_id,
            agent_id,
            originator,
        };
        self.emit_at(&target, state, writer, Instant::now(), true);
    }

    fn emit_at<W: Write>(
        &mut self,
        target: &ResponseUpdateTarget<'_>,
        state: &CodexStreamState,
        writer: &mut PeerOutputWriter<W>,
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
        if !terminal_flush
            && !first_non_empty_sample
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
        if emit_chatgpt_stream_update(
            target.agent_prompt_id,
            target.agent_id,
            target.originator,
            state,
            &mut self.delta_emitter,
            response_stats,
            writer,
        ) {
            self.last_stats_sample = response_stats.current;
            self.last_update_emitted_at = Some(now);
            self.emitted_non_empty_sample |= response_stats.current.response_bytes_received > 0;
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

fn emit_chatgpt_stream_update<W: Write>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    agent_id: &tau_proto::AgentId,
    originator: &tau_proto::PromptOriginator,
    state: &CodexStreamState,
    delta_emitter: &mut CodexStreamDeltaEmitter,
    response_stats: ProviderResponseStats,
    writer: &mut PeerOutputWriter<W>,
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
    {
        return false;
    }
    let Ok(()) = writer.write_message(&HarnessInputMessage::emit_transient(
        Event::ProviderResponseUpdatedReported(ProviderResponseUpdated {
            agent_prompt_id: agent_prompt_id.clone(),
            agent_id: agent_id.clone(),
            deltas,
            compaction,
            status: None,
            response_stats: Some(response_stats),
            originator: originator.clone(),
        }),
    )) else {
        return false;
    };
    writer.flush().is_ok()
}

fn backend_descriptor(
    config: &ResolvedConfig,
    transport: ProviderBackendTransport,
    stale_chain_fallback: bool,
) -> ProviderBackend {
    ProviderBackend {
        kind: ProviderBackendKind::Responses,
        base_url: config.base_url().to_owned(),
        transport,
        stale_chain_fallback,
    }
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
fn finish_stream<W: Write>(
    session_id: &tau_proto::SessionId,
    agent_prompt_id: &tau_proto::AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    request: &CodexPrompt<'_>,
    backend: &ProviderBackend,
    state: CodexStreamState,
    debug_capture: tau_provider_codex::CodexDebugCapture,
    ws_pool_delta: Option<tau_proto::WsPoolDelta>,
    debug_provider_requests: bool,
    writer: &mut PeerOutputWriter<W>,
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
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: agent_prompt_id.clone(),
        agent_id: prompt.agent_id.clone(),
        stop_reason: stop_reason_from_output_items(&output_items),
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_items,
        originator: prompt.originator.clone(),
        usage,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: Some(backend.clone()),
        provider_response_id,
        ws_pool_delta,
    };
    maybe_debug_submit_provider_response(
        session_id,
        &finished,
        debug_provider_requests,
        Some(&debug_capture),
    );
    let diagnostic = cache_miss_diagnostic(prompt, request, &finished);
    if let Some(diagnostic) = diagnostic {
        writer.write_message(&HarnessInputMessage::emit_transient(
            Event::ProviderCacheMissDiagnosticReported(diagnostic),
        ))?;
    }
    writer.write_message(&HarnessInputMessage::emit_transient(
        Event::ProviderResponseFinishedReported(finished),
    ))?;
    writer.flush()?;
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

fn finish_error<W: Write>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    prompt: &tau_proto::AgentPromptCreated,
    backend: &ProviderBackend,
    error: CodexError,
    ws_pool_delta: Option<tau_proto::WsPoolDelta>,
    debug_provider_requests: bool,
    writer: &mut PeerOutputWriter<W>,
) -> Result<(), Box<dyn Error>> {
    let finished = ProviderResponseFinished {
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
        originator: prompt.originator.clone(),
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: Some(backend.clone()),
        provider_response_id: None,
        ws_pool_delta,
    };
    maybe_debug_submit_provider_response(
        &prompt.session_id,
        &finished,
        debug_provider_requests,
        None,
    );
    writer.write_message(&HarnessInputMessage::emit_transient(
        Event::ProviderResponseFinishedReported(finished),
    ))?;
    writer.flush()?;
    Ok(())
}

fn emit_repetition_detected_update<W: Write>(
    agent_prompt_id: &tau_proto::AgentPromptId,
    agent_id: &tau_proto::AgentId,
    originator: &tau_proto::PromptOriginator,
    repetition: &tau_provider::StreamRepetition,
    writer: &mut PeerOutputWriter<W>,
) {
    let text = bounded_provider_error(&format!(
        "provider stream repetition detected; aborting response ({repetition})"
    ));
    let _ = writer.write_message(&HarnessInputMessage::emit_transient(
        Event::ProviderResponseUpdatedReported(ProviderResponseUpdated {
            agent_prompt_id: agent_prompt_id.clone(),
            agent_id: agent_id.clone(),
            deltas: Vec::new(),
            compaction: None,
            status: Some(ProviderResponseStatusUpdate {
                text,
                clear_response: true,
                retry: None,
            }),
            response_stats: None,
            originator: originator.clone(),
        }),
    ));
    let _ = writer.flush();
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

#[cfg(test)]
mod openai_tests;
#[cfg(test)]
mod scheduler_model_tests;
#[cfg(test)]
mod tests;
