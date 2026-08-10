use super::*;

fn report(status: &mut WorkStatus, phase: AgentWorkStatusPhase, title: &str) -> bool {
    status.report_at(
        WorkStatusReport::new(phase, title.to_owned()).expect("valid report"),
        Instant::now(),
        false,
    )
}

/// Runtime mutation rejects protocol-only phases and noncanonical titles in
/// release-effective validation.
#[test]
fn report_validation_rejects_non_model_phases_and_titles() {
    for phase in [
        AgentWorkStatusPhase::Unreported,
        AgentWorkStatusPhase::Unknown,
    ] {
        assert!(WorkStatusReport::new(phase, "invalid".to_owned()).is_err());
    }
    for title in ["", "two\nlines", "line\u{2028}separator"] {
        assert!(WorkStatusReport::new(AgentWorkStatusPhase::Working, title.to_owned()).is_err());
    }
}

/// Task work, accepted status, final refusal, and fresh activations keep
/// independent activation-scoped lifecycle state.
#[test]
fn start_status_lifecycle_is_scoped_to_genuine_activations() {
    let mut status = WorkStatus::default();
    assert!(!status.mark_start_reminder_delivered());
    status.begin_task_activation(tau_proto::ObservationId::random());
    status.record_task_work();
    assert!(status.mark_start_reminder_delivered());
    assert!(!status.mark_start_reminder_delivered());
    assert_eq!(
        status.decide_start_status_final(true),
        Some(StartStatusFinalDecision::Challenge)
    );
    status.record_start_status_final_challenge();
    assert_eq!(
        status.decide_start_status_final(true),
        Some(StartStatusFinalDecision::Fail)
    );
    assert!(report(
        &mut status,
        AgentWorkStatusPhase::Working,
        "first title"
    ));
    assert_eq!(status.decide_start_status_final(true), None);
    status.begin_task_activation(tau_proto::ObservationId::random());
    assert!(!status.mark_start_reminder_delivered());
    status.record_task_work();
    assert!(status.mark_start_reminder_delivered());
    status.retire_task_activation();
    assert_eq!(status.decide_start_status_final(true), None);
}

/// No-tool and lifecycle-only activations stay exempt, while accepted status
/// suppresses work admitted before or after it in the same parallel round.
#[test]
fn start_status_exemptions_and_parallel_suppression_are_order_independent() {
    let mut status = WorkStatus::default();
    status.begin_task_activation(tau_proto::ObservationId::random());
    assert_eq!(status.decide_start_status_final(true), None);

    assert!(report(
        &mut status,
        AgentWorkStatusPhase::Working,
        "parallel status first"
    ));
    status.record_task_work();
    assert!(!status.mark_start_reminder_delivered());
    assert_eq!(status.decide_start_status_final(true), None);

    status.begin_task_activation(tau_proto::ObservationId::random());
    status.record_task_work();
    assert!(report(
        &mut status,
        AgentWorkStatusPhase::Done,
        "parallel status last"
    ));
    assert!(!status.mark_start_reminder_delivered());
    assert_eq!(status.decide_start_status_final(true), None);
}

/// Qualifying work without an accepted status receives one reminder and the
/// bounded final refusal sequence.
#[test]
fn missing_accepted_status_keeps_start_status_enforcement() {
    let mut status = WorkStatus::default();
    status.begin_task_activation(tau_proto::ObservationId::random());
    status.record_task_work();
    assert!(status.mark_start_reminder_delivered());
    assert_eq!(
        status.decide_start_status_final(true),
        Some(StartStatusFinalDecision::Challenge)
    );
    status.record_start_status_final_challenge();
    assert_eq!(
        status.decide_start_status_final(true),
        Some(StartStatusFinalDecision::Fail)
    );
    assert_eq!(status.decide_start_status_final(false), None);
}

/// A status report rejected by release-effective validation never mutates the
/// activation gate or suppresses its reminder.
#[test]
fn rejected_status_report_cannot_suppress_start_status_enforcement() {
    let mut status = WorkStatus::default();
    status.begin_task_activation(tau_proto::ObservationId::random());
    status.record_task_work();
    let rejected = WorkStatusReport::new(AgentWorkStatusPhase::Unreported, "invalid".to_owned());
    assert!(rejected.is_err());
    assert!(status.mark_start_reminder_delivered());
    assert_eq!(
        status.decide_start_status_final(true),
        Some(StartStatusFinalDecision::Challenge)
    );
}

/// Queued addressed work cannot take ownership until the active foreground
/// round has made its own reminder decision.
#[test]
fn queued_activation_promotes_only_after_old_round_settles() {
    let mut status = WorkStatus::default();
    let old = tau_proto::ObservationId::random();
    let new = tau_proto::ObservationId::random();
    status.begin_task_activation(old);
    status.record_task_work();
    status.join_task_activation(new);
    assert!(status.mark_start_reminder_delivered());

    status.promote_pending_task_activation();
    assert!(!status.mark_start_reminder_delivered());
    status.record_task_work();
    assert!(status.mark_start_reminder_delivered());
}

/// Done and Blocked disable the Working final gate without changing the runtime
/// into another implicit Working epoch.
#[test]
fn terminal_reports_disable_only_the_working_gate() {
    for phase in [AgentWorkStatusPhase::Done, AgentWorkStatusPhase::Blocked] {
        let mut status = WorkStatus::default();
        assert!(report(&mut status, phase, "terminal report"));
        assert_eq!(status.phase(), phase);
        assert_eq!(status.decide_final(true), None);
        assert_eq!(status.epoch(), 0);
    }
}

/// Wait accounting uses the union of overlapping installed waits, pauses after
/// settlement, and resumes without resetting the current Working epoch.
#[test]
fn wait_accounting_accumulates_union_within_working_epoch() {
    let start = Instant::now();
    let mut status = WorkStatus::default();
    assert!(
        status.report_at(
            WorkStatusReport::new(AgentWorkStatusPhase::Working, "waiting".to_owned())
                .expect("valid report"),
            start,
            false,
        )
    );

    status.synchronize_wait_at(true, start + Duration::from_secs(5 * 60));
    status.synchronize_wait_at(true, start + Duration::from_secs(10 * 60));
    status.synchronize_wait_at(false, start + Duration::from_secs(15 * 60));
    assert_eq!(status.next_wait_deadline(), None);

    status.synchronize_wait_at(true, start + Duration::from_secs(20 * 60));
    assert_eq!(
        status.next_wait_deadline(),
        Some(start + Duration::from_secs(25 * 60))
    );
    assert_eq!(
        status.take_crossed_wait_threshold_at(start + Duration::from_secs(25 * 60)),
        Some(15)
    );
}

/// A new Working epoch resets accumulated duration and the crossed-threshold
/// cursor even when another wait remains installed.
#[test]
fn new_working_epoch_resets_wait_accounting() {
    let start = Instant::now();
    let mut status = WorkStatus::default();
    assert!(
        status.report_at(
            WorkStatusReport::new(AgentWorkStatusPhase::Working, "first".to_owned())
                .expect("valid report"),
            start,
            true,
        )
    );
    assert_eq!(
        status.take_crossed_wait_threshold_at(start + Duration::from_secs(15 * 60)),
        Some(15)
    );
    assert!(status.report_at(
        WorkStatusReport::new(AgentWorkStatusPhase::Done, "done".to_owned()).expect("valid report"),
        start + Duration::from_secs(16 * 60),
        true,
    ));
    assert!(
        status.report_at(
            WorkStatusReport::new(AgentWorkStatusPhase::Working, "second".to_owned())
                .expect("valid report"),
            start + Duration::from_secs(20 * 60),
            true,
        )
    );
    assert_eq!(
        status.next_wait_deadline(),
        Some(start + Duration::from_secs(35 * 60))
    );
}
