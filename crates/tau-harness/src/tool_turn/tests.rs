use tau_proto::{BackgroundSupport, CborValue};

use super::*;

fn cid(value: &str) -> AgentId {
    crate::parse_agent_id(value)
}

fn call(id: &str) -> AgentToolCall {
    AgentToolCall {
        id: id.into(),
        name: ToolName::new("tool"),
        tool_type: ToolType::Function,
        arguments: CborValue::Null,
    }
}

fn push(machine: &mut ToolTurnMachine, cid: &AgentId, id: &str) {
    machine.push(cid.clone(), call(id), BackgroundSupport::Never);
}

fn pop_id(machine: &mut ToolTurnMachine) -> Option<String> {
    machine
        .pop_dispatchable(Instant::now())
        .map(|(pending, _)| pending.invocation.id.as_str().to_owned())
}

#[test]
fn queued_calls_dispatch_in_provider_order_without_global_locking() {
    let mut machine = ToolTurnMachine::default();
    let conv = cid("conv");
    push(&mut machine, &conv, "first");
    push(&mut machine, &conv, "second");
    push(&mut machine, &conv, "third");

    assert_eq!(pop_id(&mut machine).as_deref(), Some("first"));
    assert_eq!(pop_id(&mut machine).as_deref(), Some("second"));
    assert_eq!(pop_id(&mut machine).as_deref(), Some("third"));
    assert_eq!(machine.pending_len(), 0);
    assert_eq!(machine.in_flight_len(), 3);
}

#[test]
fn conversation_predicates_report_pending_and_foreground_in_flight_work() {
    let mut machine = ToolTurnMachine::default();
    let conv = cid("conv");
    let other = cid("other");
    push(&mut machine, &conv, "shared");

    assert!(machine.any_pending_for(&conv));
    assert!(!machine.any_pending_for(&other));
    assert!(!machine.any_in_flight_for(&conv));

    assert_eq!(pop_id(&mut machine).as_deref(), Some("shared"));
    assert!(!machine.any_pending_for(&conv));
    assert!(machine.any_in_flight_for(&conv));
    assert!(!machine.any_in_flight_for(&other));
}

/// Instant background support asks the harness to close the foreground at
/// dispatch time while keeping the actual tool call tracked until its real
/// result arrives.
#[test]
fn instant_background_completes_foreground_but_remains_running() {
    let mut machine = ToolTurnMachine::default();
    let conv = cid("conv");
    machine.push(conv.clone(), call("bg"), BackgroundSupport::Instant);

    let (pending, action) = machine.pop_dispatchable(Instant::now()).expect("dispatch");
    assert_eq!(pending.invocation.id.as_str(), "bg");
    assert_eq!(
        action,
        ForegroundAction::Background {
            call_id: "bg".into()
        }
    );
    assert!(!machine.is_backgrounded(&"bg".into()));
    assert!(machine.any_in_flight_for(&conv));
    assert!(machine.begin_backgrounding(&"bg".into()));
    assert!(!machine.begin_backgrounding(&"bg".into()));
    assert!(machine.any_in_flight_for(&conv));
    assert!(machine.mark_backgrounded(&"bg".into()));
    assert!(machine.is_backgrounded(&"bg".into()));
    assert!(!machine.any_in_flight_for(&conv));
    assert_eq!(machine.in_flight_len(), 1);
}

/// MinForegroundSeconds uses the dispatch instant as the start time. The
/// harness event loop can sleep until `next_background_deadline` instead of
/// polling.
#[test]
fn min_foreground_deadline_backgrounds_once_when_due() {
    let mut machine = ToolTurnMachine::default();
    let conv = cid("conv");
    let start = Instant::now();
    machine.push(
        conv,
        call("slow"),
        BackgroundSupport::MinForegroundSeconds(5),
    );
    let (_, action) = machine.pop_dispatchable(start).expect("dispatch");
    assert_eq!(action, ForegroundAction::None);
    assert_eq!(
        machine.background_due(start + std::time::Duration::from_secs(4)),
        Vec::<ToolCallId>::new()
    );

    assert_eq!(
        machine.background_due(start + std::time::Duration::from_secs(5)),
        vec![ToolCallId::from("slow")]
    );
    assert!(machine.begin_backgrounding(&"slow".into()));
    assert_eq!(
        machine.background_due(start + std::time::Duration::from_secs(6)),
        Vec::<ToolCallId>::new()
    );
    assert!(machine.mark_backgrounded(&"slow".into()));
    assert!(machine.is_backgrounded(&"slow".into()));
}

/// Never preserves old foreground behavior: no deadline is armed, but it no
/// longer participates in harness-side tool locking.
#[test]
fn never_background_has_no_deadline_and_does_not_block_dispatch() {
    let mut machine = ToolTurnMachine::default();
    let conv = cid("conv");
    machine.push(conv.clone(), call("never"), BackgroundSupport::Never);
    machine.push(conv, call("behind"), BackgroundSupport::Never);
    let (_, action) = machine.pop_dispatchable(Instant::now()).expect("dispatch");
    assert_eq!(action, ForegroundAction::None);
    assert!(machine.next_background_deadline().is_none());
    assert_eq!(pop_id(&mut machine).as_deref(), Some("behind"));
}

/// A late real result removes actual-running state exactly once after the
/// foreground was already closed by the synthetic background placeholder.
#[test]
fn late_background_completion_clears_actual_running_once() {
    let mut machine = ToolTurnMachine::default();
    let conv = cid("conv");
    machine.push(conv, call("late"), BackgroundSupport::Instant);
    machine.pop_dispatchable(Instant::now()).expect("dispatch");
    assert!(machine.begin_backgrounding(&"late".into()));
    assert!(machine.mark_backgrounded(&"late".into()));
    assert!(machine.is_backgrounded(&"late".into()));

    assert!(machine.mark_complete(&"late".into()));
    assert!(!machine.mark_complete(&"late".into()));
    assert!(!machine.is_backgrounded(&"late".into()));
}
