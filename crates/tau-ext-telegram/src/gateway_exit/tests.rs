use super::*;

/// The complete preflight transport/status table must remain stable for
/// service-manager restart policy.
#[test]
fn webhook_preflight_classifies_transport_and_every_http_status_family() {
    assert_eq!(
        GatewayExitError::webhook_preflight(TelegramApiFailure::Transport).exit_code(),
        ExitCode::from(75)
    );
    for status in 100..=599 {
        let error = GatewayExitError::webhook_preflight(TelegramApiFailure::Http {
            status,
            message: "redacted".to_owned(),
        });
        let expected = if matches!(status, 408 | 425 | 429) || (500..=599).contains(&status) {
            75
        } else {
            78
        };
        assert_eq!(error.exit_code(), ExitCode::from(expected), "HTTP {status}");
    }
    for status in [600, 999] {
        let error = GatewayExitError::webhook_preflight(TelegramApiFailure::Http {
            status,
            message: "redacted".to_owned(),
        });
        assert_eq!(error.exit_code(), ExitCode::from(78), "HTTP {status}");
    }
    assert_eq!(
        GatewayExitError::webhook_preflight(TelegramApiFailure::Protocol(
            "bad response".to_owned()
        ))
        .exit_code(),
        ExitCode::from(70)
    );
}

/// Runtime polling retries every ordinary failure internally but reports HTTP
/// 409 as temporary so a supervisor can recover after a competing poller exits.
#[test]
fn runtime_poll_only_terminates_for_http_409() {
    for status in 100..=599 {
        let failure = TelegramApiFailure::Http {
            status,
            message: "redacted".to_owned(),
        };
        assert_eq!(
            GatewayExitError::runtime_poll(&failure).map(|error| error.exit_code()),
            (status == 409).then(|| ExitCode::from(75)),
            "HTTP {status}"
        );
    }
    assert!(GatewayExitError::runtime_poll(&TelegramApiFailure::Transport).is_none());
}
