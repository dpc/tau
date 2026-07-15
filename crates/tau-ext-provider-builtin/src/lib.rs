//! Built-in provider registry extension.
//!
//! This crate owns Tau's built-in provider process, profile CLI, auth/profile
//! storage scan, model publication, and dispatch across built-in provider
//! backends. Individual backend crates own provider-specific wire formats.
//! Component responsibilities and trust boundaries are summarized in
//! `ARCH-tau-ext-provider-builtin`.
//! See `DESIGN-tau-ext-provider-builtin-testing-boundary` for that test
//! boundary.
//! Retry telemetry and debug-capture persistence follow
//! `DESIGN-tau-ext-provider-builtin-structured-retry-facts` and
//! `DESIGN-tau-ext-provider-builtin-durable-session-diagnostics`.

mod prewarm;
#[cfg(feature = "quota-test-support")]
mod quota_test_support;
#[cfg(any(test, feature = "quota-test-support"))]
mod scripted_http;

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

use dialoguer::{Confirm, Input};
use prewarm::{PrewarmAbort, PrewarmKey, PrewarmSupervisor};
#[cfg(feature = "quota-test-support")]
pub use quota_test_support::run_quota_recovery_fixture;
use serde::{Deserialize, Serialize};
use tau_client::{
    ClientError, ClientHandle, ClientResult, DispatchOutcome, ExtensionBuilder,
    ManualExtensionRuntime, ManualRuntimePoll, ManualRuntimeWaker, RawEventContext, TauExtension,
    TauExtensionRunner,
};
use tau_proto::{
    ClientKind, ContextItem, Event, EventName, HarnessInputMessage, HarnessInputReader, ModelId,
    ModelName, PeerOutputWriter, ProviderBackend, ProviderBackendKind, ProviderBackendTransport,
    ProviderCacheMissDiagnostic, ProviderModelInfo, ProviderModelsUpdated, ProviderName,
    ProviderPromptSubmitted, ProviderResponseFinished, ProviderResponseStats,
    ProviderResponseStatusUpdate, ProviderResponseUpdated, ProviderStopReason,
};
use tau_provider::retry_policy::{RetryClass, RetryDecision};
use tau_provider::storage::{AuthFile, ProviderStore};
use tau_provider_chat_completions::openrouter::{OpenRouterProfile, fetch_openrouter_models};
use tau_provider_chat_completions::{
    ChatCompletionsModel, ChatCompletionsProvider, PromptAttemptOutcome,
    models_for_provider as chat_models_for_provider, run_prompt_attempt_for_provider,
};
use tau_provider_chatgpt::{
    ChatGptRuntime, ChatGptTurnState, StreamUpdate, TurnAbort, TurnAbortWaker, common, responses,
};

/// `tracing` target for events emitted from this extension.
pub const LOG_TARGET: &str = "provider-builtin";

const EXTENSION_NAME: &str = "tau-ext-provider-builtin";
const CHATGPT_PROVIDER_NAME: &str = "chatgpt";
const DEFAULT_RESPONSES_LITE_COMPATIBILITY: bool = false;
/// One built-in provider profile loaded from `auth.d/<provider>.json`.
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
    fn responses_mode(&self) -> responses::ResponsesMode {
        if self.responses_lite_compatibility {
            responses::ResponsesMode::LiteCompatibility
        } else {
            responses::ResponsesMode::Standard
        }
    }

    fn replace_auth(&mut self, refreshed: OpenAiAuth) {
        self.auth = refreshed;
    }
}

/// Registered built-in provider profiles keyed by filename-derived namespace.
#[derive(Clone, Debug, Default)]
pub struct BuiltinProviderProfiles {
    providers: BTreeMap<ProviderName, BuiltinProviderProfile>,
}

impl BuiltinProviderProfiles {
    fn startup_responses_modes(&self) -> BTreeMap<ProviderName, responses::ResponsesMode> {
        self.providers
            .iter()
            .filter_map(|(provider, profile)| match profile {
                BuiltinProviderProfile::Chatgpt(profile) => {
                    Some((provider.clone(), profile.responses_mode()))
                }
                BuiltinProviderProfile::ChatCompletions(_)
                | BuiltinProviderProfile::OpenRouter(_) => None,
            })
            .collect()
    }

    fn apply_startup_responses_modes(
        &mut self,
        startup_modes: &BTreeMap<ProviderName, responses::ResponsesMode>,
    ) {
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
                tau_provider_chatgpt::models_for_provider_mode(provider, profile.responses_mode())
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
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
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
    identity: u64,
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

    fn ensure_profile(&mut self, provider: ProviderName, identity: u64) -> Option<Event> {
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
        Some(Event::ProviderQuotaReplace(
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
        Some(Event::ProviderQuotaClear(tau_proto::ProviderQuotaClear {
            provider: provider.clone(),
            profile_epoch: current.epoch,
            sequence: current.sequence.saturating_add(1),
        }))
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
                        .is_none_or(|remaining| age_seconds >= remaining)
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
        snapshot: tau_provider_chatgpt::quota::FullQuotaSnapshot,
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
        Some(Event::ProviderQuotaReplace(
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
        profile_identity: u64,
        observation: tau_provider_chatgpt::quota::RollingQuotaObservation,
        observed_at_unix_ms: u64,
    ) -> Option<Event> {
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
        Some(Event::ProviderQuotaPatch(tau_proto::ProviderQuotaPatch {
            provider,
            profile_epoch: current.epoch.clone(),
            sequence,
            windows,
            removed_window_keys: Vec::new(),
            route_bindings,
        }))
    }
}

fn quota_profile_identity(config: &responses::ResponsesConfig) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    config.base_url.hash(&mut hasher);
    config.account_id.hash(&mut hasher);
    config.api_key.hash(&mut hasher);
    hasher.finish()
}

fn responses_profile_identity(config: &responses::ResponsesConfig) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    "responses".hash(&mut hasher);
    config.base_url.hash(&mut hasher);
    config.account_id.hash(&mut hasher);
    config.api_key.hash(&mut hasher);
    hasher.finish()
}

fn backend_profile_identity(backend: &PromptBackend) -> Option<u64> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    match backend {
        PromptBackend::Unavailable => return None,
        PromptBackend::Responses(config) => return Some(responses_profile_identity(config)),
        PromptBackend::ChatCompletions { provider, .. } => {
            "chat_completions".hash(&mut hasher);
            provider.base_url.hash(&mut hasher);
            provider.api_key.hash(&mut hasher);
        }
    }
    Some(hasher.finish())
}

fn full_quota_window(
    observation: tau_provider_chatgpt::quota::QuotaWindowObservation,
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
    sparse: tau_provider_chatgpt::quota::QuotaWindowObservation,
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
    match args.first().map(String::as_str).unwrap_or("help") {
        "add" => cmd_add(&args[1..])?,
        "remove" | "delete" => cmd_remove(args.get(1).map(String::as_str))?,
        "list" | "status" => cmd_list()?,
        "help" | "--help" | "-h" => println!("{PROVIDER_CLI_HELP}"),
        other => return Err(format!("unknown provider subcommand: {other}").into()),
    }
    Ok(())
}

const PROVIDER_CLI_HELP: &str = "\
Usage: tau provider <subcommand>

Subcommands:
  add                            Add or replace a provider profile interactively
  remove <name>                  Remove a provider profile
  list                           List provider profiles";

fn cmd_add(args: &[String]) -> Result<(), Box<dyn Error>> {
    if !args.is_empty() {
        return Err(
            "tau provider add does not accept arguments; it prompts for all provider details"
                .into(),
        );
    }
    let kind: String = Input::new()
        .with_prompt("Provider kind (chatgpt, chat-completions, or openrouter)")
        .default("chatgpt".to_owned())
        .interact_text()?;
    match kind.trim() {
        "chatgpt" => cmd_add_chatgpt()?,
        "chat-completions" => cmd_add_chat_completions()?,
        "openrouter" => cmd_add_openrouter()?,
        other => return Err(format!("unknown provider kind: {other}").into()),
    }
    Ok(())
}

fn cmd_add_chatgpt() -> Result<(), Box<dyn Error>> {
    let name = prompt_provider_name("chatgpt")?;
    let auth = run_openai_codex_login()?;
    let responses_lite_compatibility = Confirm::new()
        .with_prompt("Use legacy Responses Lite compatibility for GPT-5.6?")
        .default(DEFAULT_RESPONSES_LITE_COMPATIBILITY)
        .interact()?;
    save_profile(
        &name,
        &BuiltinProviderProfile::Chatgpt(ChatGptProfile {
            auth,
            responses_lite_compatibility,
        }),
    )?;
    Ok(())
}

fn cmd_add_chat_completions() -> Result<(), Box<dyn Error>> {
    let name = prompt_provider_name("local")?;
    let base_url: String = Input::new()
        .with_prompt("Base URL")
        .default("https://api.openai.com/v1".to_owned())
        .interact_text()?;
    let api_key: String = Input::new()
        .with_prompt("API key (empty for keyless/local providers)")
        .allow_empty(true)
        .interact_text()?;
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
    save_profile(&name, &BuiltinProviderProfile::ChatCompletions(profile))?;
    Ok(())
}

fn chat_completions_add_compat() -> tau_provider_chat_completions::ChatCompletionsCompat {
    tau_provider_chat_completions::ChatCompletionsCompat {
        max_completion_tokens: false,
        ..tau_provider_chat_completions::ChatCompletionsCompat::openai_defaults()
    }
}

fn cmd_add_openrouter() -> Result<(), Box<dyn Error>> {
    let name = prompt_provider_name("openrouter")?;
    let api_key: String = Input::new()
        .with_prompt("API key")
        .allow_empty(true)
        .interact_text()?;
    let models_input: String = Input::new()
        .with_prompt("Models (comma-separated, or press enter to fetch from OpenRouter)")
        .allow_empty(true)
        .interact_text()?;
    let models = if models_input.trim().is_empty() {
        eprintln!("Fetching models from OpenRouter...");
        fetch_openrouter_models(&api_key)?
    } else {
        parse_chat_model_list(&models_input)?
    };
    let profile = OpenRouterProfile { api_key, models };
    save_profile(&name, &BuiltinProviderProfile::OpenRouter(profile))?;
    Ok(())
}

fn cmd_remove(name_arg: Option<&str>) -> Result<(), Box<dyn Error>> {
    let name = match name_arg {
        Some(name) => ProviderName::try_new(name.trim().to_owned())
            .map_err(|error| format!("invalid provider namespace '{name}': {error}"))?,
        None => prompt_provider_name(CHATGPT_PROVIDER_NAME)?,
    };
    let file = AuthFile::<BuiltinProviderProfile>::open_default(name.as_str())?;
    if file.delete()? {
        eprintln!("Removed provider profile '{name}'.");
    } else {
        eprintln!("Provider profile '{name}' was not configured.");
    }
    Ok(())
}

fn cmd_list() -> Result<(), Box<dyn Error>> {
    let profiles = load_profiles();
    if profiles.providers.is_empty() {
        println!("No provider profiles configured.");
        return Ok(());
    }
    for (name, profile) in profiles.providers {
        match profile {
            BuiltinProviderProfile::Chatgpt(profile) => {
                let status = if profile.auth.access_token.trim().is_empty()
                    && profile.auth.refresh_token.trim().is_empty()
                {
                    "not-configured"
                } else if now_ms() < profile.auth.expires_at_ms {
                    "logged-in"
                } else {
                    "expired"
                };
                let mode = if profile.responses_lite_compatibility {
                    "responses-lite-compatibility"
                } else {
                    "responses-standard"
                };
                println!("{name}\tchatgpt\t{status}\t{mode}");
            }
            BuiltinProviderProfile::ChatCompletions(provider) => {
                let auth_status = if provider.api_key.trim().is_empty() {
                    "no-api-key"
                } else {
                    "api-key"
                };
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
                let auth_status = if profile.api_key.trim().is_empty() {
                    "no-api-key"
                } else {
                    "api-key"
                };
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
        }
    }
    Ok(())
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
        });
    }
    if models.is_empty() {
        return Err("at least one model is required".into());
    }
    Ok(models)
}

fn save_profile(
    name: &ProviderName,
    profile: &BuiltinProviderProfile,
) -> Result<(), Box<dyn Error>> {
    let file = AuthFile::<BuiltinProviderProfile>::open_default(name.as_str())?;
    file.save(profile)?;
    eprintln!("Provider profile saved to: {}", file.path().display());
    Ok(())
}

fn run_openai_codex_login() -> Result<OpenAiAuth, Box<dyn Error>> {
    let (auth_url, expected_state, verifier) = tau_provider::oauth::openai_codex_auth_url();

    eprintln!("\nOpen this URL in your browser:\n");
    eprintln!("{auth_url}");
    eprintln!("\x1b]8;;{auth_url}\x1b\\Or click here.\x1b]8;;\x1b\\");
    eprintln!();
    eprintln!("After logging in, you'll be redirected to a page that won't load.");
    eprintln!("Copy the full URL from your browser's address bar and paste it here:\n");

    std::io::stdout().flush()?;
    let redirect_input: String = Input::new().with_prompt("Redirect URL").interact_text()?;

    let (code, state) = tau_provider::oauth::parse_redirect_url(&redirect_input)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

    if state != expected_state {
        return Err("state mismatch — possible CSRF attack or stale URL".into());
    }

    eprintln!("Exchanging code for tokens...");
    let tokens = tau_provider::oauth::openai_codex_exchange(&code, &verifier)?;

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
    let startup_profiles = load_profiles();
    run_inner(reader, writer, startup_profiles, load_profiles)
}

fn load_profiles() -> BuiltinProviderProfiles {
    match load_profiles_result() {
        Ok(profiles) => profiles,
        Err(error) => {
            tracing::warn!(
                target: LOG_TARGET,
                error = %error,
                "failed to load provider profiles; publishing no models"
            );
            BuiltinProviderProfiles::default()
        }
    }
}

fn load_profiles_result() -> std::io::Result<BuiltinProviderProfiles> {
    let store = ProviderStore::open_default()?;
    let mut profiles = BuiltinProviderProfiles::default();
    let auth_dir = store.auth_dir();
    let entries = match std::fs::read_dir(&auth_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(profiles),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Ok(name) = ProviderName::try_new(stem.to_owned()) else {
            tracing::warn!(target: LOG_TARGET, path = %path.display(), "skipping provider profile with invalid filename");
            continue;
        };
        let file = match store.auth_file::<BuiltinProviderProfile>(stem.to_owned()) {
            Ok(file) => file,
            Err(error) => {
                tracing::warn!(target: LOG_TARGET, path = %path.display(), error = %error, "skipping provider profile with invalid auth file name");
                continue;
            }
        };
        match file.load() {
            Ok(Some(profile)) => {
                profiles.providers.insert(name, profile);
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(target: LOG_TARGET, path = %path.display(), error = %error, "skipping invalid provider profile");
            }
        }
    }
    Ok(profiles)
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
    BuiltinProviderProfiles { providers }
}

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
    run_inner_with_executors_and_clock(
        reader,
        writer,
        startup_profiles,
        load_prompt_profiles,
        prompt_concurrency_limit,
        RuntimeExecutors {
            prompt: prompt_executor,
            prewarm: prewarm_executor,
            retry_clock: Arc::new(SystemRetryClock),
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
    let (worker_tx, worker_rx) = mpsc::channel::<WorkerMessage>();
    let startup_responses_modes = startup_profiles.startup_responses_modes();
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
        chatgpt_runtime: Arc::new(ChatGptRuntime::new()),
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
    };
    let mut runtime = TauExtensionRunner::new(ProviderExtension::<F>::new(startup_profiles))
        .start_manual_loop(reader, writer, runtime)?;
    let worker_waker = runtime.waker();
    runtime.state_mut().set_worker_waker(worker_waker);
    let handle = runtime.handle();
    runtime.state_mut().initialize_quota(&handle)?;
    run_provider_loop(runtime)
}

/// Tau-client declaration for the built-in provider peer.
struct ProviderExtension<F> {
    /// Provider profiles used to publish startup model availability.
    startup_profiles: BuiltinProviderProfiles,
    /// Marker tying the declaration to the runtime state's profile loader type.
    _load_prompt_profiles: PhantomData<fn() -> F>,
}

impl<F> ProviderExtension<F> {
    /// Creates a provider declaration for the supplied startup profiles.
    fn new(startup_profiles: BuiltinProviderProfiles) -> Self {
        Self {
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
        // No past effectful provider events requested: provider work starts from
        // fresh live state. Harness session directory announcements are
        // current-state facts, so replay catch-up is allowed for diagnostics
        // policy only.
        builder
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
            .startup_event(Event::ProviderModelsUpdated(ProviderModelsUpdated {
                models: models_for_profiles(&self.startup_profiles),
            }))
            .ready_message("builtin provider ready");
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
    startup_responses_modes: BTreeMap<ProviderName, responses::ResponsesMode>,
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
    chatgpt_runtime: Arc<ChatGptRuntime>,
    /// Main-loop ownership and cancellation for prewarm workers.
    prewarm_supervisor: PrewarmSupervisor,
    /// Last resolved inference identity for every configured provider
    /// namespace.
    provider_profile_identities: BTreeMap<ProviderName, Option<u64>>,
    /// Last Responses identity used to supervise prewarm transport state.
    prewarm_profile_identities: BTreeMap<ProviderName, u64>,
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
}

impl<F> ProviderRuntime<F>
where
    F: FnMut() -> BuiltinProviderProfiles + 'static,
{
    fn load_profiles(&mut self) -> BuiltinProviderProfiles {
        let mut profiles = (self.load_prompt_profiles)();
        profiles.apply_startup_responses_modes(&self.startup_responses_modes);
        profiles
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
        for (provider, config) in profiles.resolve_initial_quota_backends(resolve_responses_backend)
        {
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
        config: &responses::ResponsesConfig,
        handle: &ClientHandle,
    ) -> ClientResult<bool> {
        self.reconcile_prewarm_profile(provider, config);
        let identity = quota_profile_identity(config);
        if let Some(event) = self.quota.ensure_profile(provider.clone(), identity) {
            handle.send(HarnessInputMessage::emit(event))?;
        }
        Ok(self.start_quota_fetch_if_due(provider, config))
    }

    fn start_quota_fetch_if_due(
        &mut self,
        provider: &ProviderName,
        config: &responses::ResponsesConfig,
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
        let base_url = config.base_url.clone();
        let access_token = config.api_key.clone();
        let account_id = config.account_id.clone();
        thread::spawn(move || {
            let result = tau_provider_chatgpt::quota::fetch_usage(
                &base_url,
                &access_token,
                account_id.as_deref(),
            );
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
        let backend = resolve_prompt_backend(model, profiles).unwrap_or(PromptBackend::Unavailable);
        self.reconcile_provider_profile(&model.provider, backend_profile_identity(&backend));
        if let PromptBackend::Responses(config) = &backend {
            let _ = self.ensure_quota_profile(&model.provider, config, handle)?;
        } else {
            self.clear_prewarm_profile(&model.provider);
            if let Some(event) = self.quota.clear_profile(&model.provider) {
                handle.send(HarnessInputMessage::emit(event))?;
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
        let Some((model, config)) = resolve_prewarm_backend(&prewarm, &mut profiles) else {
            if let Some(provider) = requested_provider {
                self.clear_prewarm_profile(&provider);
            }
            return Ok(());
        };
        self.reconcile_provider_profile(&model.provider, Some(responses_profile_identity(&config)));
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
        let runtime = self.chatgpt_runtime.clone();
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
            self.cancel_all_prewarms();
            if let Err(error) = self.chatgpt_runtime.invalidate_all_websockets() {
                tracing::debug!(
                    target: LOG_TARGET,
                    "failed to invalidate websocket pool after profile change: {error}",
                );
            }
        }
    }

    fn reconcile_prewarm_profile(
        &mut self,
        provider: &ProviderName,
        config: &responses::ResponsesConfig,
    ) {
        let identity = quota_profile_identity(config);
        let changed = self
            .prewarm_profile_identities
            .insert(provider.clone(), identity)
            .is_some_and(|previous| previous != identity);
        if changed {
            self.cancel_all_prewarms();
            if let Err(error) = self.chatgpt_runtime.invalidate_all_websockets() {
                tracing::debug!(
                    target: LOG_TARGET,
                    "failed to invalidate websocket pool after profile change: {error}",
                );
            }
        }
    }

    fn clear_prewarm_profile(&mut self, provider: &ProviderName) {
        if self.prewarm_profile_identities.remove(provider).is_some() {
            self.cancel_all_prewarms();
            if let Err(error) = self.chatgpt_runtime.invalidate_all_websockets() {
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
            emit_retry_status(&job, cooldown.class, due, now, handle)?;
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
                session_id: tau_proto::SessionId::default(),
                target_agent_id: None,
                agent_prompt_id: None,
            },
            handle,
        )?;
        if let Err(error) = self.chatgpt_runtime.invalidate_all_websockets() {
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
                Ok(WorkerMessage::Retry { mut job, decision }) => {
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
                    let hint_delay = decision.retry_after.unwrap_or(Duration::ZERO);
                    let hint_jitter = decision.retry_after.map_or(Duration::ZERO, |_| {
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
                    emit_retry_status(&job, decision.class, due, now, handle)?;
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
                    frame_writer.write_message(&HarnessInputMessage::emit(
                        Event::ProviderRetryPromptResult(tau_proto::ProviderRetryPromptResult {
                            request_id,
                            agent_prompt_id,
                            status,
                        }),
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
                        handle.send(HarnessInputMessage::emit(event))?;
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
                            handle.send(HarnessInputMessage::emit(event))?;
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
                            .and_then(|model| resolve_responses_backend(&model.id, &mut profiles));
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
                            handle.send(HarnessInputMessage::emit(event))?;
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
            emit_retry_status(&job, cooldown.class, due, now, handle)?;
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
            chatgpt_runtime: self.chatgpt_runtime.clone(),
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
    let HarnessInputMessage::Emit(emit) = message.as_ref() else {
        return None;
    };
    let Event::ProviderResponseFinished(finished) = emit.event.as_ref() else {
        return None;
    };
    cancellation.take_canceled(agent_prompt_id);
    Some(HarnessInputMessage::emit(Event::ProviderResponseFinished(
        simple_finished(
            finished.agent_prompt_id.clone(),
            finished.agent_id.clone(),
            finished.originator.clone(),
            "(cancelled by harness)",
        ),
    )))
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
    let Event::ProviderResponseFinished(finished) = emit.event.as_ref() else {
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
    runtime: Arc<ChatGptRuntime>,
    /// Resolved immutable backend configuration.
    config: responses::ResponsesConfig,
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
/// See `DESIGN-tau-ext-provider-builtin-required-work-retries`.
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
    Responses(responses::ResponsesConfig),
    ChatCompletions {
        provider: ChatCompletionsProvider,
        model: ChatCompletionsModel,
    },
}

struct PromptExecution {
    job: PromptJob,
    /// Cooldown generation this exact finite attempt may invalidate.
    cooldown_probe: Option<CooldownProbe>,
    output_tx: Sender<WorkerMessage>,
    output_waker: ManualRuntimeWaker,
    cancellation: Arc<CancellationState>,
    chatgpt_runtime: Arc<ChatGptRuntime>,
}

struct PromptWorkerContext {
    worker_tx: Sender<WorkerMessage>,
    worker_waker: ManualRuntimeWaker,
    prompt_executor: PromptExecutor,
    cancellation: Arc<CancellationState>,
    chatgpt_runtime: Arc<ChatGptRuntime>,
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
        profile_identity: u64,
        /// Provider-normalized sparse rolling observation.
        observation: tau_provider_chatgpt::quota::RollingQuotaObservation,
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
        result: Result<
            tau_provider_chatgpt::quota::FullQuotaSnapshot,
            tau_provider_chatgpt::quota::UsageFetchError,
        >,
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
            HarnessInputMessageTarget::Handle(handle) => handle
                .send(message)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::BrokenPipe, error)),
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
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "writer closed")),
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
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?
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
        let mut on_quota = |observation: &tau_provider_chatgpt::quota::RollingQuotaObservation| {
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
                runtime: &execution.chatgpt_runtime,
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
            Ok(Some(decision)) => {
                let _ = send_worker_message(
                    &execution.output_tx,
                    &execution.output_waker,
                    WorkerMessage::Retry {
                        job: execution.job,
                        decision,
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
        chatgpt_runtime: context.chatgpt_runtime.clone(),
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
    handle: &ClientHandle,
) -> ClientResult<()> {
    let delay = due.checked_duration_since(now).unwrap_or(Duration::ZERO);
    let text = format!(
        "{}; next attempt in about {}s (attempt {}). Tau will keep trying; cancel the prompt to stop.",
        class.public_reason(),
        delay.as_secs(),
        job.retry_state.attempts,
    );
    handle.send(HarnessInputMessage::emit(Event::ProviderResponseUpdated(
        ProviderResponseUpdated {
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
        },
    )))
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

fn trace_provider_prompt(prompt: &tau_proto::AgentPromptCreated, agent_prompt_id: &str) {
    if !tracing::enabled!(target: LOG_TARGET, tracing::Level::TRACE) {
        return;
    }
    let mut redacted = prompt.clone();
    redacted.context.clear_provider_image_bytes();
    trace_prompt_like("provider prompt", &redacted, agent_prompt_id);
}

fn trace_prompt_like<T: serde::Serialize>(label: &str, value: &T, agent_prompt_id: &str) {
    if !tracing::enabled!(target: LOG_TARGET, tracing::Level::TRACE) {
        return;
    }
    match serde_json::to_string_pretty(value) {
        Ok(json) => tracing::trace!(
            target: LOG_TARGET,
            agent_prompt_id,
            "{label}:\n{json}"
        ),
        Err(error) => tracing::trace!(
            target: LOG_TARGET,
            agent_prompt_id,
            "{label} (failed to serialize for log: {error})"
        ),
    }
}

fn write_prompt_submitted<W: Write>(
    agent_prompt_id: &str,
    originator: &tau_proto::PromptOriginator,
    writer: &mut PeerOutputWriter<W>,
) -> Result<(), Box<dyn Error>> {
    writer.write_message(&HarnessInputMessage::emit(Event::ProviderPromptSubmitted(
        ProviderPromptSubmitted {
            agent_prompt_id: agent_prompt_id.into(),
            originator: originator.clone(),
        },
    )))?;
    writer.flush()?;
    Ok(())
}

fn finish_canceled<W: Write>(
    agent_prompt_id: &str,
    prompt: &tau_proto::AgentPromptCreated,
    writer: &mut PeerOutputWriter<W>,
) -> Result<(), Box<dyn Error>> {
    tracing::info!(
        target: LOG_TARGET,
        agent_prompt_id,
        "skipping provider request — already canceled by harness",
    );
    writer.write_message(&HarnessInputMessage::emit(Event::ProviderResponseFinished(
        simple_finished(
            agent_prompt_id.into(),
            prompt.agent_id.clone(),
            prompt.originator.clone(),
            "(cancelled by harness)",
        ),
    )))?;
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
) -> Option<PromptBackend> {
    match profiles.providers.get_mut(&model.provider)? {
        BuiltinProviderProfile::Chatgpt(profile) => {
            let mode = profile.responses_mode();
            resolve_chatgpt_backend(model, &model.provider, &mut profile.auth, mode)
                .map(PromptBackend::Responses)
        }
        BuiltinProviderProfile::ChatCompletions(provider) => {
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
    }
}

fn resolve_responses_backend(
    model: &ModelId,
    profiles: &mut BuiltinProviderProfiles,
) -> Option<responses::ResponsesConfig> {
    match profiles.providers.get_mut(&model.provider)? {
        BuiltinProviderProfile::Chatgpt(profile) => {
            let mode = profile.responses_mode();
            resolve_chatgpt_backend(model, &model.provider, &mut profile.auth, mode)
        }
        BuiltinProviderProfile::ChatCompletions(_) | BuiltinProviderProfile::OpenRouter(_) => None,
    }
}

fn resolve_chatgpt_backend(
    model: &ModelId,
    provider_name: &ProviderName,
    auth_store: &mut OpenAiAuth,
    mode: responses::ResponsesMode,
) -> Option<responses::ResponsesConfig> {
    resolve_chatgpt_backend_with_refresh(
        model,
        provider_name,
        auth_store,
        mode,
        refresh_chatgpt_credentials_locked,
    )
}

fn resolve_chatgpt_backend_with_refresh(
    model: &ModelId,
    provider_name: &ProviderName,
    auth_store: &mut OpenAiAuth,
    mode: responses::ResponsesMode,
    refresh: impl FnOnce(&ProviderName) -> std::io::Result<OpenAiAuth>,
) -> Option<responses::ResponsesConfig> {
    if oauth_token_should_refresh(&auth_store.access_token, auth_store.expires_at_ms)
        && !auth_store.refresh_token.trim().is_empty()
    {
        match refresh(provider_name) {
            Ok(refreshed) => {
                *auth_store = refreshed;
            }
            Err(error) => tracing::warn!(
                target: LOG_TARGET,
                provider = %provider_name,
                "failed to refresh ChatGPT credentials: {error}"
            ),
        }
    }
    if auth_store.access_token.trim().is_empty() {
        return None;
    }

    Some(tau_provider_chatgpt::config_for_model_mode(
        &model.model,
        auth_store.access_token.clone(),
        auth_store.account_id.clone(),
        mode,
    ))
}

fn refresh_chatgpt_credentials_locked(provider_name: &ProviderName) -> std::io::Result<OpenAiAuth> {
    let auth_file = AuthFile::<BuiltinProviderProfile>::open_default(provider_name.as_str())?;
    auth_file.with_lock(|locked| {
        let BuiltinProviderProfile::Chatgpt(mut profile) = locked.load()?.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "provider profile not found")
        })?
        else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "provider profile is not a ChatGPT profile",
            ));
        };
        let current = profile.auth.clone();
        if !oauth_token_should_refresh(&current.access_token, current.expires_at_ms)
            || current.refresh_token.trim().is_empty()
        {
            return Ok(current);
        }

        let tokens = tau_provider::oauth::openai_codex_refresh(&current.refresh_token)
            .map_err(std::io::Error::other)?;
        let refreshed = OpenAiAuth {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            expires_at_ms: tokens.expires_at_ms,
            account_id: tokens.account_id,
        };
        profile.replace_auth(refreshed.clone());
        locked.save(&BuiltinProviderProfile::Chatgpt(profile))?;
        Ok(refreshed)
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
    let payload = jwt.split('.').nth(1)?;
    let payload = tau_provider::oauth::base64_url_safe_no_pad_decode(payload)?;
    let claims: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    claims.get("iat")?.as_u64().map(|secs| secs * 1000)
}

#[cfg(test)]
fn emit_retry_banner<W: Write>(
    agent_prompt_id: &str,
    agent_id: &tau_proto::AgentId,
    originator: &tau_proto::PromptOriginator,
    writer: &mut PeerOutputWriter<W>,
    error: &common::LlmError,
    delay: Duration,
    attempt: usize,
) {
    let banner = format!(
        "provider error — retrying in {}s (attempt {}). Tau will keep trying; cancel to stop.\n\n> {}",
        delay.as_secs(),
        attempt,
        error,
    );
    let _ = writer.write_message(&HarnessInputMessage::emit(Event::ProviderResponseUpdated(
        ProviderResponseUpdated {
            agent_prompt_id: agent_prompt_id.into(),
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
        },
    )));
    let _ = writer.flush();
}

fn is_canceled_by_harness(error: &common::LlmError) -> bool {
    matches!(error, common::LlmError::Canceled)
}

fn resolve_prewarm_backend(
    prewarm: &tau_proto::AgentPromptPrewarmRequested,
    profiles: &mut BuiltinProviderProfiles,
) -> Option<(ModelId, responses::ResponsesConfig)> {
    let Some(model) = prewarm.model.as_ref() else {
        tracing::debug!(
            target: LOG_TARGET,
            agent_id = %prewarm.agent_id,
            "skipping prompt prewarm: no selected model",
        );
        return None;
    };
    let Some(config) = resolve_responses_backend(model, profiles) else {
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
    config: &responses::ResponsesConfig,
    chatgpt_runtime: &ChatGptRuntime,
    debug_provider_requests: bool,
    abort: &mut impl TurnAbort,
) {
    let session_id_str = prewarm.session_id.as_str();
    let request = common::PromptPayload {
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
    match chatgpt_runtime.prewarm(config, session_id_str, &request, abort) {
        Ok(()) => {
            tracing::debug!(target: LOG_TARGET, session_id = session_id_str, "completed prompt prewarm")
        }
        Err(error) => tracing::debug!(
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
    on_quota: &mut impl FnMut(&tau_provider_chatgpt::quota::RollingQuotaObservation),
) -> Result<Option<RetryDecision>, Box<dyn Error>>
where
    R: TurnAbort,
{
    match backend {
        PromptBackend::Unavailable => Ok(Some(RetryDecision::new(RetryClass::Auth))),
        PromptBackend::Responses(config) => handle_prompt(
            agent_prompt_id.as_str(),
            config,
            prompt,
            writer,
            retry_ctx,
            context,
            on_quota,
        ),
        PromptBackend::ChatCompletions { provider, model } => {
            let outcome = run_prompt_attempt_for_provider(
                agent_prompt_id,
                prompt,
                provider,
                model,
                context.debug_provider_requests,
                writer,
                &mut || TurnAbort::is_aborted(retry_ctx),
            );
            match outcome {
                PromptAttemptOutcome::Finished(finished) => {
                    if TurnAbort::is_aborted(retry_ctx) {
                        finish_canceled(agent_prompt_id, prompt, writer)?;
                        return Ok(None);
                    }
                    writer.write_message(&HarnessInputMessage::emit(
                        Event::ProviderResponseFinished(*finished),
                    ))?;
                    writer.flush()?;
                    Ok(None)
                }
                PromptAttemptOutcome::Retry(decision) => Ok(Some(decision)),
                PromptAttemptOutcome::Canceled => {
                    finish_canceled(agent_prompt_id, prompt, writer)?;
                    Ok(None)
                }
            }
        }
    }
}

/// Shared immutable inputs for one ChatGPT provider prompt attempt.
#[derive(Clone, Copy)]
struct ChatGptPromptExecutionContext<'a> {
    /// Whether durable-session policy permits provider debug captures.
    debug_provider_requests: bool,
    /// Shared ChatGPT transport runtime and WebSocket pool.
    runtime: &'a ChatGptRuntime,
}

fn handle_prompt<R, W: Write>(
    agent_prompt_id: &str,
    config: &responses::ResponsesConfig,
    prompt: &tau_proto::AgentPromptCreated,
    writer: &mut PeerOutputWriter<W>,
    retry_ctx: &mut R,
    execution: ChatGptPromptExecutionContext<'_>,
    on_quota: &mut impl FnMut(&tau_provider_chatgpt::quota::RollingQuotaObservation),
) -> Result<Option<RetryDecision>, Box<dyn Error>>
where
    R: TurnAbort,
{
    let request = common::PromptPayload {
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
        // This deliberately has no inline fallback; see
        // `DESIGN-tau-ext-provider-builtin-standalone-compaction`.
        match execution
            .runtime
            .compact(agent_prompt_id, config, &request, retry_ctx)
        {
            Ok(output_items) => {
                writer.write_message(&HarnessInputMessage::emit(
                    Event::ProviderResponseFinished(ProviderResponseFinished {
                        agent_prompt_id: agent_prompt_id.into(),
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
                return Ok(None);
            }
            Err(error) if error.retry_decision().is_some() => {
                return Ok(error.retry_decision());
            }
            Err(error) => {
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
                return Ok(None);
            }
        }
    }

    let originator = prompt.originator.clone();
    let mut chatgpt_turn_state = ChatGptTurnState::new(usize::MAX);
    let mut transport_taken = if config.supports_websocket {
        ProviderBackendTransport::Websocket
    } else {
        ProviderBackendTransport::HttpSse
    };
    let mut ws_pool_delta = None;
    let mut response_update_emitter = RateLimitedResponseUpdateEmitter::new();
    let mut on_update = |update: StreamUpdate<'_>| match update {
        StreamUpdate::Connecting => {
            emit_chatgpt_connecting_update(agent_prompt_id, &prompt.agent_id, &originator, writer);
        }
        StreamUpdate::Response(state) => {
            if let Some(observation) = state.quota_observation.as_ref() {
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
    let result = execution.runtime.stream(
        agent_prompt_id,
        config,
        &request,
        &mut chatgpt_turn_state,
        retry_ctx,
        &mut on_update,
    );
    if TurnAbort::is_aborted(retry_ctx) {
        finish_canceled(agent_prompt_id, prompt, writer)?;
        return Ok(None);
    }
    if let Ok(dispatch) = &result {
        response_update_emitter.emit_terminal_flush(
            agent_prompt_id,
            &prompt.agent_id,
            &originator,
            &dispatch.state,
            writer,
        );
    }
    match result {
        Ok(dispatch) => {
            transport_taken = dispatch.transport;
            ws_pool_delta = dispatch.ws_pool_delta;
            let backend =
                backend_descriptor(config, transport_taken, dispatch.state.stale_chain_fallback);
            finish_stream(
                prompt.session_id.as_str(),
                agent_prompt_id,
                prompt,
                &request,
                &backend,
                dispatch.state,
                ws_pool_delta,
                execution.debug_provider_requests,
                writer,
            )?
        }
        Err(error) if is_canceled_by_harness(&error) => {
            finish_canceled(agent_prompt_id, prompt, writer)?
        }
        Err(error @ common::LlmError::RepetitionDetected(_)) => {
            let common::LlmError::RepetitionDetected(repetition) = &error else {
                unreachable!()
            };
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
        Err(error) if error.retry_decision().is_some() => {
            return Ok(error.retry_decision());
        }
        Err(error) => {
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
    agent_prompt_id: &str,
    agent_id: &tau_proto::AgentId,
    originator: &tau_proto::PromptOriginator,
    writer: &mut PeerOutputWriter<W>,
) {
    let update = ProviderResponseUpdated {
        agent_prompt_id: agent_prompt_id.into(),
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
    let _ = writer.write_message(&HarnessInputMessage::emit(Event::ProviderResponseUpdated(
        update,
    )));
    let _ = writer.flush();
}

/// Samples ChatGPT streaming progress according to
/// `DESIGN-tau-provider-chatgpt-stream-update-sampling`.
struct RateLimitedResponseUpdateEmitter {
    delta_emitter: common::StreamDeltaEmitter,
    started_at: Instant,
    last_update_emitted_at: Option<Instant>,
    last_stats_sample: tau_proto::ProviderResponseStatsSample,
    emitted_non_empty_sample: bool,
}

struct ResponseUpdateTarget<'a> {
    agent_prompt_id: &'a str,
    agent_id: &'a tau_proto::AgentId,
    originator: &'a tau_proto::PromptOriginator,
}

impl RateLimitedResponseUpdateEmitter {
    fn new() -> Self {
        Self::new_at(Instant::now())
    }

    fn new_at(started_at: Instant) -> Self {
        Self {
            delta_emitter: common::StreamDeltaEmitter::default(),
            started_at,
            last_update_emitted_at: None,
            last_stats_sample: tau_proto::ProviderResponseStatsSample::default(),
            emitted_non_empty_sample: false,
        }
    }

    fn emit_if_due<W: Write>(
        &mut self,
        agent_prompt_id: &str,
        agent_id: &tau_proto::AgentId,
        originator: &tau_proto::PromptOriginator,
        state: &common::StreamState,
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
        agent_prompt_id: &str,
        agent_id: &tau_proto::AgentId,
        originator: &tau_proto::PromptOriginator,
        state: &common::StreamState,
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
        state: &common::StreamState,
        writer: &mut PeerOutputWriter<W>,
        now: Instant,
        terminal_flush: bool,
    ) {
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

    fn response_stats_at(
        &self,
        state: &common::StreamState,
        now: Instant,
    ) -> ProviderResponseStats {
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
        }
    }
}

fn emit_chatgpt_stream_update<W: Write>(
    agent_prompt_id: &str,
    agent_id: &tau_proto::AgentId,
    originator: &tau_proto::PromptOriginator,
    state: &common::StreamState,
    delta_emitter: &mut common::StreamDeltaEmitter,
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
    let Ok(()) = writer.write_message(&HarnessInputMessage::emit(Event::ProviderResponseUpdated(
        ProviderResponseUpdated {
            agent_prompt_id: agent_prompt_id.into(),
            agent_id: agent_id.clone(),
            deltas,
            compaction,
            status: None,
            response_stats: Some(response_stats),
            originator: originator.clone(),
        },
    ))) else {
        return false;
    };
    writer.flush().is_ok()
}

fn backend_descriptor(
    config: &responses::ResponsesConfig,
    transport: ProviderBackendTransport,
    stale_chain_fallback: bool,
) -> ProviderBackend {
    ProviderBackend {
        kind: ProviderBackendKind::Responses,
        base_url: config.base_url.clone(),
        transport,
        stale_chain_fallback,
    }
}

fn maybe_debug_write_provider_response(
    session_id: &str,
    response: &ProviderResponseFinished,
    debug_provider_requests: bool,
    provider_terminal_event: Option<&serde_json::Value>,
) {
    if !debug_provider_requests {
        return;
    }
    let Some(backend) = response.backend.as_ref() else {
        return;
    };
    if !matches!(backend.kind, ProviderBackendKind::Responses) {
        return;
    }
    let Some(dir) = responses::debug_provider_request_dir(session_id, debug_provider_requests)
    else {
        return;
    };
    if let Err(error) = std::fs::create_dir_all(&dir) {
        tracing::warn!(
            target: LOG_TARGET,
            session_id,
            agent_prompt_id = %response.agent_prompt_id,
            "failed to create provider response debug dir: {error}",
        );
        return;
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    let transport_label = match backend.transport {
        ProviderBackendTransport::HttpSse => "http-sse",
        ProviderBackendTransport::Websocket => "websocket",
    };
    let path = dir.join(format!(
        "{ts}-{}-{transport_label}-response.json",
        response.agent_prompt_id
    ));
    let metadata = serde_json::json!({
        "session_id": session_id,
        "agent_prompt_id": response.agent_prompt_id,
        "transport": transport_label,
        "backend": backend,
        "provider_response_id": response.provider_response_id,
        "usage": response.usage,
        "provider_response_finished": response,
        "provider_terminal_event": provider_terminal_event,
    });
    if let Err(error) = serde_json::to_vec_pretty(&metadata)
        .map_err(std::io::Error::other)
        .and_then(|bytes| std::fs::write(path, bytes))
    {
        tracing::warn!(
            target: LOG_TARGET,
            session_id,
            agent_prompt_id = %response.agent_prompt_id,
            "failed to write provider response debug log: {error}",
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn finish_stream<W: Write>(
    session_id: &str,
    agent_prompt_id: &str,
    prompt: &tau_proto::AgentPromptCreated,
    request: &common::PromptPayload<'_>,
    backend: &ProviderBackend,
    mut state: common::StreamState,
    ws_pool_delta: Option<tau_proto::WsPoolDelta>,
    debug_provider_requests: bool,
    writer: &mut PeerOutputWriter<W>,
) -> Result<(), Box<dyn Error>> {
    let input_tokens = state.input_tokens;
    let cached_tokens = state.cached_tokens;
    let output_tokens = state.output_tokens;
    tracing::debug!(
        target: LOG_TARGET,
        agent_prompt_id,
        input_tokens,
        cached_tokens,
        output_tokens,
        "provider response token usage"
    );
    let provider_terminal_event = state.provider_terminal_event.take();
    let usage = state.usage();
    let provider_response_id = state.response_id.clone();
    let output_items = state.into_output_items();
    let finished = ProviderResponseFinished {
        agent_prompt_id: agent_prompt_id.into(),
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
    maybe_debug_write_provider_response(
        session_id,
        &finished,
        debug_provider_requests,
        provider_terminal_event.as_ref(),
    );
    let diagnostic = cache_miss_diagnostic(prompt, request, &finished);
    if let Some(diagnostic) = diagnostic {
        writer.write_message(&HarnessInputMessage::emit(
            Event::ProviderCacheMissDiagnostic(diagnostic),
        ))?;
    }
    writer.write_message(&HarnessInputMessage::emit(Event::ProviderResponseFinished(
        finished,
    )))?;
    writer.flush()?;
    Ok(())
}

fn cache_miss_diagnostic(
    prompt: &tau_proto::AgentPromptCreated,
    request: &common::PromptPayload<'_>,
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
    agent_prompt_id: &str,
    prompt: &tau_proto::AgentPromptCreated,
    backend: &ProviderBackend,
    error: common::LlmError,
    ws_pool_delta: Option<tau_proto::WsPoolDelta>,
    debug_provider_requests: bool,
    writer: &mut PeerOutputWriter<W>,
) -> Result<(), Box<dyn Error>> {
    let finished = ProviderResponseFinished {
        agent_prompt_id: agent_prompt_id.into(),
        agent_id: prompt.agent_id.clone(),
        output_items: Vec::new(),
        stop_reason: match &error {
            common::LlmError::RepetitionDetected(_) => ProviderStopReason::RepetitionDetected,
            _ => ProviderStopReason::Error,
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
    maybe_debug_write_provider_response(
        prompt.session_id.as_str(),
        &finished,
        debug_provider_requests,
        None,
    );
    writer.write_message(&HarnessInputMessage::emit(Event::ProviderResponseFinished(
        finished,
    )))?;
    writer.flush()?;
    Ok(())
}

fn emit_repetition_detected_update<W: Write>(
    agent_prompt_id: &str,
    agent_id: &tau_proto::AgentId,
    originator: &tau_proto::PromptOriginator,
    repetition: &tau_provider::StreamRepetition,
    writer: &mut PeerOutputWriter<W>,
) {
    let text = bounded_provider_error(&format!(
        "provider stream repetition detected; aborting response ({repetition})"
    ));
    let _ = writer.write_message(&HarnessInputMessage::emit(Event::ProviderResponseUpdated(
        ProviderResponseUpdated {
            agent_prompt_id: agent_prompt_id.into(),
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
        },
    )));
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
                models.extend(tau_provider_chatgpt::models_for_provider_mode(
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
