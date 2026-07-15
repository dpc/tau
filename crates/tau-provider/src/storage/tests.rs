use super::*;

#[derive(Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct TestAuth {
    token: String,
}

#[test]
fn auth_file_loads_default_when_missing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let file = ProviderStore::open_in(temp.path())
        .auth_file::<TestAuth>("provider-test")
        .expect("auth file");

    assert_eq!(file.load().expect("load"), None);
}

#[test]
fn auth_file_saves_and_deletes_typed_json() {
    let temp = tempfile::tempdir().expect("tempdir");
    let file = ProviderStore::open_in(temp.path())
        .auth_file::<TestAuth>("provider-test")
        .expect("auth file");

    file.save(&TestAuth {
        token: "secret".to_owned(),
    })
    .expect("save");
    assert_eq!(
        file.load().expect("load"),
        Some(TestAuth {
            token: "secret".to_owned()
        })
    );
    assert!(file.delete().expect("delete"));
    assert!(!file.delete().expect("delete missing"));
    assert_eq!(file.load().expect("load missing"), None);
}

#[test]
fn auth_file_rejects_unsafe_names() {
    for name in ["", ".hidden", "-leading", "has/slash", "has space"] {
        assert!(
            ProviderStore::open_in("/tmp")
                .auth_file::<TestAuth>(name)
                .is_err(),
            "expected '{name}' to be rejected"
        );
    }
}

/// Callback and unlock classification preserves every successful value and both
/// independently failing error channels.
#[test]
fn auth_file_lock_result_classifies_callback_and_unlock_matrix() {
    match super::auth_file_lock_result::classify_callback_result(Ok(42), Ok(())) {
        AuthFileLockResult::Completed {
            value,
            unlock_error: None,
        } => assert_eq!(value, 42),
        AuthFileLockResult::Completed {
            value: _,
            unlock_error: Some(error),
        }
        | AuthFileLockResult::LockFailed(error)
        | AuthFileLockResult::CallbackFailed {
            error,
            unlock_error: _,
        } => panic!("unexpected lock failure: {error}"),
    }
    match super::auth_file_lock_result::classify_callback_result(
        Ok(42),
        Err(io::Error::other("unlock failed")),
    ) {
        AuthFileLockResult::Completed {
            value,
            unlock_error: Some(error),
        } => {
            assert_eq!(value, 42);
            assert_eq!(error.to_string(), "unlock failed");
        }
        AuthFileLockResult::Completed {
            value: _,
            unlock_error: None,
        }
        | AuthFileLockResult::LockFailed(_)
        | AuthFileLockResult::CallbackFailed {
            error: _,
            unlock_error: _,
        } => panic!("successful callback outcome was not preserved"),
    }
    match super::auth_file_lock_result::classify_callback_result(
        Err::<(), _>(io::Error::other("callback failed")),
        Ok(()),
    ) {
        AuthFileLockResult::CallbackFailed {
            error,
            unlock_error: None,
        } => assert_eq!(error.to_string(), "callback failed"),
        AuthFileLockResult::CallbackFailed {
            error: _,
            unlock_error: Some(error),
        }
        | AuthFileLockResult::LockFailed(error)
        | AuthFileLockResult::Completed {
            value: _,
            unlock_error: Some(error),
        } => panic!("unexpected lock failure: {error}"),
        AuthFileLockResult::Completed {
            value: _,
            unlock_error: None,
        } => panic!("callback unexpectedly succeeded"),
    }
    match super::auth_file_lock_result::classify_callback_result(
        Err::<(), _>(io::Error::other("callback failed")),
        Err(io::Error::other("unlock failed")),
    ) {
        AuthFileLockResult::CallbackFailed {
            error,
            unlock_error: Some(unlock_error),
        } => {
            assert_eq!(error.to_string(), "callback failed");
            assert_eq!(unlock_error.to_string(), "unlock failed");
        }
        AuthFileLockResult::CallbackFailed {
            error: _,
            unlock_error: None,
        }
        | AuthFileLockResult::LockFailed(_)
        | AuthFileLockResult::Completed {
            value: _,
            unlock_error: _,
        } => panic!("dual callback/unlock failure was not preserved"),
    }
}

/// Lock acquisition failure is distinct and never invokes the callback.
#[test]
fn auth_file_lock_result_reports_pre_callback_failure() {
    let temp = tempfile::tempdir().expect("tempdir");
    let not_a_directory = temp.path().join("not-a-directory");
    fs::write(&not_a_directory, b"file").expect("create non-directory state path");
    let file = ProviderStore::open_in(&not_a_directory)
        .auth_file::<TestAuth>("provider-test")
        .expect("auth file");
    let mut callback_ran = false;

    match file.with_lock_result(|_| {
        callback_ran = true;
        Ok(())
    }) {
        AuthFileLockResult::LockFailed(_) => {}
        AuthFileLockResult::CallbackFailed {
            error: _,
            unlock_error: _,
        }
        | AuthFileLockResult::Completed {
            value: _,
            unlock_error: _,
        } => panic!("lock acquisition unexpectedly succeeded"),
    }
    assert!(!callback_ran);
}

/// Legacy projection preserves callback-error precedence and reports unlock
/// failure when a successful callback value cannot be cleanly released.
#[test]
fn auth_file_legacy_lock_projection_preserves_error_precedence() {
    let callback_wins = super::auth_file_lock_result::into_legacy_result(
        super::auth_file_lock_result::classify_callback_result(
            Err::<(), _>(io::Error::other("callback failed")),
            Err(io::Error::other("unlock failed")),
        ),
    )
    .expect_err("callback failure must win");
    assert_eq!(callback_wins.to_string(), "callback failed");

    let unlock_after_success = super::auth_file_lock_result::into_legacy_result(
        super::auth_file_lock_result::classify_callback_result(
            Ok(42),
            Err(io::Error::other("unlock failed")),
        ),
    )
    .expect_err("unlock failure must replace successful callback value");
    assert_eq!(unlock_after_success.to_string(), "unlock failed");
}
