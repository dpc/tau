//! Built-in provider registry extension.
//!
//! This crate owns Tau's built-in provider process, profile CLI, auth/profile
//! storage scan, model publication, and dispatch across built-in provider
//! backends. Individual backend crates own provider-specific wire formats.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::error::Error;
use std::io::{BufReader, BufWriter, Read, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use backon::BackoffBuilder;
use dialoguer::Input;
use serde::{Deserialize, Serialize};
use tau_proto::{
    ClientKind, ContextItem, Event, EventDelivery, EventName, HarnessInputMessage,
    HarnessOutputMessage, ModelId, ModelName, PeerInputReader, PeerOutputWriter, ProviderBackend,
    ProviderBackendKind, ProviderBackendTransport, ProviderCacheMissDiagnostic, ProviderModelInfo,
    ProviderModelsUpdated, ProviderName, ProviderPromptSubmitted, ProviderResponseFinished,
    ProviderResponseStatusUpdate, ProviderResponseUpdated, ProviderStopReason,
};
use tau_provider::storage::{AuthFile, ProviderStore};
use tau_provider_chat_completions::openrouter::{OpenRouterProfile, fetch_openrouter_models};
use tau_provider_chat_completions::{
    ChatCompletionsModel, ChatCompletionsProvider, models_for_provider as chat_models_for_provider,
    run_prompt_for_provider as run_chat_completions_prompt,
};
use tau_provider_chatgpt::{ChatGptRuntime, ChatGptTurnState, common, responses};

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

/// Maximum number of retry attempts before giving up on a transient provider
/// error.
const LLM_MAX_RETRIES: usize = 8;

/// Tighter cap for extension-originated turns (delegate sub-agents,
/// notifications, etc.). These are best-effort from the user's perspective, and
/// should not block the provider extension's single prompt slot for minutes.
const LLM_MAX_RETRIES_EXTENSION: usize = 2;
/// Maximum delay accepted from either Tau's retry schedule or provider-supplied
/// `Retry-After` style limits.
///
/// Provider prompts run on a bounded worker pool. Upstream APIs can report
/// account reset windows measured in hours, but sleeping a worker that long is
/// indistinguishable from a stuck provider process from the harness's point of
/// view. Keep retries useful for transient overload while guaranteeing a prompt
/// slot returns in human-scale time.
const LLM_MAX_RETRY_DELAY: Duration = Duration::from_secs(60);

/// Default number of provider prompts allowed to execute concurrently.
const DEFAULT_PROMPT_CONCURRENCY: usize = 4;

/// Environment override for prompt execution concurrency.
const PROMPT_CONCURRENCY_ENV: &str = "TAU_BUILTIN_PROVIDER_PROMPT_CONCURRENCY";
const CANCELED_BY_HARNESS_STATUS: u16 = 499;
const CANCELED_BY_HARNESS_BODY: &str = "cancelled by harness";

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
    tau_extension::init_logging_for(LOG_TARGET);
    run(std::io::stdin(), std::io::stdout())
}

/// Runs the extension over arbitrary reader/writer streams.
///
/// The reader is moved to a background thread so retry-backoff sleeps can wake
/// early when the harness disconnects or sends a targeted cancel.
pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write,
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
    W: Write,
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
    W: Write,
    F: FnMut() -> BuiltinProviderProfiles,
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
    W: Write,
    F: FnMut() -> BuiltinProviderProfiles,
{
    let writer = handshake_provider(writer, &startup_profiles)?;
    let frame_rx = spawn_reader_pump(reader);
    let (worker_tx, worker_rx) = mpsc::channel::<WorkerMessage>();
    ProviderRuntime {
        writer,
        load_prompt_profiles,
        prompt_concurrency_limit,
        prompt_executor,
        frame_rx,
        worker_tx,
        worker_rx,
        chatgpt_runtime: Arc::new(ChatGptRuntime::new()),
        cancellation: Arc::new(CancellationState::default()),
        prompt_queue: VecDeque::new(),
        session_debug_allowed: BTreeMap::new(),
        active_prompts: 0,
        input_closed: false,
    }
    .run()
}

fn handshake_provider<W: Write>(
    writer: W,
    startup_profiles: &BuiltinProviderProfiles,
) -> Result<BufWriter<W>, Box<dyn Error>> {
    let mut handshake_writer = PeerOutputWriter::new(BufWriter::new(writer));

    // No past events requested: provider work starts from fresh live state.
    // Models are announced from current auth below; replaying old prompt,
    // prewarm, or cancel events would rerun or cancel completed turns.
    tau_extension::Handshake::with_kind(EXTENSION_NAME, ClientKind::Provider)
        .subscribe([
            EventName::AGENT_PROMPT_PREWARM_REQUESTED,
            EventName::HARNESS_SESSION_DIR,
            EventName::UI_CANCEL_PROMPT,
        ])
        .announce_event(Event::ProviderModelsUpdated(ProviderModelsUpdated {
            models: models_for_profiles(startup_profiles),
        }))
        .ready_message("builtin provider ready")
        .run(&mut handshake_writer)?;
    Ok(handshake_writer.into_inner())
}

fn spawn_reader_pump<R: Read + Send + 'static>(reader: R) -> Receiver<HarnessOutputMessage> {
    let (frame_tx, frame_rx) = mpsc::channel::<HarnessOutputMessage>();
    thread::spawn(move || {
        let mut reader = PeerInputReader::new(BufReader::new(reader));
        loop {
            match reader.read_message() {
                Ok(Some(frame)) => {
                    if frame_tx.send(frame).is_err() {
                        return;
                    }
                }
                Ok(None) => return,
                Err(error) => {
                    tracing::warn!(target: LOG_TARGET, "reader pump failed: {error}");
                    return;
                }
            }
        }
    });
    frame_rx
}

/// Live provider event loop state after the Tau extension handshake completes.
struct ProviderRuntime<W: Write, F> {
    /// Framed protocol writer back to the harness.
    writer: BufWriter<W>,
    /// Reloads provider profiles for prompt-time auth/model resolution.
    load_prompt_profiles: F,
    /// Maximum number of prompt workers that may run at once.
    prompt_concurrency_limit: usize,
    /// Starts provider backend execution for one prompt job.
    prompt_executor: PromptExecutor,
    /// Frames read by the background input pump.
    frame_rx: Receiver<HarnessOutputMessage>,
    /// Sender used by prompt workers to return frames and completion notices.
    worker_tx: Sender<WorkerMessage>,
    /// Receiver used by the runtime loop to drain worker output.
    worker_rx: Receiver<WorkerMessage>,
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
}

impl<W, F> ProviderRuntime<W, F>
where
    W: Write,
    F: FnMut() -> BuiltinProviderProfiles,
{
    fn run(&mut self) -> Result<(), Box<dyn Error>> {
        loop {
            self.drain_workers_and_start_prompts()?;
            if self.is_finished() {
                return Ok(());
            }
            let Some(frame) = self.next_frame() else {
                continue;
            };
            if self.handle_frame(frame)? == RunDecision::Stop {
                return Ok(());
            }
        }
    }

    fn drain_workers_and_start_prompts(&mut self) -> Result<(), Box<dyn Error>> {
        drain_worker_messages(&self.worker_rx, &mut self.writer, &mut self.active_prompts)?;
        let prompt_worker_context = PromptWorkerContext {
            worker_tx: &self.worker_tx,
            prompt_executor: &self.prompt_executor,
            cancellation: &self.cancellation,
            chatgpt_runtime: &self.chatgpt_runtime,
        };
        start_queued_prompts(
            &mut self.prompt_queue,
            &mut self.active_prompts,
            self.prompt_concurrency_limit,
            &prompt_worker_context,
            &mut self.writer,
        )
    }

    fn is_finished(&self) -> bool {
        self.input_closed && self.active_prompts == 0 && self.prompt_queue.is_empty()
    }

    fn next_frame(&mut self) -> Option<HarnessOutputMessage> {
        if self.input_closed {
            return None;
        }
        if self.active_prompts == 0 && self.prompt_queue.is_empty() {
            self.recv_blocking()
        } else {
            self.recv_while_workers_run()
        }
    }

    fn recv_blocking(&mut self) -> Option<HarnessOutputMessage> {
        match self.frame_rx.recv() {
            Ok(frame) => Some(frame),
            Err(_) => {
                self.input_closed = true;
                None
            }
        }
    }

    fn recv_while_workers_run(&mut self) -> Option<HarnessOutputMessage> {
        match self.frame_rx.recv_timeout(Duration::from_millis(10)) {
            Ok(frame) => Some(frame),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => {
                self.input_closed = true;
                None
            }
        }
    }

    fn handle_frame(&mut self, frame: HarnessOutputMessage) -> Result<RunDecision, Box<dyn Error>> {
        match frame {
            HarnessOutputMessage::Deliver(delivery) => {
                self.process_delivery(delivery)?;
                Ok(RunDecision::Continue)
            }
            HarnessOutputMessage::Disconnect(_) => {
                self.cancellation.shutdown();
                Ok(RunDecision::Stop)
            }
            _ => Ok(RunDecision::Continue),
        }
    }

    fn process_delivery(&mut self, delivery: EventDelivery) -> Result<(), Box<dyn Error>> {
        let is_replay = delivery.is_replay();
        let event = delivery.into_event();
        // Prompt execution is an effect; replay-marked frames re-send history
        // and must never start a provider call. Harness session directory
        // announcements are current-state facts, so replay catch-up is allowed
        // to update the diagnostics policy.
        if is_replay && !matches!(event, Event::HarnessSessionDir(_)) {
            return Ok(());
        }
        self.handle_event(event)?;
        Ok(())
    }

    fn handle_event(&mut self, event: Event) -> Result<(), Box<dyn Error>> {
        match event {
            Event::HarnessSessionDir(session_dir) => self.record_session_debug_policy(session_dir),
            Event::AgentPromptPrewarmRequested(prewarm) => self.prewarm_backend(prewarm),
            Event::AgentPromptCreated(prompt) => self.handle_prompt_created(prompt)?,
            Event::UiCancelPrompt(cancel) => self.handle_cancel_prompt(cancel)?,
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
    ) -> Result<(), Box<dyn Error>> {
        let agent_prompt_id = prompt.agent_prompt_id.clone();
        let prompt = materialize_prompt(&prompt);
        if self.cancellation.take_canceled(&agent_prompt_id) {
            return self.finish_canceled_prompt(&agent_prompt_id, &prompt);
        }
        trace_prompt_like("provider prompt", &prompt, &agent_prompt_id);
        self.start_or_reject_prompt(agent_prompt_id, prompt)
    }

    fn finish_canceled_prompt(
        &mut self,
        agent_prompt_id: &tau_proto::AgentPromptId,
        prompt: &tau_proto::AgentPromptCreated,
    ) -> Result<(), Box<dyn Error>> {
        let mut frame_writer = PeerOutputWriter::new(&mut self.writer);
        finish_canceled(agent_prompt_id, prompt, &mut frame_writer)
    }

    fn start_or_reject_prompt(
        &mut self,
        agent_prompt_id: tau_proto::AgentPromptId,
        prompt: tau_proto::AgentPromptCreated,
    ) -> Result<(), Box<dyn Error>> {
        let mut profiles = (self.load_prompt_profiles)();
        let Some(backend) = resolve_prompt_backend(&prompt.model, &mut profiles) else {
            return self.finish_prompt_with_missing_backend(&prompt, &agent_prompt_id);
        };
        self.enqueue_or_start_prompt(PromptJob {
            agent_prompt_id,
            debug_provider_requests: debug_provider_requests_for(
                &prompt.session_id,
                &self.session_debug_allowed,
            ),
            prompt,
            backend,
        });
        Ok(())
    }

    fn finish_prompt_with_missing_backend(
        &mut self,
        prompt: &tau_proto::AgentPromptCreated,
        agent_prompt_id: &tau_proto::AgentPromptId,
    ) -> Result<(), Box<dyn Error>> {
        let mut frame_writer = PeerOutputWriter::new(&mut self.writer);
        write_prompt_submitted(agent_prompt_id, &prompt.originator, &mut frame_writer)?;
        finish_missing_backend(prompt, agent_prompt_id, &mut frame_writer)
    }

    fn enqueue_or_start_prompt(&mut self, job: PromptJob) {
        if self.active_prompts >= self.prompt_concurrency_limit {
            self.prompt_queue.push_back(job);
            return;
        }
        let prompt_worker_context = PromptWorkerContext {
            worker_tx: &self.worker_tx,
            prompt_executor: &self.prompt_executor,
            cancellation: &self.cancellation,
            chatgpt_runtime: &self.chatgpt_runtime,
        };
        start_prompt_job(job, &mut self.active_prompts, &prompt_worker_context);
    }

    fn handle_cancel_prompt(
        &mut self,
        cancel: tau_proto::UiCancelPrompt,
    ) -> Result<(), Box<dyn Error>> {
        let Some(apid) = cancel.agent_prompt_id else {
            self.cancellation.cancel_retry_sleeps();
            return Ok(());
        };
        self.cancellation.cancel(apid.clone());
        finish_queued_canceled(&apid, &mut self.prompt_queue, &mut self.writer)
    }
}

/// Control-flow result for handling one harness frame.
#[derive(Clone, Copy, Eq, PartialEq)]
enum RunDecision {
    /// Continue processing frames and worker output.
    Continue,
    /// Stop the provider runtime cleanly.
    Stop,
}

type PromptExecutor = Arc<dyn Fn(PromptExecution) + Send + Sync + 'static>;

struct PromptJob {
    agent_prompt_id: tau_proto::AgentPromptId,
    debug_provider_requests: bool,
    prompt: tau_proto::AgentPromptCreated,
    backend: PromptBackend,
}

#[derive(Clone)]
enum PromptBackend {
    Responses(responses::ResponsesConfig),
    ChatCompletions {
        provider: ChatCompletionsProvider,
        model: ChatCompletionsModel,
    },
}

struct PromptExecution {
    job: PromptJob,
    output_tx: Sender<WorkerMessage>,
    cancellation: Arc<CancellationState>,
    chatgpt_runtime: Arc<ChatGptRuntime>,
}

struct PromptWorkerContext<'a> {
    worker_tx: &'a Sender<WorkerMessage>,
    prompt_executor: &'a PromptExecutor,
    cancellation: &'a Arc<CancellationState>,
    chatgpt_runtime: &'a Arc<ChatGptRuntime>,
}

impl PromptExecution {
    fn frame_writer(&self) -> PeerOutputWriter<BufWriter<ChannelWrite>> {
        PeerOutputWriter::new(BufWriter::new(ChannelWrite::new(self.output_tx.clone())))
    }
}

enum WorkerMessage {
    Output(Vec<u8>),
    PromptDone,
}

struct ChannelWrite {
    tx: Sender<WorkerMessage>,
    buf: Vec<u8>,
}

impl ChannelWrite {
    fn new(tx: Sender<WorkerMessage>) -> Self {
        Self {
            tx,
            buf: Vec::new(),
        }
    }
}

impl Write for ChannelWrite {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let bytes = std::mem::take(&mut self.buf);
        self.tx
            .send(WorkerMessage::Output(bytes))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "writer closed"))
    }
}

#[derive(Default)]
struct CancellationState {
    inner: Mutex<CancellationInner>,
    changed: Condvar,
}

#[derive(Default)]
struct CancellationInner {
    canceled_apids: HashSet<tau_proto::AgentPromptId>,
    retry_cancel_generation: u64,
    shutdown: bool,
}

impl CancellationState {
    fn cancel(&self, apid: tau_proto::AgentPromptId) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.canceled_apids.insert(apid);
            self.changed.notify_all();
        }
    }

    fn cancel_retry_sleeps(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.retry_cancel_generation = inner.retry_cancel_generation.saturating_add(1);
            self.changed.notify_all();
        }
    }

    fn shutdown(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.shutdown = true;
            self.changed.notify_all();
        }
    }

    fn take_canceled(&self, apid: &tau_proto::AgentPromptId) -> bool {
        self.inner
            .lock()
            .map(|mut inner| inner.canceled_apids.remove(apid) || inner.shutdown)
            .unwrap_or(true)
    }

    fn sleep_or_abort(&self, delay: Duration, current_apid: &str) -> SleepOutcome {
        let Some(deadline) = Instant::now().checked_add(delay) else {
            return SleepOutcome::Aborted;
        };
        let mut inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return SleepOutcome::Aborted,
        };
        let generation = inner.retry_cancel_generation;
        loop {
            if inner.shutdown
                || inner.retry_cancel_generation != generation
                || inner
                    .canceled_apids
                    .iter()
                    .any(|apid| apid.as_str() == current_apid)
            {
                return SleepOutcome::Aborted;
            }
            let now = Instant::now();
            let Some(remaining) = deadline.checked_duration_since(now) else {
                return SleepOutcome::Elapsed;
            };
            match self.changed.wait_timeout(inner, remaining) {
                Ok((guard, result)) => {
                    inner = guard;
                    if result.timed_out() {
                        return SleepOutcome::Elapsed;
                    }
                }
                Err(_) => return SleepOutcome::Aborted,
            }
        }
    }
}

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
        let mut writer = execution.frame_writer();
        let mut retry_ctx = SharedRetryContext {
            cancellation: execution.cancellation.clone(),
        };
        let result = handle_prompt_backend(
            &agent_prompt_id,
            &execution.job.backend,
            &execution.job.prompt,
            execution.job.debug_provider_requests,
            &mut writer,
            &mut retry_ctx,
            &execution.chatgpt_runtime,
        );
        if let Err(error) = result {
            tracing::warn!(
                target: LOG_TARGET,
                agent_prompt_id = %agent_prompt_id,
                "prompt worker failed to emit provider response: {error}"
            );
        }
    })
}

fn start_prompt_job(job: PromptJob, active_prompts: &mut usize, context: &PromptWorkerContext<'_>) {
    *active_prompts += 1;
    let execution = PromptExecution {
        job,
        output_tx: context.worker_tx.clone(),
        cancellation: context.cancellation.clone(),
        chatgpt_runtime: context.chatgpt_runtime.clone(),
    };
    let executor = context.prompt_executor.clone();
    let done_tx = context.worker_tx.clone();
    thread::spawn(move || {
        executor(execution);
        let _ = done_tx.send(WorkerMessage::PromptDone);
    });
}

fn start_queued_prompts<W: Write>(
    prompt_queue: &mut VecDeque<PromptJob>,
    active_prompts: &mut usize,
    prompt_concurrency_limit: usize,
    context: &PromptWorkerContext<'_>,
    writer: &mut BufWriter<W>,
) -> Result<(), Box<dyn Error>> {
    while *active_prompts < prompt_concurrency_limit {
        let Some(job) = prompt_queue.pop_front() else {
            return Ok(());
        };
        if context.cancellation.take_canceled(&job.agent_prompt_id) {
            let mut frame_writer = PeerOutputWriter::new(&mut *writer);
            finish_canceled(&job.agent_prompt_id, &job.prompt, &mut frame_writer)?;
            continue;
        }
        start_prompt_job(job, active_prompts, context);
    }
    Ok(())
}

fn finish_queued_canceled<W: Write>(
    apid: &tau_proto::AgentPromptId,
    prompt_queue: &mut VecDeque<PromptJob>,
    writer: &mut BufWriter<W>,
) -> Result<(), Box<dyn Error>> {
    let Some(index) = prompt_queue
        .iter()
        .position(|job| job.agent_prompt_id.as_str() == apid.as_str())
    else {
        return Ok(());
    };
    let Some(job) = prompt_queue.remove(index) else {
        return Ok(());
    };
    let mut frame_writer = PeerOutputWriter::new(writer);
    finish_canceled(&job.agent_prompt_id, &job.prompt, &mut frame_writer)?;
    Ok(())
}

fn drain_worker_messages<W: Write>(
    worker_rx: &Receiver<WorkerMessage>,
    writer: &mut BufWriter<W>,
    active_prompts: &mut usize,
) -> Result<(), Box<dyn Error>> {
    loop {
        match worker_rx.try_recv() {
            Ok(WorkerMessage::Output(bytes)) => {
                writer.write_all(&bytes)?;
                writer.flush()?;
            }
            Ok(WorkerMessage::PromptDone) => {
                *active_prompts = active_prompts.saturating_sub(1);
            }
            Err(TryRecvError::Empty) => return Ok(()),
            Err(TryRecvError::Disconnected) => return Ok(()),
        }
    }
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

fn finish_missing_backend<W: Write>(
    prompt: &tau_proto::AgentPromptCreated,
    agent_prompt_id: &str,
    writer: &mut PeerOutputWriter<W>,
) -> Result<(), Box<dyn Error>> {
    let msg = format!("cannot resolve provider backend for: {}", prompt.model);
    writer.write_message(&HarnessInputMessage::emit(Event::ProviderResponseFinished(
        simple_finished(
            agent_prompt_id.into(),
            prompt.agent_id.clone(),
            prompt.originator.clone(),
            msg,
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

trait RetrySleeper {
    fn sleep_or_abort(&mut self, delay: Duration, current_apid: &str) -> SleepOutcome;

    fn is_aborted(&mut self, current_apid: &str) -> bool {
        matches!(
            self.sleep_or_abort(Duration::ZERO, current_apid),
            SleepOutcome::Aborted,
        )
    }
}

struct SharedRetryContext {
    cancellation: Arc<CancellationState>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SleepOutcome {
    Elapsed,
    Aborted,
}

impl RetrySleeper for SharedRetryContext {
    fn sleep_or_abort(&mut self, delay: Duration, current_apid: &str) -> SleepOutcome {
        // Prompt workers do not own the blocking network request, so targeted
        // cancel cannot preempt an in-flight HTTP/WS read yet. It still aborts
        // retry backoff sleeps and keeps queued prompts from starting, matching
        // the existing provider's retry-abort safety without collateral-canceling
        // unrelated prompt ids.
        self.cancellation.sleep_or_abort(delay, current_apid)
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

fn max_retries_for(originator: &tau_proto::PromptOriginator) -> usize {
    match originator {
        tau_proto::PromptOriginator::User => LLM_MAX_RETRIES,
        tau_proto::PromptOriginator::Extension { .. } => LLM_MAX_RETRIES_EXTENSION,
    }
}

fn llm_retry_schedule(max_attempts: usize) -> backon::FibonacciBackoff {
    backon::FibonacciBuilder::default()
        .with_min_delay(Duration::from_secs(10))
        .with_max_times(max_attempts)
        .with_jitter()
        .build()
}

fn with_llm_retry<F, R, W: Write, T>(
    agent_prompt_id: &str,
    agent_id: &tau_proto::AgentId,
    originator: &tau_proto::PromptOriginator,
    writer: &mut PeerOutputWriter<W>,
    retry_ctx: &mut R,
    mut call: F,
) -> Result<T, common::LlmError>
where
    F: FnMut(&mut PeerOutputWriter<W>, &mut R) -> Result<T, common::LlmError>,
    R: RetrySleeper,
{
    let max_attempts = max_retries_for(originator);
    let mut backoff = llm_retry_schedule(max_attempts);
    let mut attempt = 0_usize;
    loop {
        let error = match call(writer, retry_ctx) {
            Ok(state) => return Ok(state),
            Err(error) => error,
        };
        let Some(retry_after) = error.retry_after() else {
            return Err(error);
        };
        let Some(backoff_delay) = backoff.next() else {
            return Err(error);
        };
        let delay = retry_after.max(backoff_delay).min(LLM_MAX_RETRY_DELAY);
        attempt += 1;
        tracing::warn!(
            target: LOG_TARGET,
            agent_prompt_id,
            "provider error, retrying in {delay:?} (attempt {attempt}/{max_attempts}): {error}",
        );
        emit_retry_banner(
            agent_prompt_id,
            agent_id,
            originator,
            writer,
            &error,
            delay,
            attempt,
        );
        if matches!(
            retry_ctx.sleep_or_abort(delay, agent_prompt_id),
            SleepOutcome::Aborted,
        ) {
            tracing::info!(
                target: LOG_TARGET,
                agent_prompt_id,
                "retry aborted by disconnect/cancel",
            );
            return Err(error);
        }
    }
}

fn emit_retry_banner<W: Write>(
    agent_prompt_id: &str,
    agent_id: &tau_proto::AgentId,
    originator: &tau_proto::PromptOriginator,
    writer: &mut PeerOutputWriter<W>,
    error: &common::LlmError,
    delay: Duration,
    attempt: usize,
) {
    let max_attempts = max_retries_for(originator);
    let banner = format!(
        "provider error — retrying in {}s (attempt {}/{})\n\n> {}",
        delay.as_secs(),
        attempt,
        max_attempts,
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
            }),
            originator: originator.clone(),
        },
    )));
    let _ = writer.flush();
}

fn is_canceled_by_harness(error: &common::LlmError) -> bool {
    matches!(
        error,
        common::LlmError::HttpStatus(CANCELED_BY_HARNESS_STATUS, body)
            if body == CANCELED_BY_HARNESS_BODY
    )
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
) -> Result<(), Box<dyn Error>>
where
    R: RetrySleeper,
{
    match backend {
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
            write_prompt_submitted(agent_prompt_id, &prompt.originator, writer)?;
            let finished = run_chat_completions_prompt(
                agent_prompt_id,
                prompt,
                provider,
                model,
                debug_provider_requests,
                writer,
            );
            writer.write_message(&HarnessInputMessage::emit(Event::ProviderResponseFinished(
                finished,
            )))?;
            writer.flush()?;
            Ok(())
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
) -> Result<(), Box<dyn Error>>
where
    R: RetrySleeper,
{
    write_prompt_submitted(agent_prompt_id, &prompt.originator, writer)?;
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

    let originator = prompt.originator.clone();
    let mut chatgpt_turn_state = ChatGptTurnState::new(max_retries_for(&originator));
    let mut transport_taken = ProviderBackendTransport::HttpSse;
    let mut ws_pool_delta = None;
    let result = with_llm_retry(
        agent_prompt_id,
        &prompt.agent_id,
        &originator,
        writer,
        retry_ctx,
        |writer, retry_ctx| {
            let mut delta_emitter = common::StreamDeltaEmitter::default();
            let mut on_update = |state: &common::StreamState| {
                let deltas = delta_emitter.deltas(state);
                let compaction = state.compaction_update();
                if deltas.is_empty() && compaction.is_none() {
                    return;
                }
                let _ = writer.write_message(&HarnessInputMessage::emit(
                    Event::ProviderResponseUpdated(ProviderResponseUpdated {
                        agent_prompt_id: agent_prompt_id.into(),
                        agent_id: prompt.agent_id.clone(),
                        deltas,
                        compaction,
                        status: None,
                        originator: originator.clone(),
                    }),
                ));
                let _ = writer.flush();
            };
            chatgpt_runtime.stream(
                agent_prompt_id,
                config,
                &request,
                &mut chatgpt_turn_state,
                &mut || retry_ctx.is_aborted(agent_prompt_id),
                &mut on_update,
            )
        },
    );
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
    Ok(())
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
            }),
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
