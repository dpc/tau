use std::time::{Duration, Instant};

use tau_config::settings::NotificationDeliveryPolicy;

use super::{DeliveryDeadlineKind, DeliverySchedule};

fn policy(idle_ms: u64, wait_any_ms: u64, wait_tool_ms: u64) -> NotificationDeliveryPolicy {
    NotificationDeliveryPolicy::from_millis(idle_ms, wait_any_ms, wait_tool_ms)
        .expect("valid policy")
}

/// State changes select immutable deadlines from the original admission cut.
#[test]
fn state_changes_do_not_reset_delivery_deadlines() {
    let admitted = Instant::now();
    let mut schedule = DeliverySchedule::new(admitted, policy(10, 20, 30)).expect("clock range");

    assert!(!schedule.mark_ready_at(
        DeliveryDeadlineKind::WaitTool,
        admitted + Duration::from_millis(20),
    ));
    assert!(schedule.mark_ready_at(
        DeliveryDeadlineKind::Idle,
        admitted + Duration::from_millis(20),
    ));
    assert!(schedule.is_ready_at(DeliveryDeadlineKind::WaitTool, admitted));
}

/// Equality is a valid deadline and readiness remains sticky afterward.
#[test]
fn equality_marks_delivery_ready_once() {
    let admitted = Instant::now();
    let mut schedule = DeliverySchedule::new(admitted, policy(5, 5, 5)).expect("clock range");
    let deadline = admitted + Duration::from_millis(5);

    assert!(schedule.mark_ready_at(DeliveryDeadlineKind::WaitAny, deadline));
    assert!(!schedule.mark_ready_at(DeliveryDeadlineKind::WaitTool, deadline));
    assert!(schedule.is_ready_at(DeliveryDeadlineKind::Idle, admitted));
}
