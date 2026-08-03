//! First-party utility extension.
//!
//! The MVP registers one `timer` tool. Timers are active-only, session-scoped
//! state reconstructed from replayed `tool.started`, terminal tool results, and
//! timer-generated `agent.prompt_submitted` facts; there is no separate timer
//! store.

#[cfg(test)]
mod tests;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::io::{Read, Write};
use std::time::Duration;

use tau_client::{
    ClientHandle, ClientResult, DispatchOutcome, ExtensionBuilder, ManualExtensionRuntime,
    ManualRuntimeInput, RawEventContext, TauExtension, TauExtensionRunner, ToolContext,
};
use tau_proto::{
    AgentId, AgentPromptSteered, AgentPromptSubmitted, AgentReplayComplete, CborValue, Event,
    EventName, EventSelector, ExtInternalPromptSubmitRequest, HarnessInputMessage,
    SessionAgentUnloaded, SessionShutdown, SessionStarted, ToolError, ToolResult, ToolResultKind,
    ToolSpec, ToolStarted, ToolType, ToolUseState, ToolUseStats, ToolUseStatus, UnixMicros,
};

/// Protocol/logging name for the utility extension.
pub const EXTENSION_NAME: &str = "tau-ext-utils";
/// Model-visible timer tool name.
pub const TIMER_TOOL_NAME: &str = "timer";

const MAX_TIMERS_PER_AGENT: usize = 32;
const MAX_TIMERS_TOTAL: usize = 128;
const MAX_MESSAGE_BYTES: usize = 4096;
const MAX_TIMER_ID_BYTES: usize = 64;
const MIN_DELAY_SECONDS: u64 = 10;
const MIN_INTERVAL_SECONDS: u64 = 60;
const MAX_DELAY_SECONDS: u64 = 10 * 365 * 24 * 60 * 60;
const MAX_LIST_TIMERS: usize = 64;
const DEFAULT_WAIT: Duration = Duration::from_secs(60);

/// Run the extension on stdin/stdout.
///
/// # Errors
///
/// Returns protocol I/O, handshake, or handler errors that prevent the
/// extension from continuing.
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    run(std::io::stdin(), std::io::stdout())
}

/// Run the extension over caller-provided protocol streams.
///
/// # Errors
///
/// Returns protocol I/O, handshake, or handler errors that prevent the
/// extension from continuing.
pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let runtime = TauExtensionRunner::new(UtilsExtension).start_manual_loop_with_state(
        reader,
        writer,
        TimerRuntime::new,
    )?;
    TimerRuntime::run(runtime)?;
    Ok(())
}

struct UtilsExtension;

impl TauExtension for UtilsExtension {
    type State = TimerRuntime;

    fn name(&self) -> &'static str {
        EXTENSION_NAME
    }

    fn register(self, builder: &mut ExtensionBuilder<Self::State>) {
        builder.ready_message("utils ready");
        builder.tool_with_group_and_prompt_fragment(
            timer_tool_spec(),
            Some(tau_proto::ToolGroup {
                name: tau_proto::ToolGroupName::new("timer"),
                prompt_fragment: None,
            }),
            Some(tau_proto::PromptFragment::new(
                "timer",
                tau_proto::PromptPriority::new(0),
                "Do not use timers to wait for or poll tools or commands; completion is delivered automatically. Use timers only for genuine external elapsed-time waits or wakeups.",
            )),
            handle_timer_tool,
        );
        builder
            .on_restore::<ToolStarted>(handle_replay_tool_started)
            .on_raw_restore(
                EventSelector::Exact(EventName::TOOL_RESULT),
                handle_replay_terminal_result,
            )
            .on_raw_restore(
                EventSelector::Exact(EventName::PROVIDER_TOOL_RESULT),
                handle_replay_terminal_result,
            )
            .on_raw_restore(
                EventSelector::Exact(EventName::TOOL_ERROR),
                handle_replay_terminal_error,
            )
            .on_raw_restore(
                EventSelector::Exact(EventName::PROVIDER_TOOL_ERROR),
                handle_replay_terminal_error,
            )
            .on_restore::<AgentPromptSubmitted>(handle_replay_prompt_submitted)
            .on_restore::<AgentPromptSteered>(handle_replay_prompt_steered)
            .on_live::<AgentReplayComplete>(handle_agent_replay_complete)
            .on_live::<SessionStarted>(handle_session_started)
            .on_live::<SessionShutdown>(handle_session_shutdown)
            .on_live::<SessionAgentUnloaded>(handle_session_agent_unloaded);
    }
}

/// Runtime and folded timer state owned by the single-threaded manual loop.
struct TimerRuntime {
    /// Outbound handle used by due-timer firing outside ordinary handlers.
    handle: Option<ClientHandle>,
    /// Active timers keyed by `(agent_id, timer_id)`.
    timers: HashMap<TimerKey, TimerEntry>,
    /// Replayed live timer invocations awaiting their terminal success/error.
    pending_invocations: HashMap<String, PendingInvocation>,
    /// Agents whose restore boundary succeeded and may receive timer prompts.
    replay_complete_agents: HashSet<AgentId>,
}

/// Stable map key for one active timer.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct TimerKey {
    /// Agent that owns the timer.
    agent_id: AgentId,
    /// Timer id unique within that agent.
    timer_id: String,
}

/// Active timer reconstructed from session-scoped history.
#[derive(Clone, Debug, Eq, PartialEq)]
struct TimerEntry {
    /// Agent that owns the timer.
    agent_id: AgentId,
    /// Stable path-safe timer id.
    timer_id: String,
    /// Next due timestamp in Unix microseconds.
    next_fire_at: UnixMicros,
    /// Optional recurrence interval.
    interval_seconds: Option<u64>,
    /// Internal prompt message to submit when the timer fires.
    message: String,
    /// Number of prompt submissions this timer has already fired.
    fired_count: u64,
}

/// Replayed timer tool invocation awaiting a terminal result.
#[derive(Clone, Debug)]
struct PendingInvocation {
    /// Agent that owned the original tool call.
    agent_id: AgentId,
    /// Stable tool call id used to correlate terminal results.
    call_id: String,
    /// Original tool arguments copied from `tool.started`.
    arguments: CborValue,
}

/// Parsed timer tool action.
#[derive(Clone, Debug, Eq, PartialEq)]
enum TimerAction {
    /// Insert or replace an active timer.
    Schedule(ScheduleArgs),
    /// Remove an active timer if present.
    Cancel {
        /// Timer id to cancel for the invoking agent.
        timer_id: String,
    },
    /// Report active timers for the invoking agent.
    List,
}

/// Validated arguments for a `schedule` action.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ScheduleArgs {
    /// Stable path-safe timer id.
    timer_id: String,
    /// Initial delay before the first firing.
    delay_seconds: u64,
    /// Optional recurrence interval after each firing.
    interval_seconds: Option<u64>,
    /// Internal prompt body to submit when due.
    message: String,
}

/// Successful live timer result and its terminal-only display projection.
struct TimerToolCompletion {
    /// Unchanged model-visible result for the tool invocation.
    result: CborValue,
    /// Bounded terminal metadata rendered by the CLI.
    display: ToolUseState,
}

/// Model result and display-only facts from one successful timer action.
struct TimerActionResult {
    /// Unchanged model-visible result for the action.
    result: CborValue,
    /// Bounded fact that selects the terminal display context.
    outcome: TimerActionOutcome,
}

/// Operation facts needed only to describe a successful terminal display.
enum TimerActionOutcome {
    /// The action's validated arguments already provide sufficient context.
    NoAdditionalContext,
    /// A cancellation found no active timer with the requested id.
    NotActive,
    /// A list result contained this many timer rows.
    Listed {
        /// Number of active timers included in the bounded list result.
        matches: u64,
    },
}

/// Prepared internal prompt for one due timer firing.
#[derive(Clone, Debug, Eq, PartialEq)]
struct FireRecord {
    /// Agent receiving the wakeup prompt.
    agent_id: AgentId,
    /// Timer id that fired.
    timer_id: String,
    /// Prompt text submitted to the agent.
    prompt: String,
    /// Stable prompt correlation id used during replay folding.
    ctx_id: String,
}

impl FireRecord {
    /// Convert one due timer into an explicit `persist=false` wire request.
    fn into_internal_prompt_message(self) -> HarnessInputMessage {
        HarnessInputMessage::emit_transient(Event::ExtInternalPromptSubmitRequest(
            ExtInternalPromptSubmitRequest {
                agent_id: self.agent_id,
                text: self.prompt,
                ctx_id: Some(self.ctx_id),
                activation_kind: Some(tau_proto::InternalPromptActivationKind::Timer),
            },
        ))
    }
}

impl TimerRuntime {
    fn new(handle: ClientHandle) -> Self {
        Self {
            handle: Some(handle),
            timers: HashMap::new(),
            pending_invocations: HashMap::new(),
            replay_complete_agents: HashSet::new(),
        }
    }

    fn run(mut runtime: ManualExtensionRuntime<Self>) -> ClientResult<()> {
        loop {
            let input = match recv_next(&mut runtime) {
                Ok(input) => input,
                Err(error) => return finish_with_error(runtime, error),
            };
            match input {
                LoopInput::Message(message) => {
                    match runtime.dispatch_one(message) {
                        Ok(DispatchOutcome::Continue) => {}
                        Ok(DispatchOutcome::Disconnect(_)) => {
                            let _state = runtime.finish_detached();
                            return Ok(());
                        }
                        Ok(DispatchOutcome::StopRequested) => break,
                        Err(error) => return finish_with_error(runtime, error),
                    }
                    if let Err(error) = runtime.state_mut().fire_due_now() {
                        return finish_with_error(runtime, error);
                    }
                }
                LoopInput::Timeout => {
                    if let Err(error) = runtime.state_mut().fire_due_now() {
                        return finish_with_error(runtime, error);
                    }
                }
                LoopInput::InputClosed => break,
            }
        }
        let _state = runtime.finish()?;
        Ok(())
    }

    fn next_deadline_duration(&self) -> Duration {
        let now = UnixMicros::now();
        self.timers
            .values()
            .filter(|timer| self.replay_complete_agents.contains(&timer.agent_id))
            .map(|timer| duration_until(timer.next_fire_at, now))
            .min()
            .unwrap_or(DEFAULT_WAIT)
    }

    fn fire_due_now(&mut self) -> ClientResult<()> {
        if self.handle.is_none() {
            return Ok(());
        }
        let fires = self.collect_due(UnixMicros::now());
        for fire in fires {
            let Some(handle) = &self.handle else {
                continue;
            };
            handle.send(fire.into_internal_prompt_message())?;
        }
        Ok(())
    }

    fn collect_due(&mut self, now: UnixMicros) -> Vec<FireRecord> {
        let mut due_keys: Vec<_> = self
            .timers
            .iter()
            .filter(|(_, timer)| {
                self.replay_complete_agents.contains(&timer.agent_id) && timer.next_fire_at <= now
            })
            .map(|(key, _)| key.clone())
            .collect();
        due_keys.sort_by(|a, b| {
            a.agent_id
                .as_ref()
                .cmp(b.agent_id.as_ref())
                .then(a.timer_id.cmp(&b.timer_id))
        });

        let mut fires = Vec::new();
        for key in due_keys {
            let Some(mut timer) = self.timers.remove(&key) else {
                continue;
            };
            timer.fired_count = timer.fired_count.saturating_add(1);
            let ctx_id = timer_ctx_id(&timer.timer_id, timer.fired_count);
            let mut prompt = format!("Timer `{}` fired: {}", timer.timer_id, timer.message);
            use std::fmt::Write as _;
            if let Some(interval) = timer.interval_seconds {
                let missed = missed_intervals(timer.next_fire_at, now, interval);
                if 1 < missed {
                    let _ = write!(
                        &mut prompt,
                        "\n\nCoalesced {missed} missed scheduled firings while Tau was unavailable."
                    );
                }
                timer.next_fire_at =
                    add_seconds(timer.next_fire_at, interval.saturating_mul(missed));
                self.timers.insert(key.clone(), timer.clone());
            }
            fires.push(FireRecord {
                agent_id: timer.agent_id,
                timer_id: timer.timer_id,
                prompt,
                ctx_id,
            });
        }
        fires
    }

    fn handle_started_replay(&mut self, started: &ToolStarted) {
        if started.tool_name.as_str() != TIMER_TOOL_NAME {
            return;
        }
        self.pending_invocations.insert(
            started.call_id.to_string(),
            PendingInvocation {
                agent_id: started.agent_id.clone(),
                call_id: started.call_id.to_string(),
                arguments: started.arguments.clone(),
            },
        );
    }

    fn handle_result_replay(&mut self, result: &ToolResult, now: UnixMicros) {
        if result.tool_name.as_str() != TIMER_TOOL_NAME || result.kind != ToolResultKind::Final {
            return;
        }
        let call_id = result.call_id.to_string();
        let Some(pending) = self.pending_invocations.remove(&call_id) else {
            return;
        };
        let _ = self.apply_successful_invocation(&pending, now);
    }

    fn handle_error_replay(&mut self, call_id: &str) {
        self.pending_invocations.remove(call_id);
    }

    fn handle_prompt_replay(
        &mut self,
        prompt: &AgentPromptSubmitted,
        recorded_at: Option<UnixMicros>,
    ) {
        let Some(ctx_id) = prompt.ctx_id.as_deref() else {
            return;
        };
        let Some((timer_id, fired_count)) = parse_timer_ctx_id(ctx_id) else {
            return;
        };
        let key = TimerKey {
            agent_id: prompt.agent_id.clone(),
            timer_id,
        };
        let Some(mut timer) = self.timers.remove(&key) else {
            return;
        };
        timer.fired_count = timer.fired_count.max(fired_count);
        if let Some(interval) = timer.interval_seconds {
            let base = recorded_at.unwrap_or(timer.next_fire_at);
            while timer.next_fire_at <= base {
                timer.next_fire_at = add_seconds(timer.next_fire_at, interval);
            }
            self.timers.insert(key, timer);
        }
    }

    fn complete_agent_replay(&mut self, done: &AgentReplayComplete) -> ClientResult<()> {
        if done.error.is_none() {
            self.replay_complete_agents.insert(done.agent_id.clone());
            self.fire_due_now()?;
        } else {
            self.drop_agent_restore_state(&done.agent_id);
        }
        Ok(())
    }

    fn clear_session_state(&mut self) {
        self.timers.clear();
        self.pending_invocations.clear();
        self.replay_complete_agents.clear();
    }

    fn unload_agent(&mut self, agent_id: &AgentId) {
        self.replay_complete_agents.remove(agent_id);
        self.pending_invocations
            .retain(|_, pending| &pending.agent_id != agent_id);
    }

    fn drop_agent_restore_state(&mut self, agent_id: &AgentId) {
        self.replay_complete_agents.remove(agent_id);
        self.pending_invocations
            .retain(|_, pending| &pending.agent_id != agent_id);
        self.timers.retain(|key, _| &key.agent_id != agent_id);
    }

    fn handle_live_tool(
        &mut self,
        invoke: &ToolStarted,
        now: UnixMicros,
    ) -> Result<TimerToolCompletion, String> {
        // A live tool call is delivered only after the connection catch-up phase
        // has released live traffic for this agent, so timers scheduled from it
        // may fire even if the harness did not emit a separate live-load replay
        // boundary for a brand-new agent.
        self.replay_complete_agents.insert(invoke.agent_id.clone());
        let pending = PendingInvocation {
            agent_id: invoke.agent_id.clone(),
            call_id: invoke.call_id.to_string(),
            arguments: invoke.arguments.clone(),
        };
        let action = parse_action(&pending.arguments, &pending.call_id)?;
        let display_args = timer_action_display_args(&action);
        let TimerActionResult { result, outcome } =
            self.apply_timer_action(&pending.agent_id, action, now)?;
        Ok(TimerToolCompletion {
            result,
            display: timer_success_display(display_args, outcome),
        })
    }

    fn apply_successful_invocation(
        &mut self,
        pending: &PendingInvocation,
        now: UnixMicros,
    ) -> Result<CborValue, String> {
        let action = parse_action(&pending.arguments, &pending.call_id)?;
        self.apply_timer_action(&pending.agent_id, action, now)
            .map(|result| result.result)
    }

    fn apply_timer_action(
        &mut self,
        agent_id: &AgentId,
        action: TimerAction,
        now: UnixMicros,
    ) -> Result<TimerActionResult, String> {
        match action {
            TimerAction::Schedule(args) => {
                self.schedule_timer(agent_id, args, now)
                    .map(|result| TimerActionResult {
                        result,
                        outcome: TimerActionOutcome::NoAdditionalContext,
                    })
            }
            TimerAction::Cancel { timer_id } => Ok(self.cancel_timer(agent_id, &timer_id)),
            TimerAction::List => Ok(self.list_timers(agent_id, now)),
        }
    }

    fn schedule_timer(
        &mut self,
        agent_id: &AgentId,
        args: ScheduleArgs,
        now: UnixMicros,
    ) -> Result<CborValue, String> {
        let replacing = self.timers.contains_key(&TimerKey {
            agent_id: agent_id.clone(),
            timer_id: args.timer_id.clone(),
        });
        if !replacing {
            if self.timers.len() >= MAX_TIMERS_TOTAL {
                return Err(format!(
                    "timer limit exceeded: at most {MAX_TIMERS_TOTAL} active timers per session"
                ));
            }
            let count = self
                .timers
                .values()
                .filter(|timer| &timer.agent_id == agent_id)
                .count();
            if MAX_TIMERS_PER_AGENT <= count {
                return Err(format!(
                    "timer limit exceeded: at most {MAX_TIMERS_PER_AGENT} active timers per agent"
                ));
            }
        } else {
            return Err(format!(
                "timer `{}` is already active; cancel it before scheduling a replacement",
                args.timer_id
            ));
        }
        let next_fire_at = add_seconds(now, args.delay_seconds);
        let entry = TimerEntry {
            agent_id: agent_id.clone(),
            timer_id: args.timer_id.clone(),
            next_fire_at,
            interval_seconds: args.interval_seconds,
            message: args.message,
            fired_count: 0,
        };
        self.timers.insert(
            TimerKey {
                agent_id: agent_id.clone(),
                timer_id: args.timer_id.clone(),
            },
            entry,
        );
        Ok(text_result(format!("scheduled timer `{}`", args.timer_id)))
    }

    fn cancel_timer(&mut self, agent_id: &AgentId, timer_id: &str) -> TimerActionResult {
        let removed = self
            .timers
            .remove(&TimerKey {
                agent_id: agent_id.clone(),
                timer_id: timer_id.to_owned(),
            })
            .is_some();
        if removed {
            TimerActionResult {
                result: text_result(format!("cancelled timer `{timer_id}`")),
                outcome: TimerActionOutcome::NoAdditionalContext,
            }
        } else {
            TimerActionResult {
                result: text_result(format!("timer `{timer_id}` was not active")),
                outcome: TimerActionOutcome::NotActive,
            }
        }
    }

    fn list_timers(&self, agent_id: &AgentId, now: UnixMicros) -> TimerActionResult {
        let mut timers: Vec<_> = self
            .timers
            .values()
            .filter(|timer| &timer.agent_id == agent_id)
            .collect();
        timers.sort_by_key(|timer| (timer.next_fire_at.get(), timer.timer_id.clone()));
        let lines: Vec<String> = timers
            .into_iter()
            .take(MAX_LIST_TIMERS)
            .map(|timer| {
                let due = duration_until(timer.next_fire_at, now).as_secs();
                match timer.interval_seconds {
                    Some(interval) => format!(
                        "{}: due in {}s, repeats every {}s",
                        timer.timer_id, due, interval
                    ),
                    None => format!("{}: due in {}s, one-shot", timer.timer_id, due),
                }
            })
            .collect();
        let matches = u64::try_from(lines.len())
            .expect("the bounded timer list contains no more than 64 entries");
        if lines.is_empty() {
            TimerActionResult {
                result: text_result("no active timers".to_owned()),
                outcome: TimerActionOutcome::Listed { matches },
            }
        } else {
            TimerActionResult {
                result: text_result(lines.join("\n")),
                outcome: TimerActionOutcome::Listed { matches },
            }
        }
    }
}

fn finish_with_error(
    runtime: ManualExtensionRuntime<TimerRuntime>,
    error: tau_client::ClientError,
) -> ClientResult<()> {
    let finish_result = runtime.finish();
    match finish_result {
        Ok(_) => Err(error),
        Err(finish_error) => Err(finish_error),
    }
}

enum LoopInput {
    Message(tau_proto::HarnessOutputMessage),
    Timeout,
    InputClosed,
}

fn recv_next(runtime: &mut ManualExtensionRuntime<TimerRuntime>) -> ClientResult<LoopInput> {
    match runtime.recv_timeout(runtime.state().next_deadline_duration())? {
        ManualRuntimeInput::Message(message) => Ok(LoopInput::Message(message)),
        ManualRuntimeInput::Timeout => Ok(LoopInput::Timeout),
        ManualRuntimeInput::InputClosed => Ok(LoopInput::InputClosed),
    }
}

fn handle_timer_tool(cx: ToolContext<'_, TimerRuntime>) -> ClientResult<()> {
    let now = UnixMicros::now();
    let result = cx.state.handle_live_tool(cx.invoke, now);
    let display_args = timer_display_args(&cx.invoke.arguments, cx.invoke.call_id.as_str());
    match result {
        Ok(completion) => cx.report_result(ToolResult {
            call_id: cx.invoke.call_id.clone(),
            tool_name: cx.invoke.tool_name.clone(),
            tool_type: ToolType::Function,
            result: completion.result,
            provider_content: Vec::new(),
            kind: ToolResultKind::Final,
            display: Some(completion.display),
            originator: cx.invoke.originator.clone(),
        }),
        Err(message) => cx.report_error(ToolError {
            call_id: cx.invoke.call_id.clone(),
            tool_name: cx.invoke.tool_name.clone(),
            tool_type: ToolType::Function,
            message,
            details: None,
            display: Some(error_display(display_args)),
            originator: cx.invoke.originator.clone(),
        }),
    }
}

fn handle_replay_tool_started(
    cx: tau_client::EventContext<'_, TimerRuntime, ToolStarted>,
) -> ClientResult<()> {
    cx.state.handle_started_replay(cx.event);
    Ok(())
}

fn handle_replay_terminal_result(cx: RawEventContext<'_, TimerRuntime>) -> ClientResult<()> {
    let result = match cx.event() {
        Event::ToolResult(result) | Event::ProviderToolResult(result) => result.clone(),
        _ => return Ok(()),
    };
    let recorded_at = cx.recorded_at().unwrap_or_else(UnixMicros::now);
    cx.state.handle_result_replay(&result, recorded_at);
    Ok(())
}

fn handle_replay_terminal_error(cx: RawEventContext<'_, TimerRuntime>) -> ClientResult<()> {
    let call_id = match cx.event() {
        Event::ToolError(error) | Event::ProviderToolError(error) => error.call_id.to_string(),
        _ => return Ok(()),
    };
    cx.state.handle_error_replay(&call_id);
    Ok(())
}

fn handle_replay_prompt_submitted(
    cx: tau_client::EventContext<'_, TimerRuntime, AgentPromptSubmitted>,
) -> ClientResult<()> {
    cx.state.handle_prompt_replay(cx.event, cx.recorded_at);
    Ok(())
}

fn handle_replay_prompt_steered(
    cx: tau_client::EventContext<'_, TimerRuntime, AgentPromptSteered>,
) -> ClientResult<()> {
    let submitted = AgentPromptSubmitted {
        inference_activation: false,
        agent_id: cx.event.agent_id.clone(),
        text: cx.event.text.clone(),
        message_class: cx.event.message_class,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: Default::default(),
        display_name: None,
        ctx_id: cx.event.ctx_id.clone(),
    };
    cx.state.handle_prompt_replay(&submitted, cx.recorded_at);
    Ok(())
}

fn handle_agent_replay_complete(
    cx: tau_client::EventContext<'_, TimerRuntime, AgentReplayComplete>,
) -> ClientResult<()> {
    cx.state.complete_agent_replay(cx.event)
}

fn handle_session_started(
    cx: tau_client::EventContext<'_, TimerRuntime, SessionStarted>,
) -> ClientResult<()> {
    let _ = cx.event;
    cx.state.clear_session_state();
    Ok(())
}

fn handle_session_shutdown(
    cx: tau_client::EventContext<'_, TimerRuntime, SessionShutdown>,
) -> ClientResult<()> {
    let _ = cx.event;
    cx.state.clear_session_state();
    Ok(())
}

fn handle_session_agent_unloaded(
    cx: tau_client::EventContext<'_, TimerRuntime, SessionAgentUnloaded>,
) -> ClientResult<()> {
    cx.state.unload_agent(&cx.event.agent_id);
    Ok(())
}

fn timer_display_args(arguments: &CborValue, call_id: &str) -> String {
    match parse_action(arguments, call_id) {
        Ok(action) => timer_action_display_args(&action),
        Err(_) => fallback_timer_display_args(arguments),
    }
}

fn timer_action_display_args(action: &TimerAction) -> String {
    match action {
        TimerAction::Schedule(args) => schedule_display_args(
            Some(args.timer_id.as_str()),
            Some(args.delay_seconds),
            args.interval_seconds,
        ),
        TimerAction::Cancel { timer_id } => format!("cancel {timer_id}"),
        TimerAction::List => "list".to_owned(),
    }
}

fn timer_success_display(args: String, outcome: TimerActionOutcome) -> ToolUseState {
    let mut display = ok_display(args);
    match outcome {
        TimerActionOutcome::NoAdditionalContext => {}
        TimerActionOutcome::NotActive => display.info_chips.push("not active".to_owned()),
        TimerActionOutcome::Listed { matches } => {
            display.stats = ToolUseStats {
                matches: Some(matches),
                ..Default::default()
            };
        }
    }
    display
}

fn fallback_timer_display_args(arguments: &CborValue) -> String {
    let Some(action) = tau_proto::cbor_text_field(arguments, "action") else {
        return String::new();
    };
    match action.as_str() {
        "schedule" => schedule_display_args(
            sanitized_display_timer_id(arguments).as_deref(),
            display_seconds_field(arguments, "delay_seconds"),
            display_seconds_field(arguments, "interval_seconds"),
        ),
        "cancel" => sanitized_display_timer_id(arguments)
            .map(|timer_id| format!("cancel {timer_id}"))
            .unwrap_or_else(|| "cancel".to_owned()),
        "list" => "list".to_owned(),
        _ => String::new(),
    }
}

fn schedule_display_args(
    timer_id: Option<&str>,
    delay_seconds: Option<u64>,
    interval_seconds: Option<u64>,
) -> String {
    let mut parts = vec!["schedule".to_owned()];
    if let Some(timer_id) = timer_id {
        parts.push(timer_id.to_owned());
    }
    if let Some(delay) = delay_seconds {
        parts.push(format!("in {}", format_seconds(delay)));
    }
    if let Some(interval) = interval_seconds {
        parts.push(format!("every {}", format_seconds(interval)));
    }
    parts.join(" ")
}

fn sanitized_display_timer_id(arguments: &CborValue) -> Option<String> {
    let timer_id = tau_proto::cbor_text_field(arguments, "timer_id")?;
    validate_timer_id(&timer_id).ok()
}

fn display_seconds_field(arguments: &CborValue, key: &str) -> Option<u64> {
    let raw = tau_proto::cbor_int_field(arguments, key)?;
    let seconds = u64::try_from(raw).ok()?;
    (seconds <= MAX_DELAY_SECONDS).then_some(seconds)
}

fn format_seconds(seconds: u64) -> String {
    if seconds != 0 && seconds.is_multiple_of(3600) {
        format!("{}h", seconds / 3600)
    } else if seconds != 0 && seconds.is_multiple_of(60) {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

fn ok_display(args: String) -> ToolUseState {
    // Keep successful tool-result metadata consistent with the shared short `ok`.
    ToolUseState {
        args,
        status: ToolUseStatus::Success,
        status_text: "ok".to_owned(),
        ..Default::default()
    }
}

fn error_display(args: String) -> ToolUseState {
    ToolUseState {
        args,
        status: ToolUseStatus::Error,
        status_text: "error".to_owned(),
        ..Default::default()
    }
}

fn timer_tool_spec() -> ToolSpec {
    ToolSpec {
        name: tau_proto::ToolName::new(TIMER_TOOL_NAME),
        model_visible_name: None,
        description: Some("Schedule, cancel, and list session-scoped timer reminders. Timers wake the agent with internal prompts.".to_owned()),
        tool_type: ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["schedule", "cancel", "list"]},
                "timer_id": {"type": "string", "maxLength": MAX_TIMER_ID_BYTES, "pattern": "^[A-Za-z0-9_-]{1,64}$", "description": "Path-safe id. Required for cancel; optional for schedule."},
                "delay_seconds": {"type": "integer", "minimum": MIN_DELAY_SECONDS, "maximum": MAX_DELAY_SECONDS},
                "interval_seconds": {"type": "integer", "minimum": MIN_INTERVAL_SECONDS, "maximum": MAX_DELAY_SECONDS},
                // Keep the byte limit in runtime validation: JSON Schema
                // maxLength counts characters, and large values also produce
                // grammar repetitions that llama.cpp refuses to parse.
                "message": {"type": "string", "description": format!("Reminder text; maximum {MAX_MESSAGE_BYTES} bytes.")}
            },
            "required": ["action"],
            "additionalProperties": false
        })),
        format: None,
        tags: vec![],
        enabled_by_default: true,
        background_support: None,
        examples: vec![],
    }
}

fn parse_action(value: &CborValue, call_id: &str) -> Result<TimerAction, String> {
    let action = tau_proto::cbor_text_field(value, "action")
        .ok_or_else(|| "timer action is required".to_owned())?;
    match action.as_str() {
        "schedule" => {
            let timer_id = match tau_proto::cbor_text_field(value, "timer_id") {
                Some(id) => validate_timer_id(&id)?,
                None => generated_timer_id(call_id),
            };
            let delay_seconds =
                bounded_seconds(value, "delay_seconds", MIN_DELAY_SECONDS, MAX_DELAY_SECONDS)?;
            let interval_seconds = match tau_proto::cbor_int_field(value, "interval_seconds") {
                Some(_) => Some(bounded_seconds(
                    value,
                    "interval_seconds",
                    MIN_INTERVAL_SECONDS,
                    MAX_DELAY_SECONDS,
                )?),
                None => None,
            };
            let message = tau_proto::cbor_text_field(value, "message")
                .ok_or_else(|| "message is required for schedule".to_owned())?;
            if message.is_empty() || message.len() > MAX_MESSAGE_BYTES {
                return Err(format!("message must be 1..={MAX_MESSAGE_BYTES} bytes"));
            }
            Ok(TimerAction::Schedule(ScheduleArgs {
                timer_id,
                delay_seconds,
                interval_seconds,
                message,
            }))
        }
        "cancel" => {
            let id = tau_proto::cbor_text_field(value, "timer_id")
                .ok_or_else(|| "timer_id is required for cancel".to_owned())?;
            Ok(TimerAction::Cancel {
                timer_id: validate_timer_id(&id)?,
            })
        }
        "list" => Ok(TimerAction::List),
        other => Err(format!("unsupported timer action `{other}`")),
    }
}

fn bounded_seconds(value: &CborValue, key: &str, min: u64, max: u64) -> Result<u64, String> {
    let raw = tau_proto::cbor_int_field(value, key).ok_or_else(|| format!("{key} is required"))?;
    let seconds = u64::try_from(raw).map_err(|_| format!("{key} must be a positive integer"))?;
    if !(min..=max).contains(&seconds) {
        return Err(format!("{key} must be between {min} and {max}"));
    }
    Ok(seconds)
}

fn validate_timer_id(id: &str) -> Result<String, String> {
    if id.is_empty() || id.len() > MAX_TIMER_ID_BYTES {
        return Err(format!("timer_id must be 1..={MAX_TIMER_ID_BYTES} bytes"));
    }
    if !id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err("timer_id must contain only ASCII letters, digits, '_' or '-'".to_owned());
    }
    Ok(id.to_owned())
}

fn generated_timer_id(call_id: &str) -> String {
    let mut id = String::from("call-");
    for b in call_id.bytes() {
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' {
            id.push(char::from(b));
        } else {
            id.push('-');
        }
        if id.len() >= MAX_TIMER_ID_BYTES {
            break;
        }
    }
    id
}

fn text_result(text: String) -> CborValue {
    CborValue::Text(text)
}

fn timer_ctx_id(timer_id: &str, fired_count: u64) -> String {
    format!("timer:{timer_id}:{fired_count}")
}

fn parse_timer_ctx_id(ctx_id: &str) -> Option<(String, u64)> {
    let rest = ctx_id.strip_prefix("timer:")?;
    let (timer_id, count) = rest.rsplit_once(':')?;
    Some((timer_id.to_owned(), count.parse().ok()?))
}

fn add_seconds(base: UnixMicros, seconds: u64) -> UnixMicros {
    let add = seconds.saturating_mul(1_000_000);
    UnixMicros::new(base.get().saturating_add(add))
}

fn duration_until(deadline: UnixMicros, now: UnixMicros) -> Duration {
    if deadline <= now {
        Duration::ZERO
    } else {
        Duration::from_micros(deadline.get() - now.get())
    }
}

fn missed_intervals(next_fire_at: UnixMicros, now: UnixMicros, interval_seconds: u64) -> u64 {
    if now <= next_fire_at {
        return 1;
    }
    let overdue_micros = now.get() - next_fire_at.get();
    overdue_micros / interval_seconds.saturating_mul(1_000_000) + 1
}
