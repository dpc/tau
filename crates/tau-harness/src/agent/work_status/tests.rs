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

/// Every effective snapshot gets a fresh acknowledgement decision, while a
/// changed-title Working report acknowledges that current snapshot.
#[test]
fn acknowledgement_tracks_effective_snapshots_and_changed_working_titles() {
    let mut status = WorkStatus::default();
    status.reset_ack_notice();
    assert!(status.mark_ack_notice_delivered());
    status.reset_ack_notice();
    assert!(status.mark_ack_notice_delivered());
    status.reset_ack_notice();
    assert!(report(
        &mut status,
        AgentWorkStatusPhase::Working,
        "first title"
    ));
    status.reset_ack_notice();
    assert!(report(
        &mut status,
        AgentWorkStatusPhase::Working,
        "changed title"
    ));
    assert!(!status.mark_ack_notice_delivered());
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
