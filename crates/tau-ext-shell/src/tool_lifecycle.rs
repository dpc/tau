//! Bounded lifecycle state for admitted model tool calls.

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak, mpsc};

use tau_proto::{AgentId, Event, ToolCallId, ToolCancelled, ToolName, ToolType};

use crate::Output;

/// Shared cancellation state for pre-effect and actively cancellable model
/// calls.
#[derive(Clone, Default)]
pub(crate) struct ToolCancellationState {
    /// Bounded lifecycle authority for every admitted scheduled model call.
    pub(crate) lifecycles: ToolLifecycleRegistry,
    /// Cancellation senders for shell and search effects currently executing.
    pub(crate) running_calls: Arc<Mutex<HashMap<ToolCallId, mpsc::Sender<()>>>>,
}

/// Registry that keeps cancellation authoritative across scheduler and lock
/// handoffs.
#[derive(Clone, Default)]
pub(crate) struct ToolLifecycleRegistry {
    /// Live calls indexed by their harness call identifier.
    inner: Arc<Mutex<HashMap<ToolCallId, Arc<Entry>>>>,
    #[cfg(test)]
    /// Deterministic handoff barriers installed by focused race tests.
    hooks: Arc<Mutex<TestHooks>>,
}

/// One admitted call's shared lifecycle authority.
#[derive(Clone)]
pub(crate) struct ToolLifecycle {
    /// State shared with cancellation processing.
    entry: Arc<Entry>,
}

/// Result of processing a cancellation against a live lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CancelOutcome {
    /// Cancellation won before the call crossed the effect-start boundary.
    PreventedEffect,
    /// The effect had started, so existing active cancellation must be
    /// signalled.
    EffectStarted,
}

/// Shared state and report metadata for one admitted model call.
struct Entry {
    /// Call identifier used for registry cleanup and cancellation reports.
    call_id: ToolCallId,
    /// Local tool name; the scoped output maps it back to the wire name.
    tool_name: ToolName,
    /// Agent that owns the admitted call.
    agent_id: AgentId,
    /// Output scope captured when the call was admitted.
    tx: Output,
    /// Registry owning this entry while the call remains live.
    registry: Weak<Mutex<HashMap<ToolCallId, Arc<Entry>>>>,
    /// Atomic lifecycle winner protected by one short critical section.
    state: Mutex<State>,
    #[cfg(test)]
    /// Registry-scoped deterministic handoff barriers.
    hooks: Arc<Mutex<TestHooks>>,
}

#[derive(Clone, Copy)]
enum State {
    /// The call has not started an externally visible effect.
    BeforeEffect,
    /// The effect started; the flag remembers cancellation during sender
    /// handoff.
    EffectStarted { cancel_requested: bool },
    /// A terminal path won; successful publication or shutdown removes the
    /// registry entry.
    Terminal,
}

#[cfg(test)]
#[derive(Default)]
/// Registry-scoped deterministic barriers used only by lifecycle race tests.
struct TestHooks {
    /// Barrier after scheduler dequeue and before ordinary dispatch.
    after_dequeue: Option<TestHandoff>,
    /// Barrier after lock acquisition and before effect start.
    after_lock: Option<TestHandoff>,
    /// Barrier after effect start and before active sender registration.
    before_active_registration: Option<TestHandoff>,
    /// Barrier after effect start and before direct lock-waiter registration.
    before_lock_waiter_registration: Option<TestHandoff>,
}

#[cfg(test)]
/// One deterministic worker-to-test handoff barrier.
struct TestHandoff {
    /// Notification that the worker reached the handoff.
    reached: mpsc::SyncSender<()>,
    /// Permission for the worker to leave the handoff.
    resume: mpsc::Receiver<()>,
}

impl ToolLifecycleRegistry {
    /// Admit one scheduled model call into the bounded live-call registry.
    pub(crate) fn admit(
        &self,
        call_id: ToolCallId,
        tool_name: ToolName,
        agent_id: AgentId,
        tx: Output,
    ) -> ToolLifecycle {
        let entry = Arc::new(Entry {
            call_id: call_id.clone(),
            tool_name,
            agent_id,
            tx,
            registry: Arc::downgrade(&self.inner),
            state: Mutex::new(State::BeforeEffect),
            #[cfg(test)]
            hooks: Arc::clone(&self.hooks),
        });
        self.inner
            .lock()
            .expect("tool lifecycle registry lock poisoned")
            .insert(call_id, Arc::clone(&entry));
        ToolLifecycle { entry }
    }

    /// Install a deterministic scheduler-dequeue handoff barrier for one test.
    #[cfg(test)]
    pub(crate) fn pause_after_dequeue(
        &self,
        reached: mpsc::SyncSender<()>,
        resume: mpsc::Receiver<()>,
    ) {
        self.hooks
            .lock()
            .expect("tool lifecycle test hooks poisoned")
            .after_dequeue = Some(TestHandoff { reached, resume });
    }

    /// Install a deterministic lock-acquired handoff barrier for one test.
    #[cfg(test)]
    pub(crate) fn pause_after_lock(
        &self,
        reached: mpsc::SyncSender<()>,
        resume: mpsc::Receiver<()>,
    ) {
        self.hooks
            .lock()
            .expect("tool lifecycle test hooks poisoned")
            .after_lock = Some(TestHandoff { reached, resume });
    }

    /// Install a deterministic active-sender registration barrier for one test.
    #[cfg(test)]
    pub(crate) fn pause_before_active_registration(
        &self,
        reached: mpsc::SyncSender<()>,
        resume: mpsc::Receiver<()>,
    ) {
        self.hooks
            .lock()
            .expect("tool lifecycle test hooks poisoned")
            .before_active_registration = Some(TestHandoff { reached, resume });
    }

    /// Install a deterministic direct lock-waiter registration barrier.
    #[cfg(test)]
    pub(crate) fn pause_before_lock_waiter_registration(
        &self,
        reached: mpsc::SyncSender<()>,
        resume: mpsc::Receiver<()>,
    ) {
        self.hooks
            .lock()
            .expect("tool lifecycle test hooks poisoned")
            .before_lock_waiter_registration = Some(TestHandoff { reached, resume });
    }

    /// Process cancellation against the call's single lifecycle authority.
    pub(crate) fn cancel(&self, call_id: &ToolCallId) -> Option<CancelOutcome> {
        let entry = self
            .inner
            .lock()
            .expect("tool lifecycle registry lock poisoned")
            .get(call_id)
            .cloned()?;
        let outcome = {
            let mut state = entry.state.lock().expect("tool lifecycle state poisoned");
            match *state {
                State::BeforeEffect => {
                    *state = State::Terminal;
                    CancelOutcome::PreventedEffect
                }
                State::EffectStarted { .. } => {
                    *state = State::EffectStarted {
                        cancel_requested: true,
                    };
                    CancelOutcome::EffectStarted
                }
                State::Terminal => return None,
            }
        };
        if outcome == CancelOutcome::PreventedEffect
            && entry
                .tx
                .report_tool_terminal(Event::ToolCancelled(ToolCancelled {
                    presentation: Default::default(),
                    call_id: entry.call_id.clone(),
                    tool_name: entry.tool_name.clone(),
                    tool_type: ToolType::Function,
                }))
                .is_ok()
        {
            entry.remove_from_registry();
        }
        Some(outcome)
    }

    /// Prevent queued or pre-effect work owned by an agent that is leaving.
    ///
    /// Active work retains its entry until its ordinary terminal path because
    /// agent unload did not previously cancel already-running model tools.
    pub(crate) fn remove_agent(&self, agent_id: &AgentId) {
        let entries = {
            self.inner
                .lock()
                .expect("tool lifecycle registry lock poisoned")
                .values()
                .filter(|entry| &entry.agent_id == agent_id)
                .cloned()
                .collect::<Vec<_>>()
        };
        let mut removed = Vec::new();
        for entry in entries {
            let mut state = entry.state.lock().expect("tool lifecycle state poisoned");
            if matches!(*state, State::BeforeEffect) {
                *state = State::Terminal;
                drop(state);
                removed.push(entry);
            }
        }
        let mut registry = self
            .inner
            .lock()
            .expect("tool lifecycle registry lock poisoned");
        registry.retain(|_, entry| !removed.iter().any(|removed| Arc::ptr_eq(removed, entry)));
    }
    /// Stop pre-effect calls and preserve cancellation across active sender
    /// handoff.
    pub(crate) fn prepare_shutdown(&self) {
        let entries = {
            self.inner
                .lock()
                .expect("tool lifecycle registry lock poisoned")
                .values()
                .cloned()
                .collect::<Vec<_>>()
        };
        for entry in entries {
            let remove = {
                let mut state = entry.state.lock().expect("tool lifecycle state poisoned");
                match *state {
                    State::BeforeEffect => {
                        *state = State::Terminal;
                        true
                    }
                    State::EffectStarted { .. } => {
                        *state = State::EffectStarted {
                            cancel_requested: true,
                        };
                        false
                    }
                    State::Terminal => true,
                }
            };
            if remove {
                entry.remove_from_registry();
            }
        }
    }
}

impl ToolLifecycle {
    /// Report cancellation if this call is still before effect start.
    ///
    /// A prior explicit cancellation or shutdown has already made the state
    /// terminal, so this method suppresses duplicate reports on those paths.
    pub(crate) fn report_cancelled_before_effect(&self) {
        let should_report = {
            let mut state = self
                .entry
                .state
                .lock()
                .expect("tool lifecycle state poisoned");
            if matches!(*state, State::BeforeEffect) {
                *state = State::Terminal;
                true
            } else {
                false
            }
        };
        if should_report
            && self
                .entry
                .tx
                .report_tool_terminal(Event::ToolCancelled(ToolCancelled {
                    presentation: Default::default(),
                    call_id: self.entry.call_id.clone(),
                    tool_name: self.entry.tool_name.clone(),
                    tool_type: ToolType::Function,
                }))
                .is_ok()
        {
            self.entry.remove_from_registry();
        }
    }

    /// Claim a terminal error before effect start, unless cancellation won
    /// first.
    pub(crate) fn claim_terminal_before_effect(&self) -> bool {
        {
            let mut state = self
                .entry
                .state
                .lock()
                .expect("tool lifecycle state poisoned");
            match *state {
                State::BeforeEffect => {
                    *state = State::Terminal;
                    true
                }
                State::EffectStarted { .. } => {
                    // ast-grep-ignore: debug-assert-expression-must-not-mutate
                    debug_assert!(false, "pre-effect terminal claimed after effect start");
                    false
                }
                State::Terminal => false,
            }
        }
    }

    /// Pause at the scheduler-dequeue handoff when a focused test installed a
    /// barrier.
    #[cfg(test)]
    pub(crate) fn test_pause_after_dequeue(&self) {
        self.entry
            .pause_test_handoff(TestHandoffPoint::AfterDequeue);
    }

    /// Pause at the lock-acquired handoff when a focused test installed a
    /// barrier.
    #[cfg(test)]
    pub(crate) fn test_pause_after_lock(&self) {
        self.entry.pause_test_handoff(TestHandoffPoint::AfterLock);
    }

    /// Pause before active sender registration when a focused test installed a
    /// barrier.
    #[cfg(test)]
    pub(crate) fn test_pause_before_active_registration(&self) {
        self.entry
            .pause_test_handoff(TestHandoffPoint::BeforeActiveRegistration);
    }

    /// Pause before direct lock-waiter registration when a focused test
    /// installed a barrier.
    #[cfg(test)]
    pub(crate) fn test_pause_before_lock_waiter_registration(&self) {
        self.entry
            .pause_test_handoff(TestHandoffPoint::BeforeLockWaiterRegistration);
    }

    /// Atomically cross the effect-start boundary if cancellation has not won.
    pub(crate) fn start_effect(&self) -> bool {
        let mut state = self
            .entry
            .state
            .lock()
            .expect("tool lifecycle state poisoned");
        match *state {
            State::BeforeEffect => {
                *state = State::EffectStarted {
                    cancel_requested: false,
                };
                true
            }
            State::EffectStarted { .. } => true,
            State::Terminal => false,
        }
    }

    /// Return whether cancellation arrived after effect start.
    pub(crate) fn effect_cancel_requested(&self) -> bool {
        matches!(
            *self
                .entry
                .state
                .lock()
                .expect("tool lifecycle state poisoned"),
            State::EffectStarted {
                cancel_requested: true
            }
        )
    }

    /// Mark the call terminal and remove its bounded registry entry.
    pub(crate) fn finish(&self) {
        *self
            .entry
            .state
            .lock()
            .expect("tool lifecycle state poisoned") = State::Terminal;
        self.entry.remove_from_registry();
    }
}

impl Entry {
    /// Take and execute one deterministic test handoff without holding hook
    /// state.
    #[cfg(test)]
    fn pause_test_handoff(&self, point: TestHandoffPoint) {
        let handoff = {
            let mut hooks = self
                .hooks
                .lock()
                .expect("tool lifecycle test hooks poisoned");
            match point {
                TestHandoffPoint::AfterDequeue => hooks.after_dequeue.take(),
                TestHandoffPoint::AfterLock => hooks.after_lock.take(),
                TestHandoffPoint::BeforeActiveRegistration => {
                    hooks.before_active_registration.take()
                }
                TestHandoffPoint::BeforeLockWaiterRegistration => {
                    hooks.before_lock_waiter_registration.take()
                }
            }
        };
        if let Some(handoff) = handoff {
            handoff.reached.send(()).expect("handoff observer");
            handoff.resume.recv().expect("handoff resume");
        }
    }

    /// Remove this exact admission without deleting a reused call identifier.
    fn remove_from_registry(self: &Arc<Self>) {
        let Some(registry) = self.registry.upgrade() else {
            return;
        };
        let mut registry = registry
            .lock()
            .expect("tool lifecycle registry lock poisoned");
        if registry
            .get(&self.call_id)
            .is_some_and(|entry| Arc::ptr_eq(entry, self))
        {
            registry.remove(&self.call_id);
        }
    }
}

#[cfg(test)]
enum TestHandoffPoint {
    /// Scheduler removed the call from its queue.
    AfterDequeue,
    /// Automatic directory-lock acquisition returned a held guard.
    AfterLock,
    /// Effect start won but the active cancellation sender is not registered.
    BeforeActiveRegistration,
    /// Effect start won but a direct directory-lock waiter is not registered.
    BeforeLockWaiterRegistration,
}
