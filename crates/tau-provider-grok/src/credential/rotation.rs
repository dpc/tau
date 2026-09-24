//! One-shot refresh with exact-byte CAS and authoritative generation adoption.

use std::fmt;
use std::future::Future;

use super::Credential;
use crate::oauth::{self, TokenResponse};

/// Storage failures contain no credential bytes or backend diagnostic text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageError {
    /// No successful authoritative response was obtained.
    Unavailable,
    /// A submitted mutation may still commit after the caller stopped waiting.
    OutcomeUnknown,
}

/// Narrow authority for a single preselected Grok Secret record.
///
/// The runtime coalesces same-generation refreshes within its own process. CAS
/// arbitrates saved generations but does not serialize remote exchanges across
/// processes. Concurrent exchanges or a crash before saving may require login
/// again; this crate supplies no cross-process lock or durable recovery marker.
///
/// Implementations must use bounded operations and keep bytes out of Debug,
/// journals, and provider output. CAS compares exact bytes atomically, never a
/// parsed or reserialized approximation. Runtime implements this using the
/// existing nonblocking mainloop Secret RPC; setup can use its local store.
/// A successful read after CAS must execute after that CAS, not race an
/// outstanding mutation through a different connection.
pub trait Storage {
    /// Read authoritative bytes; `None` means removed/logged out, not a cache
    /// miss.
    fn read(&self) -> impl Future<Output = Result<Option<Vec<u8>>, StorageError>> + Send;

    /// Durably replace exactly `expected`; false means a different generation
    /// won. A timeout after submission must report `OutcomeUnknown`.
    fn compare_and_swap(
        &self,
        expected: &[u8],
        replacement: &[u8],
    ) -> impl Future<Output = Result<bool, StorageError>> + Send;
}

/// One non-retrying refresh exchange, injectable for offline rotation oracles.
pub trait RefreshExchange {
    /// Exchange exactly once; a transport error may have consumed the input.
    fn exchange(
        &self,
        refresh_token: &str,
    ) -> impl Future<Output = Result<TokenResponse, oauth::Error>> + Send;
}

impl RefreshExchange for oauth::Client {
    fn exchange(
        &self,
        refresh_token: &str,
    ) -> impl Future<Output = Result<TokenResponse, oauth::Error>> + Send {
        self.refresh(refresh_token)
    }
}

/// Why an inference admission is checking its credential generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefreshReason {
    /// Renew only if the authoritative record has a known expired access token.
    Expired,
    /// One unauthorized response rejected the observed generation.
    Unauthorized,
}

/// Closed rotation failures, never carrying credential or storage error text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// The stored or newly returned record failed the closed schema.
    InvalidRecord,
    /// Logout/removal won; missing credentials must never be resurrected.
    Missing,
    /// Authoritative storage now binds another account.
    AccountChanged,
    /// Inference was canceled before any refresh exchange began.
    Canceled,
    /// An authoritative read could not complete.
    StorageUnavailable,
    /// Mutation status could not be established; do not reuse the old token.
    PublicationUnknown,
    /// A conflicting publication did not expose an adoptable new generation.
    Conflict,
    /// One refresh exchange failed and must not be automatically resent.
    Refresh(oauth::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidRecord => "invalid Grok credential record",
            Self::Missing => "Grok credentials are missing; login is required",
            Self::AccountChanged => "Grok account changed; explicit readmission is required",
            Self::Canceled => "Grok credential admission canceled",
            Self::StorageUnavailable => "Grok credential storage is unavailable",
            Self::PublicationUnknown => "Grok credential publication outcome is unknown",
            Self::Conflict => "Grok credential publication conflicted",
            Self::Refresh(_) => "Grok credential refresh failed",
        })
    }
}

impl std::error::Error for Error {}

/// Reload, optionally rotate once, CAS, then reload before adopting.
///
/// The caller must retain this future/worker through publication once the
/// exchange starts, regardless of inference cancellation. Only the waiter
/// may be canceled then. A returned error must suppress another automatic
/// refresh of the same observed generation; no retry loop exists here.
///
/// `now_ms` is the caller's current wall-clock time. Omitted refresh token
/// and expiry preserve the previous values. OAuth refresh is account-bound;
/// only initial login performs authenticated userinfo validation.
pub async fn resolve(
    store: &impl Storage,
    exchange: &impl RefreshExchange,
    observed: &Credential,
    reason: RefreshReason,
    now_ms: u64,
    canceled: impl Fn() -> bool,
) -> Result<Credential, Error> {
    if canceled() {
        return Err(Error::Canceled);
    }
    let original = read(store).await?;
    let current = Credential::decode(&original)?;
    check_account(observed, &current)?;
    if canceled() {
        return Err(Error::Canceled);
    }
    let rejected = reason == RefreshReason::Unauthorized && &current == observed;
    if !rejected && !current.is_expired(now_ms) {
        return Ok(current);
    }
    // From this point cancellation must not abandon a rotated credential.
    let tokens = exchange
        .exchange(current.refresh_token())
        .await
        .map_err(Error::Refresh)?;
    let replacement = current.refreshed(tokens, now_ms)?;
    let replacement_bytes = replacement.encode();
    let publication = store.compare_and_swap(&original, &replacement_bytes).await;
    // Even an acknowledged CAS is not permission to adopt a cached value:
    // logout/login or another writer may already have superseded it.
    let authoritative = match read(store).await {
        Ok(bytes) => bytes,
        Err(Error::Missing) => return Err(Error::Missing),
        Err(_) => return Err(Error::PublicationUnknown),
    };
    let adopted = Credential::decode(&authoritative)?;
    check_account(observed, &adopted)?;
    // Visible replacement bytes do not prove that a lost/failed CAS reply
    // completed its durability work. Keep the saved token, but fail this
    // admission rather than claiming a durable successful publication.
    if publication.is_err() {
        return Err(Error::PublicationUnknown);
    }
    if authoritative == replacement_bytes || adopted != current {
        return Ok(adopted);
    }
    match publication {
        Ok(false) => Err(Error::Conflict),
        Ok(true) | Err(_) => Err(Error::PublicationUnknown),
    }
}

/// Fetch one authoritative record without permitting absent-record creation.
async fn read(store: &impl Storage) -> Result<Vec<u8>, Error> {
    store
        .read()
        .await
        .map_err(|_| Error::StorageUnavailable)?
        .ok_or(Error::Missing)
}

/// Never silently follow an account switch during old-generation admission.
fn check_account(observed: &Credential, current: &Credential) -> Result<(), Error> {
    if current.subject() != observed.subject() {
        return Err(Error::AccountChanged);
    }
    Ok(())
}
