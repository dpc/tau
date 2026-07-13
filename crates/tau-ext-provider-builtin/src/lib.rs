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

use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet, VecDeque};
use std::error::Error;
use std::io::{BufWriter, Cursor, Read, Write};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dialoguer::Input;
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
    ChatGptRuntime, ChatGptTurnState, TurnAbort, TurnAbortWaker, common, responses,
};

/// `tracing` target for events emitted from this extension.
pub const LOG_TARGET: &str = "provider-builtin";

const EXTENSION_NAME: &str = "tau-ext-provider-builtin";
const CHATGPT_PROVIDER_NAME: &str = "chatgpt";
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
}

/// Registered built-in provider profiles keyed by filename-derived namespace.
#[derive(Clone, Debug, Default)]
pub struct BuiltinProviderProfiles {
    providers: BTreeMap<ProviderName, BuiltinProviderProfile>,
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

#[cfg(not(test))]
const RETRY_BASE_DELAY: Duration = Duration::from_secs(10);
#[cfg(test)]
const RETRY_BASE_DELAY: Duration = Duration::from_millis(10);
const RESET_BOUNDARY_JITTER_MAX: Duration = Duration::from_secs(5);
const PROVIDER_RESPONSE_UPDATE_MIN_INTERVAL: Duration = Duration::from_secs(1);

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
    save_profile(
        &name,
        &BuiltinProviderProfile::Chatgpt(ChatGptProfile { auth }),
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
                println!("{name}\tchatgpt\t{status}");
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
        BuiltinProviderProfile::Chatgpt(ChatGptProfile { auth }),
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
    let (worker_tx, worker_rx) = mpsc::channel::<WorkerMessage>();
    let runtime = ProviderRuntime {
        load_prompt_profiles,
        prompt_concurrency_limit,
        prompt_executor,
        worker_tx,
        worker_rx,
        worker_waker: None,
        retry_scheduler: None,
        shared_cooldowns: BTreeMap::new(),
        chatgpt_runtime: Arc::new(ChatGptRuntime::new()),
        cancellation: Arc::new(CancellationState::default()),
        prompt_queue: VecDeque::new(),
        session_debug_allowed: BTreeMap::new(),
        active_prompts: 0,
        input_closed: false,
        cancel_generation: 0,
    };
    let mut runtime = TauExtensionRunner::new(ProviderExtension::<F>::new(startup_profiles))
        .start_manual_loop(reader, writer, runtime)?;
    let worker_waker = runtime.waker();
    runtime.state_mut().set_worker_waker(worker_waker);
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
    /// Maximum number of prompt workers that may run at once.
    prompt_concurrency_limit: usize,
    /// Starts provider backend execution for one prompt job.
    prompt_executor: PromptExecutor,
    /// Sender used by prompt workers to return frames and completion notices.
    worker_tx: Sender<WorkerMessage>,
    /// Receiver used by the runtime loop to drain worker output.
    worker_rx: Receiver<WorkerMessage>,
    /// Wake handle signaled after workers enqueue output or completion.
    worker_waker: Option<ManualRuntimeWaker>,
    /// Single timer scheduler shared by every delayed logical prompt.
    retry_scheduler: Option<RetryScheduler>,
    /// Account/profile cooldowns, keyed without credentials or account ids.
    shared_cooldowns: BTreeMap<ProviderName, SharedCooldown>,
    /// Shared ChatGPT backend runtime for prewarm and prompt execution.
    chatgpt_runtime: Arc<ChatGptRuntime>,
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
    /// Generation used to reject retry outcomes created before global cancel.
    cancel_generation: u64,
}

impl<F> ProviderRuntime<F>
where
    F: FnMut() -> BuiltinProviderProfiles + 'static,
{
    fn set_worker_waker(&mut self, waker: ManualRuntimeWaker) {
        self.retry_scheduler = Some(RetryScheduler::start(self.worker_tx.clone(), waker.clone()));
        self.worker_waker = Some(waker);
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
            && self.prompt_queue.is_empty()
            && self
                .retry_scheduler
                .as_ref()
                .is_none_or(RetryScheduler::is_empty)
    }

    fn begin_input_shutdown(&mut self) {
        self.input_closed = true;
        self.cancellation.shutdown();
        if let Some(scheduler) = &self.retry_scheduler {
            scheduler.cancel_all();
        }
    }

    fn handle_event(&mut self, event: Event, handle: &ClientHandle) -> ClientResult<()> {
        match event {
            Event::HarnessSessionDir(session_dir) => self.record_session_debug_policy(session_dir),
            Event::AgentPromptPrewarmRequested(prewarm) => self.prewarm_backend(prewarm),
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

    fn prewarm_backend(&mut self, prewarm: tau_proto::AgentPromptPrewarmRequested) {
        let mut profiles = (self.load_prompt_profiles)();
        handle_prewarm(
            &prewarm,
            &mut profiles,
            &self.chatgpt_runtime,
            &self.session_debug_allowed,
        );
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
        trace_prompt_like("provider prompt", &prompt, &agent_prompt_id);
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
        let mut profiles = (self.load_prompt_profiles)();
        let backend = resolve_prompt_backend(&prompt.model, &mut profiles)
            .unwrap_or(PromptBackend::Unavailable);
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
            retry_state: PromptRetryState::default(),
            cancel_generation: self.cancel_generation,
            manual_cooldown_bypass: false,
        };
        if let Some(cooldown) = self
            .shared_cooldowns
            .get(&job.prompt.model.provider)
            .copied()
            .filter(|cooldown| cooldown.not_before > Instant::now())
        {
            let due = cooldown_due_for_job(cooldown.not_before, &job);
            emit_retry_status(&job, cooldown.class, due, handle)?;
            self.retry_scheduler
                .as_ref()
                .expect("retry scheduler starts with the runtime waker")
                .schedule(job, due);
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
        let Some(apid) = cancel.agent_prompt_id else {
            self.cancellation.cancel_retry_sleeps();
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
        )
    }

    fn drain_worker_messages(&mut self, handle: &ClientHandle) -> ClientResult<()> {
        loop {
            match self.worker_rx.try_recv() {
                Ok(WorkerMessage::Output {
                    message,
                    cancel_generation,
                    agent_prompt_id,
                }) => {
                    if let Some(message) = validate_worker_output_for_commit(
                        message,
                        cancel_generation,
                        self.cancel_generation,
                        self.input_closed,
                        &agent_prompt_id,
                        &self.cancellation,
                    ) {
                        handle.send(message)?;
                    }
                }
                Ok(WorkerMessage::PromptDone) => {
                    self.active_prompts = self.active_prompts.saturating_sub(1);
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
                    let hint_jitter = if decision.retry_after.is_some() {
                        Duration::from_secs(
                            1 + stable_retry_hash(
                                job.agent_prompt_id.as_str(),
                                job.retry_state.attempts,
                            ) % RESET_BOUNDARY_JITTER_MAX.as_secs(),
                        )
                    } else {
                        Duration::ZERO
                    };
                    let now = Instant::now();
                    let common_due = now
                        .checked_add(policy_delay.max(hint_delay))
                        // An overflowing hint is nonsensical. Fall back to the
                        // class cadence rather than retrying immediately.
                        .unwrap_or_else(|| now.checked_add(policy_delay).unwrap_or(now));
                    let mut due = common_due.checked_add(hint_jitter).unwrap_or(common_due);
                    let provider = job.prompt.model.provider.clone();
                    if decision.class.shares_cooldown() {
                        let shared =
                            self.shared_cooldowns
                                .entry(provider)
                                .or_insert(SharedCooldown {
                                    not_before: common_due,
                                    class: decision.class,
                                });
                        if shared.not_before < common_due {
                            shared.not_before = common_due;
                            shared.class = decision.class;
                        } else {
                            due = cooldown_due_for_job(shared.not_before, &job);
                        }
                        self.retry_scheduler
                            .as_ref()
                            .expect("retry scheduler starts with the runtime waker")
                            .extend_cooldown(job.prompt.model.provider.clone(), shared.not_before);
                    }
                    emit_retry_status(&job, decision.class, due, handle)?;
                    self.retry_scheduler
                        .as_ref()
                        .expect("retry scheduler starts with the runtime waker")
                        .schedule(job, due);
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
                    let mut profiles = (self.load_prompt_profiles)();
                    job.backend = resolve_prompt_backend(&job.prompt.model, &mut profiles)
                        .unwrap_or(PromptBackend::Unavailable);
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
                            let mut profiles = (self.load_prompt_profiles)();
                            owned_job.backend =
                                resolve_prompt_backend(&owned_job.prompt.model, &mut profiles)
                                    .unwrap_or(PromptBackend::Unavailable);
                            owned_job.manual_cooldown_bypass = true;
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
                .filter(|cooldown| cooldown.not_before > Instant::now())
            else {
                index += 1;
                continue;
            };
            let Some(job) = self.prompt_queue.remove(index) else {
                continue;
            };
            let due = cooldown_due_for_job(cooldown.not_before, &job);
            emit_retry_status(&job, cooldown.class, due, handle)?;
            self.retry_scheduler
                .as_ref()
                .expect("retry scheduler starts with the runtime waker")
                .schedule(job, due);
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

type PromptExecutor = Arc<dyn Fn(PromptExecution) + Send + Sync + 'static>;

struct PromptJob {
    agent_prompt_id: tau_proto::AgentPromptId,
    debug_provider_requests: bool,
    prompt: tau_proto::AgentPromptCreated,
    backend: PromptBackend,
    retry_state: PromptRetryState,
    /// Runtime global-cancel generation at logical prompt creation.
    cancel_generation: u64,
    /// Lets one manually released job pass a still-active shared cooldown once.
    manual_cooldown_bypass: bool,
}

/// Shared provider-profile lower bound and its visible normalized reason.
#[derive(Clone, Copy, Debug)]
struct SharedCooldown {
    /// Common earliest provider-contact instant before prompt-local jitter.
    not_before: Instant,
    /// Failure class that established the current lower bound.
    class: RetryClass,
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
    sequence: u64,
    job: PromptJob,
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
    fn schedule(&mut self, due: Instant, job: PromptJob) -> Result<(), Box<PromptJob>> {
        if self
            .prompts
            .iter()
            .any(|scheduled| scheduled.job.agent_prompt_id == job.agent_prompt_id)
        {
            return Err(Box::new(job));
        }
        self.sequence = self.sequence.saturating_add(1);
        self.prompts.push(ScheduledPrompt {
            due,
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
    fn extend_cooldown(&mut self, provider: &ProviderName, due: Instant) {
        let mut updated = BinaryHeap::new();
        while let Some(mut scheduled) = self.prompts.pop() {
            if scheduled.job.prompt.model.provider == *provider && scheduled.due < due {
                scheduled.due = cooldown_due_for_job(due, &scheduled.job);
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
        due: Instant,
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
    },
}

struct RetryScheduler {
    commands: SyncSender<SchedulerCommand>,
    delayed_count: Arc<AtomicUsize>,
}

impl RetryScheduler {
    fn start(worker_tx: Sender<WorkerMessage>, worker_waker: ManualRuntimeWaker) -> Self {
        // Bound scheduler admission independently of the parked-job heap. The
        // harness already caps outstanding manual controls, and backpressure
        // here also covers internal schedule/cancel/cooldown producers.
        let (commands, receiver) = mpsc::sync_channel(1_024);
        let delayed_count = Arc::new(AtomicUsize::new(0));
        thread::spawn(move || {
            run_retry_scheduler(receiver, worker_tx, worker_waker);
        });
        Self {
            commands,
            delayed_count,
        }
    }

    fn schedule(&self, job: PromptJob, due: Instant) {
        self.delayed_count.fetch_add(1, AtomicOrdering::Relaxed);
        if self
            .commands
            .send(SchedulerCommand::Schedule {
                due,
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

    fn extend_cooldown(&self, provider: ProviderName, due: Instant) {
        let _ = self
            .commands
            .send(SchedulerCommand::ExtendCooldown { provider, due });
    }

    fn is_empty(&self) -> bool {
        self.delayed_count.load(AtomicOrdering::Relaxed) == 0
    }
}

fn run_retry_scheduler(
    commands: Receiver<SchedulerCommand>,
    worker_tx: Sender<WorkerMessage>,
    worker_waker: ManualRuntimeWaker,
) {
    let mut queue = RetryScheduleQueue::default();
    loop {
        while let Some(job) = queue.pop_due(Instant::now()) {
            if send_worker_message(&worker_tx, &worker_waker, WorkerMessage::RetryDue(job)).is_err()
            {
                return;
            }
        }
        let command = match queue.next_due() {
            Some(next_due) => commands.recv_timeout(
                next_due
                    .checked_duration_since(Instant::now())
                    .unwrap_or(Duration::ZERO),
            ),
            None => commands
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected),
        };
        match command {
            Ok(SchedulerCommand::Schedule { due, job }) => {
                if let Err(duplicate) = queue.schedule(due, *job) {
                    // A duplicated logical APID makes ownership ambiguous. Fail
                    // the logical prompt closed once rather than retaining either
                    // entry and risking two later dispatches.
                    if let Some(original) = queue.cancel(&duplicate.agent_prompt_id).pop() {
                        let _ = send_worker_message(
                            &worker_tx,
                            &worker_waker,
                            WorkerMessage::DelayedCanceled {
                                job: original,
                                delayed_count: 2,
                            },
                        );
                    }
                }
            }
            Ok(SchedulerCommand::Cancel(prompt_id)) => {
                for job in queue.cancel(&prompt_id) {
                    let _ = send_worker_message(
                        &worker_tx,
                        &worker_waker,
                        WorkerMessage::DelayedCanceled {
                            job,
                            delayed_count: 1,
                        },
                    );
                }
            }
            Ok(SchedulerCommand::CancelAll) => {
                for job in queue.cancel_all() {
                    let _ = send_worker_message(
                        &worker_tx,
                        &worker_waker,
                        WorkerMessage::DelayedCanceled {
                            job,
                            delayed_count: 1,
                        },
                    );
                }
            }
            Ok(SchedulerCommand::RetryNow {
                request_id,
                agent_prompt_id,
            }) => {
                let mut matches = queue.cancel(&agent_prompt_id);
                if matches.len() > 1 {
                    if let Some(job) = matches.pop() {
                        let _ = send_worker_message(
                            &worker_tx,
                            &worker_waker,
                            WorkerMessage::DelayedCanceled {
                                job,
                                delayed_count: matches.len() + 1,
                            },
                        );
                    }
                    let _ = send_worker_message(
                        &worker_tx,
                        &worker_waker,
                        WorkerMessage::ManualRetry {
                            job: None,
                            request_id,
                            agent_prompt_id,
                        },
                    );
                    continue;
                }
                let job = matches.pop();
                let _ = send_worker_message(
                    &worker_tx,
                    &worker_waker,
                    WorkerMessage::ManualRetry {
                        job,
                        request_id,
                        agent_prompt_id,
                    },
                );
            }
            Ok(SchedulerCommand::ExtendCooldown { provider, due }) => {
                queue.extend_cooldown(&provider, due);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
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
    },
    /// Marker that one prompt worker finished and freed a concurrency slot.
    PromptDone,
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
    ) -> Self {
        Self {
            target: HarnessInputMessageTarget::Worker {
                tx,
                waker,
                cancel_generation,
                agent_prompt_id,
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
            } => send_worker_message(
                tx,
                waker,
                WorkerMessage::Output {
                    message: Box::new(message),
                    cancel_generation: *cancel_generation,
                    agent_prompt_id: agent_prompt_id.clone(),
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

    fn cancel_retry_sleeps(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.retry_cancel_generation = inner.retry_cancel_generation.saturating_add(1);
            self.changed.notify_all();
        }
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
        waker: Arc<dyn Fn() + Send + Sync + 'static>,
    ) -> CancellationAbortWaker {
        let (id, call_now) = if let Ok(mut inner) = self.inner.lock() {
            let id = inner.next_abort_waker_id;
            inner.next_abort_waker_id = inner.next_abort_waker_id.saturating_add(1);
            let call_now =
                inner.shutdown || inner.canceled_apids.iter().any(|apid| apid == current_apid);
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
        let result = {
            let mut writer = execution.frame_writer();
            let mut retry_ctx = SharedRetryContext {
                cancellation: execution.cancellation.clone(),
                current_apid: agent_prompt_id.clone(),
                cancel_generation: execution.job.cancel_generation,
            };
            handle_prompt_backend(
                &agent_prompt_id,
                &execution.job.backend,
                &execution.job.prompt,
                execution.job.debug_provider_requests,
                &mut writer,
                &mut retry_ctx,
                &execution.chatgpt_runtime,
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

fn start_prompt_job(job: PromptJob, active_prompts: &mut usize, context: &PromptWorkerContext) {
    *active_prompts += 1;
    let execution = PromptExecution {
        job,
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
    handle: &ClientHandle,
) -> ClientResult<()> {
    let delay = due
        .checked_duration_since(Instant::now())
        .unwrap_or(Duration::ZERO);
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
        Box::new(
            self.cancellation
                .register_abort_waker(&self.current_apid, waker),
        )
    }
}

fn resolve_prompt_backend(
    model: &ModelId,
    profiles: &mut BuiltinProviderProfiles,
) -> Option<PromptBackend> {
    match profiles.providers.get_mut(&model.provider)? {
        BuiltinProviderProfile::Chatgpt(profile) => {
            resolve_chatgpt_backend(model, &model.provider, &mut profile.auth)
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
            resolve_chatgpt_backend(model, &model.provider, &mut profile.auth)
        }
        BuiltinProviderProfile::ChatCompletions(_) | BuiltinProviderProfile::OpenRouter(_) => None,
    }
}

fn resolve_chatgpt_backend(
    model: &ModelId,
    provider_name: &ProviderName,
    auth_store: &mut OpenAiAuth,
) -> Option<responses::ResponsesConfig> {
    if oauth_token_should_refresh(&auth_store.access_token, auth_store.expires_at_ms)
        && !auth_store.refresh_token.trim().is_empty()
    {
        match refresh_chatgpt_credentials_locked(provider_name) {
            Ok(refreshed) => {
                *auth_store = refreshed;
            }
            Err(error) => tracing::warn!(
                target: LOG_TARGET,
                "failed to refresh ChatGPT credentials: {error}"
            ),
        }
    }
    if auth_store.access_token.trim().is_empty() {
        return None;
    }

    Some(tau_provider_chatgpt::config_for_model(
        &model.model,
        auth_store.access_token.clone(),
        auth_store.account_id.clone(),
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

        let tokens = tau_provider::oauth::openai_codex_refresh(&current.refresh_token)?;
        let refreshed = OpenAiAuth {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            expires_at_ms: tokens.expires_at_ms,
            account_id: tokens.account_id,
        };
        profile.auth = refreshed.clone();
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

fn handle_prewarm(
    prewarm: &tau_proto::AgentPromptPrewarmRequested,
    profiles: &mut BuiltinProviderProfiles,
    chatgpt_runtime: &ChatGptRuntime,
    session_debug_allowed: &BTreeMap<tau_proto::SessionId, bool>,
) {
    let Some(model) = prewarm.model.as_ref() else {
        tracing::debug!(
            target: LOG_TARGET,
            agent_id = %prewarm.agent_id,
            "skipping prompt prewarm: no selected model",
        );
        return;
    };
    let Some(config) = resolve_responses_backend(model, profiles) else {
        tracing::debug!(
            target: LOG_TARGET,
            agent_id = %prewarm.agent_id,
            model = %model,
            "skipping prompt prewarm: unsupported backend",
        );
        return;
    };
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
        debug_provider_requests: debug_provider_requests_for(
            &prewarm.session_id,
            session_debug_allowed,
        ),
    };
    tracing::debug!(target: LOG_TARGET, session_id = session_id_str, "starting prompt prewarm");
    match chatgpt_runtime.prewarm(&config, session_id_str, &request) {
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
    debug_provider_requests: bool,
    writer: &mut PeerOutputWriter<W>,
    retry_ctx: &mut R,
    chatgpt_runtime: &ChatGptRuntime,
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
            debug_provider_requests,
            writer,
            retry_ctx,
            chatgpt_runtime,
        ),
        PromptBackend::ChatCompletions { provider, model } => {
            let outcome = run_prompt_attempt_for_provider(
                agent_prompt_id,
                prompt,
                provider,
                model,
                debug_provider_requests,
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

fn handle_prompt<R, W: Write>(
    agent_prompt_id: &str,
    config: &responses::ResponsesConfig,
    prompt: &tau_proto::AgentPromptCreated,
    debug_provider_requests: bool,
    writer: &mut PeerOutputWriter<W>,
    retry_ctx: &mut R,
    chatgpt_runtime: &ChatGptRuntime,
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
        debug_provider_requests,
    };

    if prompt.operation == tau_proto::PromptOperation::StandaloneCompaction {
        // This deliberately has no inline fallback; see
        // `DESIGN-tau-ext-provider-builtin-standalone-compaction`.
        match chatgpt_runtime.compact(agent_prompt_id, config, &request, retry_ctx) {
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
                    debug_provider_requests,
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
    let mut on_update = |state: &common::StreamState| {
        response_update_emitter.emit_if_due(
            agent_prompt_id,
            &prompt.agent_id,
            &originator,
            state,
            writer,
        );
    };
    let result = chatgpt_runtime.stream(
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
                debug_provider_requests,
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
                debug_provider_requests,
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
                debug_provider_requests,
                writer,
            )?
        }
    }
    Ok(None)
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
            BuiltinProviderProfile::Chatgpt(_) => {
                models.extend(tau_provider_chatgpt::models_for_provider(provider_name));
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
mod tests;
