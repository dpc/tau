use super::*;

fn report(status: &mut WorkStatus, phase: AgentWorkStatusPhase, title: &str) -> bool {
    status.report_at(
        WorkStatusReport::new(phase, title.to_owned()).expect("valid report"),
        Instant::now(),
        false,
    )
}

fn final_input(successful: bool, status_was_available: bool) -> FinalStatusInput {
    FinalStatusInput {
        successful,
        status_was_available,
    }
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

/// Direct substantive admission while nonworking creates one reminder
/// obligation for the current foreground round.
#[test]
fn substantive_nonworking_tool_admission_requests_a_reminder() {
    let mut status = WorkStatus::default();
    assert!(!status.take_working_reminder());
    status.record_substantive_tool_admission();
    assert!(status.take_working_reminder());
    assert!(!status.take_working_reminder());
}

/// An accepted Working report suppresses the current round's reminder in either
/// parallel order, including an unchanged accepted report.
#[test]
fn parallel_working_report_suppresses_reminder_in_either_order() {
    let mut status = WorkStatus::default();
    assert!(report(
        &mut status,
        AgentWorkStatusPhase::Working,
        "parallel status first"
    ));
    status.record_substantive_tool_admission();
    assert!(!status.take_working_reminder());

    assert!(report(
        &mut status,
        AgentWorkStatusPhase::Done,
        "between rounds"
    ));
    status.record_substantive_tool_admission();
    assert!(report(
        &mut status,
        AgentWorkStatusPhase::Working,
        "parallel status last"
    ));
    assert!(!status.take_working_reminder());

    status.record_substantive_tool_admission();
    assert!(!report(
        &mut status,
        AgentWorkStatusPhase::Working,
        "parallel status last"
    ));
    assert!(!status.take_working_reminder());
}

/// Working persists across later inputs without per-activation acknowledgement;
/// Done and Blocked require a fresh Working transition before later tool work.
#[test]
fn current_phase_alone_controls_later_tool_reminders() {
    let mut status = WorkStatus::default();
    assert!(report(
        &mut status,
        AgentWorkStatusPhase::Working,
        "persistent work"
    ));
    for _ in 0..3 {
        status.record_substantive_tool_admission();
        assert!(!status.take_working_reminder());
    }
    for phase in [AgentWorkStatusPhase::Done, AgentWorkStatusPhase::Blocked] {
        assert!(report(&mut status, phase, "not currently working"));
        status.record_substantive_tool_admission();
        assert!(status.take_working_reminder());
    }
}

/// Rejected status input cannot clear a reminder obligation created by
/// substantive nonworking work.
#[test]
fn rejected_status_report_does_not_suppress_working_reminder() {
    let mut status = WorkStatus::default();
    status.record_substantive_tool_admission();
    assert!(WorkStatusReport::new(AgentWorkStatusPhase::Unreported, "invalid".to_owned()).is_err());
    assert!(status.take_working_reminder());
}

/// Working receives two successful-final challenges before the escape accepts
/// the third; unsuccessful terminals still take the invalidating accept path.
#[test]
fn working_final_gate_has_bounded_escape() {
    let mut status = WorkStatus::default();
    assert!(report(
        &mut status,
        AgentWorkStatusPhase::Working,
        "must finish explicitly"
    ));
    for _ in 0..2 {
        let Some(FinalStatusDecision::Challenge(challenge)) =
            status.decide_final(final_input(true, true))
        else {
            panic!("Working final must be challenged");
        };
        assert_eq!(
            challenge,
            FinalStatusChallenge::Working {
                title: "must finish explicitly".to_owned()
            }
        );
        status.record_final_challenge(&challenge);
    }
    assert_eq!(
        status.decide_final(final_input(true, true)),
        Some(FinalStatusDecision::Accept)
    );
    assert_eq!(
        status.decide_final(final_input(false, true)),
        Some(FinalStatusDecision::Accept)
    );
    assert!(status.invalidate_working());
    assert_eq!(status.decide_final(final_input(true, true)), None);
}

/// Unreported finals use the same two-challenge escape only when the immutable
/// prompt surface exposed status; status-less agents remain unaffected.
#[test]
fn unreported_final_gate_requires_status_tool_and_has_bounded_escape() {
    let mut status = WorkStatus::default();
    assert_eq!(status.decide_final(final_input(true, false)), None);
    assert_eq!(status.decide_final(final_input(false, true)), None);
    for _ in 0..2 {
        let Some(FinalStatusDecision::Challenge(challenge)) =
            status.decide_final(final_input(true, true))
        else {
            panic!("Unreported final must be challenged");
        };
        assert_eq!(challenge, FinalStatusChallenge::Unreported);
        status.record_final_challenge(&challenge);
    }
    assert_eq!(
        status.decide_final(final_input(true, true)),
        Some(FinalStatusDecision::Accept)
    );
    assert_eq!(status.phase(), AgentWorkStatusPhase::Unreported);
}

/// An accepted Unreported-to-Working transition starts a distinct Working epoch
/// with its own two-challenge budget.
#[test]
fn working_epoch_resets_unreported_final_challenge_budget() {
    let mut status = WorkStatus::default();
    for _ in 0..2 {
        let Some(FinalStatusDecision::Challenge(challenge)) =
            status.decide_final(final_input(true, true))
        else {
            panic!("Unreported final must be challenged");
        };
        status.record_final_challenge(&challenge);
    }
    assert!(report(
        &mut status,
        AgentWorkStatusPhase::Working,
        "new working epoch"
    ));
    for _ in 0..2 {
        let Some(FinalStatusDecision::Challenge(challenge)) =
            status.decide_final(final_input(true, true))
        else {
            panic!("Working final must receive a fresh challenge budget");
        };
        status.record_final_challenge(&challenge);
    }
    assert_eq!(
        status.decide_final(final_input(true, true)),
        Some(FinalStatusDecision::Accept)
    );
}

/// Done, Blocked, and Waiting disable final-status gating without changing the
/// runtime into an implicit Working epoch.
#[test]
fn terminal_reports_disable_final_status_gate() {
    for phase in [
        AgentWorkStatusPhase::Done,
        AgentWorkStatusPhase::Blocked,
        AgentWorkStatusPhase::Waiting,
    ] {
        let mut status = WorkStatus::default();
        assert!(report(&mut status, phase, "terminal report"));
        assert_eq!(status.phase(), phase);
        assert_eq!(status.decide_final(final_input(true, true)), None);
        assert_eq!(status.epoch(), 0);
    }
}

/// Three consecutive input-wait timeouts produce one advisory, while reported
/// Waiting and substantive progress suppress or reset the no-progress run.
#[test]
fn repeated_wait_guard_is_one_shot_and_progress_scoped() {
    let mut status = WorkStatus::default();
    assert!(!status.record_input_wait_timeout());
    assert!(!status.record_input_wait_timeout());
    assert!(status.record_input_wait_timeout());
    assert!(!status.record_input_wait_timeout());

    status.record_substantive_tool_admission();
    assert!(!status.record_input_wait_timeout());
    assert!(!status.record_input_wait_timeout());
    assert!(status.record_input_wait_timeout());

    assert!(report(
        &mut status,
        AgentWorkStatusPhase::Waiting,
        "awaiting review"
    ));
    for _ in 0..4 {
        assert!(!status.record_input_wait_timeout());
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
