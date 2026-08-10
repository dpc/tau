use super::HarnessError;

/// Ensures provider discovery timeout diagnostics remain distinct from
/// configured extension process-start timeout diagnostics.
#[test]
fn session_init_timeout_has_distinct_classification_and_message() {
    let startup = HarnessError::StartupTimeout;
    let session_init = HarnessError::SessionInitTimeout;

    assert!(!matches!(session_init, HarnessError::StartupTimeout));
    assert_eq!(
        session_init.to_string(),
        "timed out waiting for session context providers to initialize"
    );
    assert_ne!(session_init.to_string(), startup.to_string());
}
