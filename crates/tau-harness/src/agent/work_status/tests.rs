use super::*;

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
    assert!(
        status.report(
            WorkStatusReport::new(AgentWorkStatusPhase::Working, "first title".to_owned(),)
                .expect("valid first report")
        )
    );
    status.reset_ack_notice();
    assert!(
        status.report(
            WorkStatusReport::new(AgentWorkStatusPhase::Working, "changed title".to_owned(),)
                .expect("valid changed report")
        )
    );
    assert!(!status.mark_ack_notice_delivered());
}

/// Done and Blocked disable the Working final gate without changing the runtime
/// into another implicit Working epoch.
#[test]
fn terminal_reports_disable_only_the_working_gate() {
    for phase in [AgentWorkStatusPhase::Done, AgentWorkStatusPhase::Blocked] {
        let mut status = WorkStatus::default();
        assert!(
            status.report(
                WorkStatusReport::new(phase, "terminal report".to_owned())
                    .expect("valid terminal report")
            )
        );
        assert_eq!(status.phase(), phase);
        assert_eq!(status.decide_final(true), None);
        assert_eq!(status.epoch(), 0);
    }
}
