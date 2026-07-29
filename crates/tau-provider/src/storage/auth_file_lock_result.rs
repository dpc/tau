//! Exhaustive callback and lock-release outcomes for provider auth files.

use std::io;

/// Exhaustive result of executing one callback under an auth-file sidecar lock.
///
/// This type preserves a successful callback value even when releasing the lock
/// fails afterwards. It intentionally has no `Debug` implementation because the
/// callback value may contain credentials.
#[must_use = "locking failures and callback values must be handled"]
pub enum AuthFileLockResult<R> {
    /// Opening or acquiring the sidecar lock failed before the callback ran.
    LockFailed(io::Error),
    /// The callback failed; lock release may have failed independently.
    CallbackFailed {
        /// Failure returned by the callback.
        error: io::Error,
        /// Failure returned while releasing the lock, when present.
        unlock_error: Option<io::Error>,
    },
    /// The callback completed and its value is preserved.
    Completed {
        /// Value returned by the callback.
        value: R,
        /// Failure returned while releasing the lock, when present.
        unlock_error: Option<io::Error>,
    },
}

pub(super) fn classify_callback_result<R>(
    callback_result: io::Result<R>,
    unlock_result: io::Result<()>,
) -> AuthFileLockResult<R> {
    let unlock_error = unlock_result.err();
    // Preserve this behavior; the structural alternative is not semantics-neutral
    // here. ast-grep-ignore: match-result-verbose
    match callback_result {
        Ok(value) => AuthFileLockResult::Completed {
            value,
            unlock_error,
        },
        Err(error) => AuthFileLockResult::CallbackFailed {
            error,
            unlock_error,
        },
    }
}

pub(super) fn into_legacy_result<R>(result: AuthFileLockResult<R>) -> io::Result<R> {
    match result {
        AuthFileLockResult::LockFailed(error)
        | AuthFileLockResult::CallbackFailed {
            error,
            unlock_error: _,
        } => Err(error),
        AuthFileLockResult::Completed {
            value,
            unlock_error: None,
        } => Ok(value),
        AuthFileLockResult::Completed {
            value: _,
            unlock_error: Some(error),
        } => Err(error),
    }
}
