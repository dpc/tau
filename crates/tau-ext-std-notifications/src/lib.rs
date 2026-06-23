//! Bridge provider prompt-start / response-finish events into iTerm2-style
//! OSC 1337 `SetUserVar` notifications, mirroring the dpc-personal
//! `notification-sounds.ts` and `user-text-notification.sh` Pi
//! extensions.
//!
//! Tau's built-in config disables all hooks. Users can configure hook actions
//! for:
//! - `agent.prompt_submitted`
//! - final `provider.response_finished` (only when `stop_reason` does not
//!   request tools and no backgrounded main-agent tools remain active)
//! - idle deadlines after a final response
//!
//! The per-agent idle timer resets on every user-originated
//! `agent.prompt_submitted` / `provider.prompt_submitted`; pending idle hooks
//! that are still waiting for their idle deadline are extended by
//! `ui.prompt_draft` typing pings.
//!
//! The downstream tooling (typically a terminal multiplexer status
//! line or a `user-notification.sh` consumer wired to a sound file)
//! is what actually plays the sounds / pops the desktop notification;
//! this extension just publishes the user-var change so a UI further
//! up the stack can forward it to the terminal.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::io::{BufReader, BufWriter, Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use tau_proto::{
    ConfigError, Event, HarnessInputMessage, HarnessOutputMessage, Osc1337SetUserVar,
    PeerInputReader, PeerOutputWriter, StartAgentRequest, TermBell,
};

/// `tracing` target for events emitted from this extension. Matches
/// the convention described in [`tau_extension`]: a short identifier
/// the user can name in `TAU_LOG=std-notifications=trace`.
pub const LOG_TARGET: &str = "std-notifications";

/// User-var name for sound notifications (matches `user-notification.sh`).
pub const SOUND_VAR_NAME: &str = "user-notification";

/// User-var name for text/desktop notifications (matches
/// `user-text-notification.sh`).
pub const TEXT_VAR_NAME: &str = "user-text-notification";

/// Sound key emitted when the user submits a prompt.
pub const VALUE_AGENT_START: &str = "protoss-probe-ack";

/// Sound key emitted at the end of an agent turn.
pub const VALUE_AGENT_END: &str = "protoss-upgrade-complete";

/// Default idle window before the extension nudges the user via a
/// text notification, in seconds. Override with an `agent_idle` hook's
/// `delay_seconds` field in `harness.yaml`.
pub const DEFAULT_IDLE_SECONDS: u64 = 60;

/// How long to wait for the agent to summarize the conversation
/// before falling back to the static idle text. Once the idle window
/// has elapsed we want to actually notify the user soon, even if the
/// provider is wedged or the model is unreachable.
pub const SUMMARY_TIMEOUT_SECONDS: u64 = 10;

/// Maximum OSC 1337 user-variable name length accepted by this extension.
///
/// The terminal UI passes names through verbatim into an escape sequence, so
/// notification keys are intentionally short and restricted.
const MAX_OSC1337_NAME_LEN: usize = 128;

/// Instruction sent to the agent as a side prompt when the idle
/// timer fires. Mirrors the prompt Pi's `idle-notification.ts` uses,
/// adapted for our harness-mediated query path.
const SUMMARY_INSTRUCTION: &str = "Summarize in one short sentence: what \
is the last thing you did or what do you need from the user now? Keep it \
under 200 characters. Output only the summary, nothing else.";

/// Maximum captured user prompt or assistant response bytes copied into the
/// side-query instruction for idle summaries.
const SUMMARY_CONTEXT_LIMIT_BYTES: usize = 4 * 1024;

/// Maximum summary text bytes exposed as `turn.agent_summary`.
const SUMMARY_TEXT_LIMIT_BYTES: usize = 1024;

/// Returns the system hostname via `gethostname(2)`. Falls back to
/// `"host"` if the syscall fails or the bytes aren't UTF-8.
fn hostname() -> String {
    let mut buf = [0_u8; 256];
    #[allow(unsafe_code)]
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast::<libc::c_char>(), buf.len()) };
    if rc != 0 {
        return "host".to_owned();
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    std::str::from_utf8(&buf[..len])
        .ok()
        .map(str::to_owned)
        .unwrap_or_else(|| "host".to_owned())
}

fn cwd_parts() -> (String, String) {
    let cwd = std::env::current_dir().unwrap_or_default();
    let cwd_short = cwd
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| cwd.to_string_lossy().into_owned());
    let cwd_short = if cwd_short.is_empty() {
        cwd.to_string_lossy().into_owned()
    } else {
        cwd_short
    };
    (cwd.to_string_lossy().into_owned(), cwd_short)
}

fn template_context<'a>(
    hook: &'a str,
    agent_id: &'a tau_proto::AgentId,
    agent_name: &'a str,
    user_prompt: &'a str,
    agent_response: &'a str,
    agent_summary: &'a str,
) -> TemplateContext<'a> {
    let host = hostname();
    let (cwd, cwd_basename) = cwd_parts();
    TemplateContext {
        hook,
        agent: AgentTemplateContext {
            id: agent_id.as_ref(),
            name: agent_name,
        },
        host,
        cwd,
        cwd_basename,
        turn: TurnTemplateContext {
            user_prompt,
            agent_response,
            agent_summary,
        },
    }
}

/// Runtime template context available to all configured hook actions.
#[derive(serde::Serialize)]
struct TemplateContext<'a> {
    /// Name of the hook currently being rendered, e.g. `agent_start`.
    hook: &'a str,
    /// Agent identity and display-name fields for the triggering agent.
    agent: AgentTemplateContext<'a>,
    /// Hostname of the machine running the extension process.
    host: String,
    /// Current working directory of the extension process.
    cwd: String,
    /// Basename of [`TemplateContext::cwd`] for compact notification titles.
    cwd_basename: String,
    /// Last visible user/assistant turn text for turn-aware templates.
    turn: TurnTemplateContext<'a>,
}

/// Agent fields exposed to notification hook templates.
#[derive(serde::Serialize)]
struct AgentTemplateContext<'a> {
    /// Stable agent id, exposed as `agent.id`.
    id: &'a str,
    /// Durable display name, or the id fallback, exposed as `agent.name`.
    name: &'a str,
}

/// Last known turn text exposed to notification hook templates.
#[derive(serde::Serialize)]
struct TurnTemplateContext<'a> {
    /// Last user prompt text, exposed as `turn.user_prompt`.
    user_prompt: &'a str,
    /// Last final assistant response text, exposed as `turn.agent_response`.
    agent_response: &'a str,
    /// Optional idle-summary response text, exposed as `turn.agent_summary`.
    agent_summary: &'a str,
}

/// Phase of a single configured idle hook in the idle-watch state machine.
enum IdleState {
    WaitingIdle { deadline: Instant },
    WaitingSummary { query_id: String, deadline: Instant },
}

impl IdleState {
    fn deadline(&self) -> Instant {
        match self {
            Self::WaitingIdle { deadline } | Self::WaitingSummary { deadline, .. } => *deadline,
        }
    }
}

/// Configured idle-hook collection a pending timer belongs to.
#[derive(Clone, Copy)]
enum IdleHookKind {
    Agent,
    AgentAll,
}

/// Pending runtime state for one configured idle hook.
struct PendingIdleHook {
    /// Which configured idle hook list owns this timer.
    hook_kind: IdleHookKind,
    /// Index into the owning hook list.
    hook_index: usize,
    /// Agent whose completed work supplies template context.
    agent_id: tau_proto::AgentId,
    /// Session that owns an `agent_idle_all` timer; absent for `agent_idle`.
    session_id: Option<tau_proto::SessionId>,
    /// Last user prompt text rendered into idle templates.
    user_prompt: String,
    /// Last assistant response text rendered into idle templates.
    agent_response: String,
    /// Current state-machine phase for this timer.
    state: IdleState,
}

/// Last turn text used when rendering an all-idle hook.
#[derive(Clone, Default)]
struct AllIdleTurnContext {
    /// Last user prompt seen for this agent.
    user_prompt: String,
    /// Last final assistant response seen for this agent.
    agent_response: String,
}
/// Per-session state used to detect all-agents-idle transitions.
#[derive(Default)]
struct SessionIdleTracker {
    /// Loaded agents by session id.
    session_agents: HashMap<tau_proto::SessionId, HashSet<tau_proto::AgentId>>,
    /// Reverse index from loaded agent id to containing sessions.
    agent_sessions: HashMap<tau_proto::AgentId, HashSet<tau_proto::SessionId>>,
    /// Agents currently reported running by harness-owned agent state.
    busy_agents: HashSet<tau_proto::AgentId>,
}

impl SessionIdleTracker {
    fn load_agent(&mut self, session_id: tau_proto::SessionId, agent_id: tau_proto::AgentId) {
        self.session_agents
            .entry(session_id.clone())
            .or_default()
            .insert(agent_id.clone());
        self.agent_sessions
            .entry(agent_id)
            .or_default()
            .insert(session_id);
    }

    fn unload_agent(
        &mut self,
        session_id: &tau_proto::SessionId,
        agent_id: &tau_proto::AgentId,
    ) -> Option<tau_proto::SessionId> {
        let was_busy = self.busy_agents.remove(agent_id);
        let mut session_is_idle = false;
        if let Some(agents) = self.session_agents.get_mut(session_id) {
            agents.remove(agent_id);
            session_is_idle = !agents.is_empty() && agents.is_disjoint(&self.busy_agents);
            if agents.is_empty() {
                self.session_agents.remove(session_id);
            }
        }
        if let Some(sessions) = self.agent_sessions.get_mut(agent_id) {
            sessions.remove(session_id);
            if sessions.is_empty() {
                self.agent_sessions.remove(agent_id);
            }
        }
        (was_busy && session_is_idle).then(|| session_id.clone())
    }

    fn mark_busy(&mut self, agent_id: tau_proto::AgentId) {
        self.busy_agents.insert(agent_id);
    }

    fn mark_idle(&mut self, agent_id: &tau_proto::AgentId) -> Vec<tau_proto::SessionId> {
        let was_busy = self.busy_agents.remove(agent_id);
        if was_busy {
            self.idle_sessions_for_agent(agent_id)
        } else {
            Vec::new()
        }
    }

    fn sessions_for_agent(&self, agent_id: &tau_proto::AgentId) -> Vec<tau_proto::SessionId> {
        self.agent_sessions
            .get(agent_id)
            .map(|sessions| sessions.iter().cloned().collect())
            .unwrap_or_default()
    }

    fn idle_sessions_for_agent(&self, agent_id: &tau_proto::AgentId) -> Vec<tau_proto::SessionId> {
        self.sessions_for_agent(agent_id)
            .into_iter()
            .filter(|session_id| {
                self.session_agents.get(session_id).is_some_and(|agents| {
                    !agents.is_empty() && agents.is_disjoint(&self.busy_agents)
                })
            })
            .collect()
    }
}
fn display_name_for_agent(
    display_names: &HashMap<tau_proto::AgentId, String>,
    agent_id: &tau_proto::AgentId,
) -> String {
    display_names
        .get(agent_id)
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| agent_id.to_string())
}

fn response_text(items: &[tau_proto::ContextItem]) -> String {
    let mut out = String::new();
    for item in items {
        let tau_proto::ContextItem::Message(message) = item else {
            continue;
        };
        if message.role != tau_proto::ContextRole::Assistant {
            continue;
        }
        for part in &message.content {
            let tau_proto::ContentPart::Text { text } = part;
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    out
}

impl PendingIdleHook {
    fn deadline(&self) -> Instant {
        self.state.deadline()
    }
}

/// User-supplied configuration for this extension. See the crate's
/// `README.md` for the full schema and worked examples.
#[derive(serde::Deserialize, Debug, Clone, Default)]
#[serde(default, deny_unknown_fields, rename_all = "snake_case")]
struct ExtConfig {
    /// Actions to run when a user-authored prompt starts a main-agent turn.
    agent_start: Vec<HookConfig>,
    /// Actions to run when the main-agent turn reaches its final response.
    agent_end: Vec<HookConfig>,
    /// Actions to run after one agent remains idle past a configured delay.
    agent_idle: Vec<IdleHookConfig>,
    /// Actions to run after every loaded agent in a session is idle.
    agent_idle_all: Vec<IdleHookConfig>,
}

impl ExtConfig {
    fn validate(&self) -> Result<(), String> {
        validate_hooks("agent_start", &self.agent_start)?;
        validate_hooks("agent_end", &self.agent_end)?;
        for idle in &self.agent_idle {
            validate_hook("agent_idle", &idle.hook)?;
        }
        for idle in &self.agent_idle_all {
            validate_hook("agent_idle_all", &idle.hook)?;
        }
        Ok(())
    }
}

/// One notification action run by a hook.
#[derive(serde::Deserialize, Debug, Default, Clone)]
#[serde(default, deny_unknown_fields)]
struct HookConfig {
    /// Emit a terminal bell when this action runs.
    bell: bool,
    /// Optional command argv. Every argv element is rendered as a Handlebars
    /// template.
    command: Option<Vec<String>>,
    /// Optional OSC 1337 SetUserVar action. Both key and value are templates.
    osc1337: Option<Osc1337Config>,
}

/// OSC 1337 SetUserVar action templates.
#[derive(serde::Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
struct Osc1337Config {
    /// User-var key template.
    key: String,
    /// User-var value template.
    value: String,
}

/// One `agent_idle` hook with idle-specific settings.
#[derive(serde::Deserialize, Debug, Clone, Default)]
#[serde(default, deny_unknown_fields)]
struct IdleHookConfig {
    /// Base action fields for this idle hook.
    #[serde(flatten)]
    hook: HookConfig,
    /// Idle delay, in seconds, before this hook fires.
    delay_seconds: Option<u64>,
    /// Whether this idle hook first asks the agent for a one-sentence summary.
    agent_summary: bool,
}
impl IdleHookConfig {
    fn delay_duration(&self, default_delay: Duration) -> Duration {
        self.delay_seconds
            .map(Duration::from_secs)
            .unwrap_or(default_delay)
    }
}
/// Run the extension against process standard input and output.
///
/// This is the production entry point used by the crate binary. It initializes
/// logging with the extension's tracing target before entering the protocol
/// loop.
///
/// # Errors
///
/// Returns protocol I/O, handshake, event encoding, or hook template-rendering
/// failures that prevent the extension from continuing. Configuration parse and
/// validation failures are reported to the harness as `ConfigError` frames and
/// are not returned from this function.
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    tau_extension::init_logging_for(LOG_TARGET);
    run(std::io::stdin(), std::io::stdout())
}

/// Run the extension protocol over arbitrary reader and writer streams.
///
/// This uses [`DEFAULT_IDLE_SECONDS`] as the default idle delay for configured
/// idle hooks that omit `delay_seconds`.
///
/// # Errors
///
/// Returns protocol I/O, handshake, event encoding, or hook template-rendering
/// failures that prevent the extension from continuing. Configuration parse and
/// validation failures are reported to the harness as `ConfigError` frames and
/// are not returned from this function.
pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write,
{
    run_with_idle(reader, writer, Duration::from_secs(DEFAULT_IDLE_SECONDS))
}

/// Inbound message on the main thread's channel: either a decoded harness
/// output message from the reader thread, or a terminal condition that ends the
/// loop.
enum InMsg {
    Message(Box<HarnessOutputMessage>),
    EndOfStream,
}

/// Test-friendly entry point. Lets unit tests drop the idle window
/// to a few hundred milliseconds so the timeout path is observable
/// without slowing the suite. Uses [`SUMMARY_TIMEOUT_SECONDS`] for
/// the summary fallback timer; tests that exercise the fallback path
/// directly should call [`run_with_idle_and_summary_timeout`] with a
/// shorter summary timeout instead.
///
/// # Errors
///
/// Returns protocol I/O, handshake, event encoding, or hook template-rendering
/// failures that prevent the extension from continuing. Configuration parse and
/// validation failures are reported to the harness as `ConfigError` frames and
/// are not returned from this function.
pub fn run_with_idle<R, W>(
    reader: R,
    writer: W,
    idle_duration: Duration,
) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write,
{
    run_with_idle_and_summary_timeout(
        reader,
        writer,
        idle_duration,
        Duration::from_secs(SUMMARY_TIMEOUT_SECONDS),
    )
}

/// Test-friendly entry point with an overridable summary fallback
/// timeout. Useful for exercising the wedged-agent path without
/// blocking the test suite for [`SUMMARY_TIMEOUT_SECONDS`] seconds.
///
/// # Errors
///
/// Returns protocol I/O, handshake, event encoding, or hook template-rendering
/// failures that prevent the extension from continuing. Configuration parse and
/// validation failures are reported to the harness as `ConfigError` frames and
/// are not returned from this function.
pub fn run_with_idle_and_summary_timeout<R, W>(
    reader: R,
    writer: W,
    idle_duration: Duration,
    summary_timeout: Duration,
) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write,
{
    let mut writer = PeerOutputWriter::new(BufWriter::new(writer));
    write_handshake(&mut writer)?;

    let rx = spawn_reader_thread(reader);
    NotificationLoop::new(writer, rx).run(idle_duration, summary_timeout)
}

fn write_handshake<W: Write>(
    writer: &mut PeerOutputWriter<BufWriter<W>>,
) -> Result<(), Box<dyn Error>> {
    // Subscribe-time catch-up delivers prior prompts/results as replay-marked
    // frames; the receive loop skips those so sounds and idle nudges only
    // fire for live activity.
    tau_extension::Handshake::tool("tau-ext-std-notifications")
        .subscribe([
            tau_proto::EventName::PROVIDER_PROMPT_SUBMITTED,
            tau_proto::EventName::PROVIDER_RESPONSE_FINISHED,
            tau_proto::EventName::AGENT_PROMPT_SUBMITTED,
            tau_proto::EventName::AGENT_STARTED,
            tau_proto::EventName::AGENT_DISPLAY_NAME_SET,
            tau_proto::EventName::AGENT_STATE,
            tau_proto::EventName::SESSION_AGENT_LOADED,
            tau_proto::EventName::SESSION_AGENT_UNLOADED,
            tau_proto::EventName::AGENT_START_ACCEPTED,
            // Trailing-edge debounced typing pings from the UI bump the idle
            // deadline so the desktop notification doesn't fire mid-sentence.
            tau_proto::EventName::UI_PROMPT_DRAFT,
            tau_proto::EventName::TOOL_RESULT,
            tau_proto::EventName::TOOL_BACKGROUND_RESULT,
            tau_proto::EventName::TOOL_BACKGROUND_ERROR,
            // Side-query results come back point-to-point from the harness, but
            // subscribe defensively in case the broadcast form ever appears.
            tau_proto::EventName::AGENT_START_RESULT,
        ])
        .ready_message("std-notifications ready")
        .run(writer)?;
    Ok(())
}

fn spawn_reader_thread<R>(reader: R) -> mpsc::Receiver<InMsg>
where
    R: Read + Send + 'static,
{
    // Spawn a reader thread so the main loop can wait on either an incoming
    // message or an idle deadline via `recv_timeout`. The reader exits
    // naturally when stdin closes, then the channel disconnects and the main
    // loop sees EndOfStream.
    let (tx, rx) = mpsc::channel::<InMsg>();
    let _reader_handle = thread::spawn(move || {
        let mut reader = PeerInputReader::new(BufReader::new(reader));
        loop {
            match reader.read_message() {
                Ok(Some(message)) => {
                    if tx.send(InMsg::Message(Box::new(message))).is_err() {
                        break;
                    }
                }
                Ok(None) => {
                    let _ = tx.send(InMsg::EndOfStream);
                    break;
                }
                Err(_) => {
                    // Treat decode errors as end-of-stream. The socket layer
                    // above will surface the failure through its own channels.
                    let _ = tx.send(InMsg::EndOfStream);
                    break;
                }
            }
        }
    });
    rx
}

/// Mutable state for the std-notifications protocol loop.
///
/// The loop keeps per-user-turn state (`idle`, `waiting_for_final_response`,
/// deferred background-tool fields) separate from all-agents-idle state
/// (`idle_all`, `session_idle`, `all_idle_context`). All-idle tracking is
/// updated before sub-agent filtering because harness-owned `agent.state` and
/// session membership are the source of truth, while user-visible prompt/end
/// hooks must ignore extension side conversations.
struct NotificationLoop<W: Write> {
    /// Protocol writer used for config errors, emitted events, and side-agent
    /// requests.
    writer: PeerOutputWriter<BufWriter<W>>,
    /// Reader-thread channel that supplies decoded harness messages and EOF
    /// markers.
    rx: mpsc::Receiver<InMsg>,
    /// Last valid live extension configuration accepted from the harness.
    /// Reloading this clears pending idle hooks so stale hook indices cannot
    /// render against a new configuration.
    config: ExtConfig,
    /// Pending per-agent idle hooks armed after a completed user turn.
    idle: Vec<PendingIdleHook>,
    /// Pending all-agents-idle hooks keyed by tracked session membership.
    idle_all: Vec<PendingIdleHook>,
    /// Session membership and busy/idle state used to detect all-idle
    /// transitions.
    session_idle: SessionIdleTracker,
    /// Last visible turn text by agent for future all-idle template rendering.
    all_idle_context: HashMap<tau_proto::AgentId, AllIdleTurnContext>,
    /// Summary side agents by pending idle-summary query id. Entries are added
    /// on matching `agent.start_accepted` and removed when the matching
    /// `agent.start_result` arrives, so this extension's own side agents do not
    /// perturb all-idle membership or busy tracking while producing a summary.
    ignored_summary_agents: HashMap<String, tau_proto::AgentId>,
    /// Durable agent display names used by hook template contexts.
    agent_display_names: HashMap<tau_proto::AgentId, String>,
    /// Whether inbound input is closed while idle notifications may still be
    /// pending.
    input_closed: bool,
    /// Whether a visible user prompt has started and is awaiting a final
    /// response.
    waiting_for_final_response: bool,
    /// Whether the current visible turn has already emitted its completion
    /// hook.
    turn_end_emitted: bool,
    /// Whether the final response is deferred until all active user-originated
    /// background tools finish; when set, the three pending-final-response
    /// fields below describe the completion hook that may still be emitted.
    final_response_pending_background_tools: bool,
    /// Prompt id for a final response deferred behind background tool
    /// completion.
    pending_final_response_prompt: Option<tau_proto::AgentPromptId>,
    /// Agent id for a final response deferred behind background tool
    /// completion.
    pending_final_response_agent: Option<tau_proto::AgentId>,
    /// Assistant text for a final response deferred behind background tool
    /// completion.
    pending_final_response_text: String,
    /// Last visible user prompt text supplied to turn-aware hook templates.
    last_user_prompt: String,
    /// Provider response prompt ids already consumed for end-of-turn
    /// notification logic.
    completed_response_prompts: HashSet<tau_proto::AgentPromptId>,
    /// User-originated background tool calls still blocking completion
    /// notification.
    active_background_tools: HashSet<tau_proto::ToolCallId>,
    /// Monotonic suffix for idle-summary side-agent query ids.
    next_query_id: u64,
}

/// Action requested by a protocol-loop message handler.
enum LoopControl {
    /// Continue processing subsequent harness input or idle deadlines.
    Continue,
    /// Exit the protocol loop without waiting for pending idle deadlines.
    Break,
}

impl<W: Write> NotificationLoop<W> {
    fn new(writer: PeerOutputWriter<BufWriter<W>>, rx: mpsc::Receiver<InMsg>) -> Self {
        Self {
            writer,
            rx,
            config: ExtConfig::default(),
            idle: Vec::new(),
            idle_all: Vec::new(),
            session_idle: SessionIdleTracker::default(),
            all_idle_context: HashMap::new(),
            ignored_summary_agents: HashMap::new(),
            agent_display_names: HashMap::new(),
            input_closed: false,
            waiting_for_final_response: false,
            turn_end_emitted: false,
            final_response_pending_background_tools: false,
            pending_final_response_prompt: None,
            pending_final_response_agent: None,
            pending_final_response_text: String::new(),
            last_user_prompt: String::new(),
            completed_response_prompts: HashSet::new(),
            active_background_tools: HashSet::new(),
            next_query_id: 0,
        }
    }

    fn run(
        mut self,
        idle_duration: Duration,
        summary_timeout: Duration,
    ) -> Result<(), Box<dyn Error>> {
        loop {
            match self.recv_next_idle_or_message() {
                Ok(InMsg::Message(message)) => {
                    if let LoopControl::Break = self.handle_message(*message, idle_duration)? {
                        break;
                    }
                }
                Ok(InMsg::EndOfStream) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.input_closed = true;
                    if self.no_pending_idle_hooks() {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.process_timeout(summary_timeout)?;
                    if self.input_closed && self.no_pending_idle_hooks() {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    fn no_pending_idle_hooks(&self) -> bool {
        self.idle.is_empty() && self.idle_all.is_empty()
    }

    fn recv_next_idle_or_message(&self) -> Result<InMsg, mpsc::RecvTimeoutError> {
        match (self.next_deadline(), self.input_closed) {
            (Some(deadline), false) => self
                .rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now())),
            (None, false) => self
                .rx
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected),
            // Input closed but a notification is still pending: the output side
            // is independent, so honor the deadline instead of dropping it.
            (Some(deadline), true) => {
                let wait = deadline.saturating_duration_since(Instant::now());
                if !wait.is_zero() {
                    thread::sleep(wait);
                }
                Err(mpsc::RecvTimeoutError::Timeout)
            }
            (None, true) => Err(mpsc::RecvTimeoutError::Disconnected),
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        next_idle_deadline(&self.idle)
            .into_iter()
            .chain(next_idle_deadline(&self.idle_all))
            .min()
    }

    fn handle_message(
        &mut self,
        message: HarnessOutputMessage,
        idle_duration: Duration,
    ) -> Result<LoopControl, Box<dyn Error>> {
        if matches!(message, HarnessOutputMessage::Disconnect(_)) {
            tracing::info!(target: LOG_TARGET, "disconnect received, exiting");
            return Ok(LoopControl::Break);
        }
        let Some(inner) = self.handle_non_disconnect_message(message)? else {
            return Ok(LoopControl::Continue);
        };
        tracing::trace!(target: LOG_TARGET, name = %inner.name(), "event received");
        self.update_all_idle_tracking(&inner, idle_duration);
        if is_sub_agent_event(&inner) {
            tracing::trace!(target: LOG_TARGET, name = %inner.name(), "skipping sub-agent event");
            return Ok(LoopControl::Continue);
        }
        self.handle_user_event(inner, idle_duration)?;
        Ok(LoopControl::Continue)
    }

    fn handle_non_disconnect_message(
        &mut self,
        message: HarnessOutputMessage,
    ) -> Result<Option<Event>, Box<dyn Error>> {
        match message {
            HarnessOutputMessage::Configure(msg) => {
                self.apply_config(msg.config)?;
                Ok(None)
            }
            HarnessOutputMessage::Deliver(delivery) => {
                if delivery.is_replay() {
                    tracing::trace!(target: LOG_TARGET, name = %delivery.event().name(), "skipping replayed event");
                    Ok(None)
                } else {
                    Ok(Some(delivery.into_event()))
                }
            }
            _ => Ok(None),
        }
    }

    fn apply_config(&mut self, raw_config: tau_proto::CborValue) -> Result<(), Box<dyn Error>> {
        match tau_extension::parse_config::<ExtConfig>(&raw_config) {
            Ok(cfg) => {
                if let Err(message) = cfg.validate() {
                    tracing::warn!(target: LOG_TARGET, error = %message, "rejecting config");
                    self.writer
                        .write_message(&HarnessInputMessage::ConfigError(ConfigError {
                            message,
                        }))?;
                    self.writer.flush()?;
                    return Ok(());
                }
                self.idle.clear();
                self.idle_all.clear();
                tracing::info!(
                    target: LOG_TARGET,
                    agent_start = cfg.agent_start.len(),
                    agent_end = cfg.agent_end.len(),
                    agent_idle = cfg.agent_idle.len(),
                    agent_idle_all = cfg.agent_idle_all.len(),
                    "applied config",
                );
                self.config = cfg;
            }
            Err(message) => {
                tracing::warn!(target: LOG_TARGET, error = %message, "rejecting config");
                self.writer
                    .write_message(&HarnessInputMessage::ConfigError(ConfigError {
                        message: message.clone(),
                    }))?;
                self.writer.flush()?;
            }
        }
        Ok(())
    }

    fn update_all_idle_tracking(&mut self, event: &Event, idle_duration: Duration) {
        match event {
            Event::SessionAgentLoaded(loaded) => self.track_loaded_agent(loaded),
            Event::SessionAgentUnloaded(unloaded) => {
                self.track_unloaded_agent(unloaded, idle_duration);
            }
            Event::AgentState(state) => self.track_agent_state(state, idle_duration),
            Event::StartAgentAccepted(accepted)
                if pending_idle_summary_query(&self.idle, &self.idle_all, &accepted.query_id) =>
            {
                self.ignored_summary_agents
                    .insert(accepted.query_id.clone(), accepted.agent_id.clone());
            }
            Event::ProviderResponseFinished(finished)
                if finished.originator.is_user() && !finished.stop_reason.requests_tool_calls() =>
            {
                self.all_idle_context.insert(
                    finished.agent_id.clone(),
                    AllIdleTurnContext {
                        user_prompt: self.last_user_prompt.clone(),
                        agent_response: response_text(&finished.output_items),
                    },
                );
            }
            _ => {}
        }
    }

    fn track_loaded_agent(&mut self, loaded: &tau_proto::SessionAgentLoaded) {
        if !self
            .ignored_summary_agents
            .values()
            .any(|agent_id| agent_id == &loaded.agent_id)
        {
            self.session_idle
                .load_agent(loaded.session_id.clone(), loaded.agent_id.clone());
        }
    }

    fn track_unloaded_agent(
        &mut self,
        unloaded: &tau_proto::SessionAgentUnloaded,
        idle_duration: Duration,
    ) {
        if let Some(session_id) = self
            .session_idle
            .unload_agent(&unloaded.session_id, &unloaded.agent_id)
        {
            let context = self
                .all_idle_context
                .get(&unloaded.agent_id)
                .cloned()
                .unwrap_or_default();
            arm_idle_all_hooks(
                &mut self.idle_all,
                session_id,
                idle_duration,
                &self.config,
                unloaded.agent_id.clone(),
                context.user_prompt,
                context.agent_response,
            );
        }
    }

    fn track_agent_state(&mut self, state: &tau_proto::AgentStateChanged, idle_duration: Duration) {
        match state.state {
            tau_proto::AgentRuntimeState::Running => self.track_running_agent(&state.agent_id),
            tau_proto::AgentRuntimeState::Idle => {
                self.track_idle_agent(&state.agent_id, idle_duration)
            }
        }
    }

    fn track_running_agent(&mut self, agent_id: &tau_proto::AgentId) {
        if self
            .ignored_summary_agents
            .values()
            .any(|summary_agent_id| summary_agent_id == agent_id)
        {
            return;
        }
        self.session_idle.mark_busy(agent_id.clone());
        let running_sessions = self.session_idle.sessions_for_agent(agent_id);
        self.idle_all.retain(|pending| {
            pending
                .session_id
                .as_ref()
                .is_none_or(|session_id| !running_sessions.contains(session_id))
        });
    }

    fn track_idle_agent(&mut self, agent_id: &tau_proto::AgentId, idle_duration: Duration) {
        for session_id in self.session_idle.mark_idle(agent_id) {
            let context = self
                .all_idle_context
                .get(agent_id)
                .cloned()
                .unwrap_or_default();
            arm_idle_all_hooks(
                &mut self.idle_all,
                session_id,
                idle_duration,
                &self.config,
                agent_id.clone(),
                context.user_prompt,
                context.agent_response,
            );
        }
    }

    fn handle_user_event(
        &mut self,
        event: Event,
        idle_duration: Duration,
    ) -> Result<(), Box<dyn Error>> {
        match event {
            Event::AgentStarted(started) => {
                self.set_display_name(started.agent_id, started.display_name.as_deref());
            }
            Event::AgentDisplayNameSet(name) => {
                self.set_display_name(name.agent_id, Some(&name.display_name));
            }
            Event::ProviderPromptSubmitted(_) => self.idle.clear(),
            Event::AgentPromptSubmitted(prompt) => self.handle_agent_prompt(prompt)?,
            Event::UiPromptDraft(_) => self.extend_idle_deadlines(idle_duration),
            Event::ProviderResponseFinished(finished) => {
                self.handle_provider_response_finished(finished, idle_duration)?;
            }
            Event::ToolResult(result) => self.handle_tool_result(result),
            Event::ToolBackgroundResult(result) => {
                self.handle_background_tool_finished(
                    result.call_id,
                    result.originator,
                    idle_duration,
                )?;
            }
            Event::ToolBackgroundError(error) => {
                self.handle_background_tool_finished(
                    error.call_id,
                    error.originator,
                    idle_duration,
                )?;
            }
            Event::StartAgentResult(result) => self.handle_start_agent_result(result)?,
            other => {
                tracing::trace!(target: LOG_TARGET, name = %other.name(), "ignoring unhandled event")
            }
        }
        Ok(())
    }

    fn set_display_name(&mut self, agent_id: tau_proto::AgentId, display_name: Option<&str>) {
        if let Some(display_name) = display_name.map(str::trim).filter(|name| !name.is_empty()) {
            self.agent_display_names
                .insert(agent_id, display_name.to_owned());
        }
    }

    fn handle_agent_prompt(
        &mut self,
        prompt: tau_proto::AgentPromptSubmitted,
    ) -> Result<(), Box<dyn Error>> {
        self.set_display_name(prompt.agent_id.clone(), prompt.display_name.as_deref());
        self.idle.clear();
        if prompt.message_class.is_internal() {
            tracing::trace!(target: LOG_TARGET, "skipping internal prompt submit");
            return Ok(());
        }
        if self.final_response_pending_background_tools {
            self.cancel_deferred_final_response();
        }
        if !self.waiting_for_final_response {
            self.last_user_prompt = prompt.text.clone();
            let agent_name = display_name_for_agent(&self.agent_display_names, &prompt.agent_id);
            let ctx = template_context(
                "agent_start",
                &prompt.agent_id,
                &agent_name,
                &self.last_user_prompt,
                "",
                "",
            );
            emit_hooks(&mut self.writer, &self.config.agent_start, &ctx)?;
            self.waiting_for_final_response = true;
            self.turn_end_emitted = false;
        }
        Ok(())
    }

    fn cancel_deferred_final_response(&mut self) {
        self.final_response_pending_background_tools = false;
        if let Some(prompt_id) = self.pending_final_response_prompt.take() {
            self.completed_response_prompts.insert(prompt_id);
        }
        self.waiting_for_final_response = false;
        self.turn_end_emitted = false;
    }

    fn extend_idle_deadlines(&mut self, idle_duration: Duration) {
        let now = Instant::now();
        for pending in self.idle.iter_mut().chain(self.idle_all.iter_mut()) {
            let delay = idle_hook_delay(&self.config, pending, idle_duration);
            if let IdleState::WaitingIdle { deadline } = &mut pending.state {
                *deadline = now + delay;
            }
        }
        if !self.no_pending_idle_hooks() {
            tracing::trace!(target: LOG_TARGET, "extended idle deadlines on prompt draft");
        }
    }

    fn handle_provider_response_finished(
        &mut self,
        finished: tau_proto::ProviderResponseFinished,
        idle_duration: Duration,
    ) -> Result<(), Box<dyn Error>> {
        if self.should_skip_finished_response(&finished) {
            return Ok(());
        }
        if self.active_background_tools.is_empty() {
            self.emit_finished_response(finished, idle_duration)?;
        } else {
            self.defer_finished_response(finished);
        }
        Ok(())
    }

    fn should_skip_finished_response(
        &self,
        finished: &tau_proto::ProviderResponseFinished,
    ) -> bool {
        if finished.stop_reason.requests_tool_calls() {
            tracing::trace!(target: LOG_TARGET, stop_reason = ?finished.stop_reason, "skipping mid-turn ProviderResponseFinished");
            return true;
        }
        if self
            .completed_response_prompts
            .contains(&finished.agent_prompt_id)
        {
            tracing::trace!(target: LOG_TARGET, agent_prompt_id = %finished.agent_prompt_id, "skipping already-completed response");
            return true;
        }
        if self.turn_end_emitted {
            tracing::trace!(target: LOG_TARGET, "skipping already-completed turn");
            return true;
        }
        false
    }

    fn emit_finished_response(
        &mut self,
        finished: tau_proto::ProviderResponseFinished,
        idle_duration: Duration,
    ) -> Result<(), Box<dyn Error>> {
        let agent_id = finished.agent_id.clone();
        let agent_name = display_name_for_agent(&self.agent_display_names, &agent_id);
        let agent_response = response_text(&finished.output_items);
        emit_agent_end(
            &mut self.writer,
            &mut self.waiting_for_final_response,
            &mut self.turn_end_emitted,
            &mut self.idle,
            idle_duration,
            &self.config,
            agent_id,
            agent_name,
            self.last_user_prompt.clone(),
            agent_response,
        )?;
        self.completed_response_prompts
            .insert(finished.agent_prompt_id);
        Ok(())
    }

    fn defer_finished_response(&mut self, finished: tau_proto::ProviderResponseFinished) {
        self.final_response_pending_background_tools = true;
        self.pending_final_response_prompt = Some(finished.agent_prompt_id);
        self.pending_final_response_agent = Some(finished.agent_id);
        self.pending_final_response_text = response_text(&finished.output_items);
        tracing::debug!(
            target: LOG_TARGET,
            active_background_tools = self.active_background_tools.len(),
            "deferring end notification until background tools complete",
        );
    }

    fn handle_tool_result(&mut self, result: tau_proto::ToolResult) {
        if result.originator.is_user()
            && result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder
        {
            self.active_background_tools.insert(result.call_id);
            tracing::trace!(
                target: LOG_TARGET,
                active_background_tools = self.active_background_tools.len(),
                "background tool started",
            );
        }
    }

    fn handle_background_tool_finished(
        &mut self,
        call_id: tau_proto::ToolCallId,
        originator: tau_proto::PromptOriginator,
        idle_duration: Duration,
    ) -> Result<(), Box<dyn Error>> {
        if !originator.is_user() {
            return Ok(());
        }
        self.active_background_tools.remove(&call_id);
        if maybe_emit_deferred_agent_end(
            &mut self.writer,
            &mut self.waiting_for_final_response,
            &mut self.turn_end_emitted,
            &mut self.final_response_pending_background_tools,
            &mut self.idle,
            idle_duration,
            &self.config,
            &self.active_background_tools,
            &self.agent_display_names,
            &mut self.pending_final_response_agent,
            &self.last_user_prompt,
            &mut self.pending_final_response_text,
        )? && let Some(prompt_id) = self.pending_final_response_prompt.take()
        {
            self.completed_response_prompts.insert(prompt_id);
        }
        Ok(())
    }

    fn handle_start_agent_result(
        &mut self,
        result: tau_proto::StartAgentResult,
    ) -> Result<(), Box<dyn Error>> {
        tracing::debug!(
            target: LOG_TARGET,
            query_id = %result.query_id,
            text_len = result.text.len(),
            error = ?result.error,
            idle_hooks = self.idle.len(),
            "received StartAgentResult",
        );
        self.ignored_summary_agents.remove(&result.query_id);
        let Some((is_all_idle, index)) = self.matching_summary_hook(&result.query_id) else {
            return Ok(());
        };
        let pending = if is_all_idle {
            self.idle_all.remove(index)
        } else {
            self.idle.remove(index)
        };
        let agent_summary = if result.error.is_some() {
            String::new()
        } else {
            truncate_for_summary_text(result.text.trim())
        };
        self.emit_summary_result(pending, &agent_summary)
    }

    fn matching_summary_hook(&self, query_id: &str) -> Option<(bool, usize)> {
        self.idle
            .iter()
            .position(|pending| idle_summary_query_matches(pending, query_id))
            .map(|index| (false, index))
            .or_else(|| {
                self.idle_all
                    .iter()
                    .position(|pending| idle_summary_query_matches(pending, query_id))
                    .map(|index| (true, index))
            })
    }

    fn emit_summary_result(
        &mut self,
        pending: PendingIdleHook,
        agent_summary: &str,
    ) -> Result<(), Box<dyn Error>> {
        let hook = configured_idle_hook(&self.config, &pending);
        let agent_name = display_name_for_agent(&self.agent_display_names, &pending.agent_id);
        emit_idle_hook(
            &mut self.writer,
            IdleHookEmission {
                hook_name: idle_hook_name(pending.hook_kind),
                hook,
                agent_id: &pending.agent_id,
                agent_name: &agent_name,
                user_prompt: &pending.user_prompt,
                agent_response: &pending.agent_response,
                agent_summary,
            },
        )
    }

    fn process_timeout(&mut self, summary_timeout: Duration) -> Result<(), Box<dyn Error>> {
        let now = Instant::now();
        process_due_idle_hooks(
            &mut self.writer,
            &mut self.idle,
            now,
            &self.config,
            &self.agent_display_names,
            summary_timeout,
            &mut self.next_query_id,
            "idle",
        )?;
        process_due_idle_hooks(
            &mut self.writer,
            &mut self.idle_all,
            now,
            &self.config,
            &self.agent_display_names,
            summary_timeout,
            &mut self.next_query_id,
            "all-idle",
        )?;
        Ok(())
    }
}

fn summary_instruction(user_prompt: &str, agent_response: &str) -> String {
    format!(
        "{SUMMARY_INSTRUCTION}\n\nRecent visible turn context follows. \
         Summarize only this captured context; do not mention these labels.\n\n\
         User prompt:\n{}\n\nAssistant response:\n{}",
        truncate_for_summary_context(user_prompt),
        truncate_for_summary_context(agent_response),
    )
}

fn truncate_for_summary_context(text: &str) -> String {
    truncate_utf8_with_marker(text, SUMMARY_CONTEXT_LIMIT_BYTES)
}

fn truncate_for_summary_text(text: &str) -> String {
    truncate_utf8_with_marker(text, SUMMARY_TEXT_LIMIT_BYTES)
}

fn truncate_utf8_with_marker(text: &str, limit_bytes: usize) -> String {
    if text.len() <= limit_bytes {
        text.to_owned()
    } else {
        let end = text
            .char_indices()
            .map(|(index, _)| index)
            .take_while(|index| *index <= limit_bytes)
            .last()
            .unwrap_or(0);
        format!("{}… [truncated]", &text[..end])
    }
}

#[allow(clippy::too_many_arguments)]
fn process_due_idle_hooks<W: Write>(
    writer: &mut PeerOutputWriter<BufWriter<W>>,
    pending_hooks: &mut Vec<PendingIdleHook>,
    now: Instant,
    config: &ExtConfig,
    agent_display_names: &HashMap<tau_proto::AgentId, String>,
    summary_timeout: Duration,
    next_query_id: &mut u64,
    log_prefix: &str,
) -> Result<(), Box<dyn Error>> {
    while let Some(index) = pending_hooks
        .iter()
        .position(|pending| pending.deadline() <= now)
    {
        let mut pending = pending_hooks.remove(index);
        let hook = configured_idle_hook(config, &pending);
        match pending.state {
            IdleState::WaitingIdle { .. } if hook.agent_summary => {
                let query_id = format!("idle-{next_query_id}");
                *next_query_id += 1;
                tracing::info!(
                    target: LOG_TARGET,
                    query_id = %query_id,
                    "{log_prefix} deadline elapsed, requesting agent summary",
                );
                let instruction =
                    summary_instruction(&pending.user_prompt, &pending.agent_response);
                writer.write_message(&HarnessInputMessage::emit(Event::StartAgentRequest(
                    StartAgentRequest {
                        parent_agent: None,
                        query_id: query_id.clone(),
                        instruction,
                        role: None,
                        input_stats: tau_proto::ToolUseStats::default(),
                        tool_call_id: None,
                        task_name: None,
                    },
                )))?;
                writer.flush()?;
                pending.state = IdleState::WaitingSummary {
                    query_id,
                    deadline: Instant::now() + summary_timeout,
                };
                pending_hooks.push(pending);
            }
            IdleState::WaitingIdle { .. } => {
                tracing::info!(
                    target: LOG_TARGET,
                    "{log_prefix} deadline elapsed, emitting static notification",
                );
                emit_due_idle_hook(writer, config, agent_display_names, &pending, "")?;
            }
            IdleState::WaitingSummary { .. } => {
                tracing::info!(
                    target: LOG_TARGET,
                    "summary timed out, falling back to static notification",
                );
                emit_due_idle_hook(writer, config, agent_display_names, &pending, "")?;
            }
        }
    }
    Ok(())
}

fn emit_due_idle_hook<W: Write>(
    writer: &mut PeerOutputWriter<BufWriter<W>>,
    config: &ExtConfig,
    agent_display_names: &HashMap<tau_proto::AgentId, String>,
    pending: &PendingIdleHook,
    agent_summary: &str,
) -> Result<(), Box<dyn Error>> {
    let hook = configured_idle_hook(config, pending);
    let agent_name = display_name_for_agent(agent_display_names, &pending.agent_id);
    emit_idle_hook(
        writer,
        IdleHookEmission {
            hook_name: idle_hook_name(pending.hook_kind),
            hook,
            agent_id: &pending.agent_id,
            agent_name: &agent_name,
            user_prompt: &pending.user_prompt,
            agent_response: &pending.agent_response,
            agent_summary,
        },
    )
}
#[allow(clippy::too_many_arguments)]
fn emit_agent_end<W: Write>(
    writer: &mut PeerOutputWriter<BufWriter<W>>,
    waiting_for_final_response: &mut bool,
    turn_end_emitted: &mut bool,
    idle: &mut Vec<PendingIdleHook>,
    default_idle_duration: Duration,
    config: &ExtConfig,
    agent_id: tau_proto::AgentId,
    agent_name: String,
    user_prompt: String,
    agent_response: String,
) -> Result<(), Box<dyn Error>> {
    let ctx = template_context(
        "agent_end",
        &agent_id,
        &agent_name,
        &user_prompt,
        &agent_response,
        "",
    );
    emit_hooks(writer, &config.agent_end, &ctx)?;
    *waiting_for_final_response = false;
    *turn_end_emitted = true;
    arm_idle_hooks(
        idle,
        default_idle_duration,
        config,
        agent_id,
        user_prompt,
        agent_response,
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn maybe_emit_deferred_agent_end<W: Write>(
    writer: &mut PeerOutputWriter<BufWriter<W>>,
    waiting_for_final_response: &mut bool,
    turn_end_emitted: &mut bool,
    final_response_pending_background_tools: &mut bool,
    idle: &mut Vec<PendingIdleHook>,
    default_idle_duration: Duration,
    config: &ExtConfig,
    active_background_tools: &HashSet<tau_proto::ToolCallId>,
    agent_display_names: &HashMap<tau_proto::AgentId, String>,
    pending_agent_id: &mut Option<tau_proto::AgentId>,
    user_prompt: &str,
    pending_response: &mut String,
) -> Result<bool, Box<dyn Error>> {
    if *final_response_pending_background_tools && active_background_tools.is_empty() {
        *final_response_pending_background_tools = false;
        let Some(agent_id) = pending_agent_id.take() else {
            return Ok(false);
        };
        let agent_name = display_name_for_agent(agent_display_names, &agent_id);
        let agent_response = std::mem::take(pending_response);
        emit_agent_end(
            writer,
            waiting_for_final_response,
            turn_end_emitted,
            idle,
            default_idle_duration,
            config,
            agent_id,
            agent_name,
            user_prompt.to_owned(),
            agent_response,
        )?;
        return Ok(true);
    }
    Ok(false)
}

fn arm_idle_hooks(
    idle: &mut Vec<PendingIdleHook>,
    default_idle_duration: Duration,
    config: &ExtConfig,
    agent_id: tau_proto::AgentId,
    user_prompt: String,
    agent_response: String,
) {
    idle.clear();
    let now = Instant::now();
    for (hook_index, hook) in config.agent_idle.iter().enumerate() {
        idle.push(PendingIdleHook {
            hook_kind: IdleHookKind::Agent,
            hook_index,
            agent_id: agent_id.clone(),
            session_id: None,
            user_prompt: user_prompt.clone(),
            agent_response: agent_response.clone(),
            state: IdleState::WaitingIdle {
                deadline: now + hook.delay_duration(default_idle_duration),
            },
        });
    }
    if !idle.is_empty() {
        tracing::debug!(target: LOG_TARGET, count = idle.len(), "idle deadlines armed");
    }
}

fn arm_idle_all_hooks(
    idle_all: &mut Vec<PendingIdleHook>,
    session_id: tau_proto::SessionId,
    default_idle_duration: Duration,
    config: &ExtConfig,
    agent_id: tau_proto::AgentId,
    user_prompt: String,
    agent_response: String,
) {
    idle_all.retain(|pending| pending.session_id.as_ref() != Some(&session_id));
    let now = Instant::now();
    for (hook_index, hook) in config.agent_idle_all.iter().enumerate() {
        idle_all.push(PendingIdleHook {
            hook_kind: IdleHookKind::AgentAll,
            hook_index,
            agent_id: agent_id.clone(),
            session_id: Some(session_id.clone()),
            user_prompt: user_prompt.clone(),
            agent_response: agent_response.clone(),
            state: IdleState::WaitingIdle {
                deadline: now + hook.delay_duration(default_idle_duration),
            },
        });
    }
    if !idle_all.is_empty() {
        tracing::debug!(target: LOG_TARGET, count = idle_all.len(), "all-idle deadlines armed");
    }
}

fn pending_idle_summary_query(
    idle: &[PendingIdleHook],
    idle_all: &[PendingIdleHook],
    expected_query_id: &str,
) -> bool {
    idle.iter()
        .chain(idle_all)
        .any(|pending| idle_summary_query_matches(pending, expected_query_id))
}
fn idle_summary_query_matches(pending: &PendingIdleHook, expected_query_id: &str) -> bool {
    matches!(
        &pending.state,
        IdleState::WaitingSummary { query_id, .. } if query_id == expected_query_id
    )
}
fn idle_hook_name(kind: IdleHookKind) -> &'static str {
    match kind {
        IdleHookKind::Agent => "agent_idle",
        IdleHookKind::AgentAll => "agent_idle_all",
    }
}

fn configured_idle_hook<'a>(
    config: &'a ExtConfig,
    pending: &PendingIdleHook,
) -> &'a IdleHookConfig {
    match pending.hook_kind {
        IdleHookKind::Agent => &config.agent_idle[pending.hook_index],
        IdleHookKind::AgentAll => &config.agent_idle_all[pending.hook_index],
    }
}

fn idle_hook_delay(
    config: &ExtConfig,
    pending: &PendingIdleHook,
    default_idle_duration: Duration,
) -> Duration {
    configured_idle_hook(config, pending).delay_duration(default_idle_duration)
}
fn next_idle_deadline(idle: &[PendingIdleHook]) -> Option<Instant> {
    idle.iter().map(PendingIdleHook::deadline).min()
}

fn emit_hooks<W: Write>(
    writer: &mut PeerOutputWriter<BufWriter<W>>,
    hooks: &[HookConfig],
    ctx: &TemplateContext<'_>,
) -> Result<(), Box<dyn Error>> {
    for hook in hooks {
        emit_hook(writer, hook, ctx)?;
    }
    writer.flush()?;
    Ok(())
}

fn emit_hook<W: Write>(
    writer: &mut PeerOutputWriter<BufWriter<W>>,
    hook: &HookConfig,
    ctx: &TemplateContext<'_>,
) -> Result<(), Box<dyn Error>> {
    if hook.bell {
        writer.write_message(&HarnessInputMessage::emit(Event::TermBell(TermBell {})))?;
    }
    if let Some(osc) = &hook.osc1337 {
        let name = render_template(&osc.key, ctx)?;
        let value = render_template(&osc.value, ctx)?;
        match validate_osc1337_name(&name) {
            Ok(()) => writer.write_message(&HarnessInputMessage::emit(
                Event::Osc1337SetUserVar(Osc1337SetUserVar { name, value }),
            ))?,
            Err(message) => {
                tracing::warn!(
                    target: LOG_TARGET,
                    name_len = name.len(),
                    error = %message,
                    "skipping notification with invalid OSC 1337 user-var name",
                );
            }
        }
    }
    if let Some(command) = &hook.command {
        spawn_command(command, ctx);
    }
    Ok(())
}

fn validate_hooks(name: &str, hooks: &[HookConfig]) -> Result<(), String> {
    for hook in hooks {
        validate_hook(name, hook)?;
    }
    Ok(())
}

fn validate_hook(name: &str, hook: &HookConfig) -> Result<(), String> {
    if !hook.bell && hook.command.is_none() && hook.osc1337.is_none() {
        return Err(format!(
            "{name} hook item must set bell, command, or osc1337"
        ));
    }
    let agent_id = tau_proto::AgentId::parse("agent").expect("valid test agent id");
    let ctx = template_context(
        name,
        &agent_id,
        "Agent",
        "user prompt",
        "agent response",
        "agent summary",
    );
    if let Some(osc) = &hook.osc1337 {
        let rendered_key = render_template(&osc.key, &ctx)
            .map_err(|e| format!("{name} osc1337.key template failed: {e}"))?;
        validate_osc1337_name(&rendered_key)
            .map_err(|e| format!("{name} osc1337.key is invalid: {e}"))?;
        render_template(&osc.value, &ctx)
            .map_err(|e| format!("{name} osc1337.value template failed: {e}"))?;
    }
    if let Some(command) = &hook.command {
        if command.is_empty() {
            return Err(format!("{name} command must not be empty"));
        }
        for part in command {
            render_template(part, &ctx)
                .map_err(|e| format!("{name} command template failed: {e}"))?;
        }
    }
    Ok(())
}

fn render_template(template: &str, ctx: &TemplateContext<'_>) -> Result<String, Box<dyn Error>> {
    let mut handlebars = handlebars::Handlebars::new();
    handlebars.set_strict_mode(true);
    handlebars.register_escape_fn(handlebars::no_escape);
    Ok(handlebars.render_template(template, ctx)?)
}

fn validate_osc1337_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("must not be empty".to_owned());
    }
    if name.len() > MAX_OSC1337_NAME_LEN {
        return Err(format!(
            "must be at most {MAX_OSC1337_NAME_LEN} bytes, got {}",
            name.len()
        ));
    }
    for ch in name.chars() {
        if !ch.is_ascii() {
            return Err("must contain printable ASCII only".to_owned());
        }
        if ch.is_ascii_control() || ch == '=' {
            return Err("must not contain '=', BEL/ESC, or control characters".to_owned());
        }
    }
    Ok(())
}

/// True when `event` belongs to a side conversation spawned by an
/// extension (`PromptOriginator::Extension`). Side conversations
/// share the bus with the user's interactive turn; this extension
/// must skip them so sub-agent activity (e.g. an `agent_start` sub-task
/// or this extension's own idle-summarizer query) doesn't fire
/// chimes or perturb the idle timer.
fn is_sub_agent_event(event: &Event) -> bool {
    match event {
        Event::ProviderPromptSubmitted(s) => !s.originator.is_user(),
        Event::ProviderResponseUpdated(u) => !u.originator.is_user(),
        Event::ProviderResponseFinished(f) => !f.originator.is_user(),
        Event::AgentPromptSubmitted(p) => !p.originator.is_user(),
        Event::AgentPromptCreated(p) => !p.originator.is_user(),
        _ => false,
    }
}

/// Borrowed data needed to render and emit one configured idle hook.
struct IdleHookEmission<'a> {
    /// Template hook name to expose as `hook`.
    hook_name: &'a str,
    /// Configured idle hook whose actions should be emitted.
    hook: &'a IdleHookConfig,
    /// Agent id that supplies `agent.id` in templates.
    agent_id: &'a tau_proto::AgentId,
    /// Agent display name that supplies `agent.name` in templates.
    agent_name: &'a str,
    /// Captured user prompt that supplies `turn.user_prompt`.
    user_prompt: &'a str,
    /// Captured assistant response that supplies `turn.agent_response`.
    agent_response: &'a str,
    /// Optional summary text that supplies `turn.agent_summary`.
    agent_summary: &'a str,
}

fn emit_idle_hook<W: Write>(
    writer: &mut PeerOutputWriter<BufWriter<W>>,
    emission: IdleHookEmission<'_>,
) -> Result<(), Box<dyn Error>> {
    let ctx = template_context(
        emission.hook_name,
        emission.agent_id,
        emission.agent_name,
        emission.user_prompt,
        emission.agent_response,
        emission.agent_summary,
    );
    emit_hook(writer, &emission.hook.hook, &ctx)?;
    writer.flush()?;
    Ok(())
}

fn spawn_command(command_template: &[String], ctx: &TemplateContext<'_>) {
    if command_template.is_empty() {
        tracing::warn!(target: LOG_TARGET, "hook command is set but empty; ignoring");
        return;
    }
    let mut argv = Vec::with_capacity(command_template.len());
    for part in command_template {
        match render_template(part, ctx) {
            Ok(rendered) => argv.push(rendered),
            Err(e) => {
                tracing::warn!(
                    target: LOG_TARGET,
                    error = %e,
                    "failed to render notification command template",
                );
                return;
            }
        }
    }
    std::thread::spawn(move || {
        let program = &argv[0];
        let mut command = Command::new(program);
        command
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match command.status() {
            Ok(status) if !status.success() => {
                tracing::warn!(
                    target: LOG_TARGET,
                    program = %program,
                    status = ?status,
                    "notification command exited non-zero",
                );
            }
            Err(e) => {
                tracing::warn!(
                    target: LOG_TARGET,
                    program = %program,
                    error = %e,
                    "notification command failed",
                );
            }
            _ => {}
        }
    });
}

#[cfg(test)]
mod tests;
