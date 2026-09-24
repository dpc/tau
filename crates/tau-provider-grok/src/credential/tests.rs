//! Offline credential schema, publication, and competing-generation oracles.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use serde_json::{Value, json};
use tokio::sync::Barrier;

use super::*;
use crate::oauth::{self, TokenResponse};

/// Construct an offline renewable generation with optional known expiry.
fn credential(access: &str, expiry: Option<u64>) -> Credential {
    Credential::decode(
        &serde_json::to_vec(&json!({
            "version": 0, "kind": "grok_oauth",
            "access_token": access, "refresh_token": "single-use",
            "expires_at_ms": expiry, "subject": "account-one"
        }))
        .expect("fixture"),
    )
    .expect("credential")
}

/// Decode a valid record and reject cross-provider or malformed
/// representations.
#[test]
fn closed_record_preserves_unknown_expiry_and_redacts_invalid_fields() {
    let record = credential("bearer", None);
    assert!(!record.is_expired(u64::MAX));
    assert!(Credential::decode(&record.encode()).expect("round trip") == record);
    for (field, value) in [
        ("version", json!(1)),
        ("kind", json!("chatgpt_oauth")),
        ("access_token", json!("secret\nvalue")),
        ("refresh_token", json!(" ")),
        ("subject", json!("bad\0subject")),
        ("expires_at_ms", json!(-1)),
        ("unknown", json!("secret-marker")),
    ] {
        let mut wire: Value = serde_json::from_slice(&record.encode()).expect("wire");
        wire[field] = value;
        let error = Credential::decode(&serde_json::to_vec(&wire).expect("wire"))
            .err()
            .expect("reject");
        assert_eq!(error, Error::InvalidRecord);
        assert!(!format!("{error:?} {error}").contains("secret-marker"));
    }
    assert_eq!(
        Credential::decode(&vec![b' '; 384 * 1024 + 1]).err(),
        Some(Error::InvalidRecord)
    );
}

/// Initial login requires renewal; refresh omission preserves prior metadata.
#[test]
fn login_and_refresh_preserve_advertised_fields_without_inventing_ttl() {
    let initial = Credential::from_login(
        TokenResponse {
            access_token: "initial".into(),
            refresh_token: Some("renewable".into()),
            expires_in: None,
        },
        "account-one".into(),
        100,
    )
    .expect("login");
    assert_eq!(initial.expires_at_ms(), None);
    let known = initial
        .refreshed(
            TokenResponse {
                access_token: "next".into(),
                refresh_token: Some("rotated".into()),
                expires_in: Some(5),
            },
            100,
        )
        .expect("refresh");
    assert_eq!(known.expires_at_ms(), Some(5100));
    assert_eq!(known.refresh_token(), "rotated");
    let omitted = known
        .refreshed(
            TokenResponse {
                access_token: "third".into(),
                refresh_token: None,
                expires_in: None,
            },
            200,
        )
        .expect("omission");
    assert_eq!(omitted.expires_at_ms(), Some(5100));
    assert_eq!(omitted.refresh_token(), "rotated");
    assert_eq!(omitted.subject(), "account-one");
    assert!(
        Credential::from_login(
            TokenResponse {
                access_token: "no-refresh".into(),
                refresh_token: None,
                expires_in: None,
            },
            "account-one".into(),
            0,
        )
        .is_err()
    );
}

/// Publication fault injected by the fake authoritative store.
enum Publish {
    /// Ordinary atomic CAS.
    Apply,
    /// Commit but lose the acknowledgement.
    LostReply,
    /// Logout wins before CAS.
    Remove,
    /// Another login replaces the record before CAS.
    Replace(Credential),
    /// No write occurred and the submitted operation's outcome is unknown.
    Unknown,
}

/// One bounded in-memory Secret slot; never derives credential-bearing Debug.
struct MemoryStore {
    /// Authoritative encoded generation.
    bytes: Mutex<Option<Vec<u8>>>,
    /// One configured publication behavior.
    publish: Publish,
    /// Count of actual CAS submissions.
    writes: AtomicUsize,
}

impl MemoryStore {
    /// Initialize one authoritative generation.
    fn new(record: &Credential, publish: Publish) -> Self {
        Self {
            bytes: Mutex::new(Some(record.encode())),
            publish,
            writes: AtomicUsize::new(0),
        }
    }
}

impl Storage for MemoryStore {
    async fn read(&self) -> Result<Option<Vec<u8>>, StorageError> {
        Ok(self.bytes.lock().expect("store").clone())
    }

    async fn compare_and_swap(
        &self,
        expected: &[u8],
        replacement: &[u8],
    ) -> Result<bool, StorageError> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        let mut bytes = self.bytes.lock().expect("store");
        match &self.publish {
            Publish::Remove => {
                *bytes = None;
                return Ok(false);
            }
            Publish::Replace(record) => {
                *bytes = Some(record.encode());
                return Ok(false);
            }
            Publish::Unknown => return Err(StorageError::OutcomeUnknown),
            Publish::Apply | Publish::LostReply => {}
        }
        if bytes.as_deref() != Some(expected) {
            return Ok(false);
        }
        *bytes = Some(replacement.to_owned());
        match self.publish {
            Publish::LostReply => Err(StorageError::OutcomeUnknown),
            _ => Ok(true),
        }
    }
}

/// Fake one-shot refresh optionally cancels its inference waiter after
/// rotation.
struct Exchange {
    /// Count of exchanges; repeated calls are observable.
    calls: AtomicUsize,
    /// Sticky inference cancellation, independent of publication.
    canceled: AtomicBool,
    /// Whether successful rotation cancels the waiter.
    cancel_on_exchange: bool,
    /// Optional finite exchange failure.
    failure: Option<oauth::Error>,
}

impl Exchange {
    /// Construct one successful synthetic exchange.
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            canceled: AtomicBool::new(false),
            cancel_on_exchange: false,
            failure: None,
        }
    }
}

impl RefreshExchange for Exchange {
    async fn exchange(&self, refresh_token: &str) -> Result<TokenResponse, oauth::Error> {
        assert_eq!(refresh_token, "single-use");
        assert_eq!(self.calls.fetch_add(1, Ordering::SeqCst), 0);
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.cancel_on_exchange {
            self.canceled.store(true, Ordering::SeqCst);
        }
        Ok(TokenResponse {
            access_token: "renewed".into(),
            refresh_token: Some("rotated".into()),
            expires_in: Some(100),
        })
    }
}

/// Expired and rejected generations rotate once, then adopt authoritative
/// bytes.
#[tokio::test]
async fn refresh_reloads_and_commits_before_adoption() {
    let old = credential("old", Some(1));
    for publication in [Publish::Apply] {
        let store = MemoryStore::new(&old, publication);
        let exchange = Exchange::new();
        let adopted = resolve(&store, &exchange, &old, RefreshReason::Expired, 2, || false)
            .await
            .expect("durably adopted");
        assert_eq!(adopted.access_token(), "renewed");
        assert_eq!(adopted.refresh_token(), "rotated");
        assert_eq!(adopted.expires_at_ms(), Some(100002));
        assert_eq!(store.writes.load(Ordering::SeqCst), 1);
        assert!(store.read().await.expect("read").expect("record") == adopted.encode());
    }
}

/// A visible rotated token after a lost reply is saved, not a durability proof.
#[tokio::test]
async fn lost_publication_reply_does_not_claim_durable_adoption() {
    let old = credential("old", Some(1));
    let store = MemoryStore::new(&old, Publish::LostReply);
    let exchange = Exchange::new();
    assert_eq!(
        resolve(&store, &exchange, &old, RefreshReason::Expired, 2, || false)
            .await
            .err(),
        Some(Error::PublicationUnknown)
    );
    let stored =
        Credential::decode(&store.read().await.expect("read").expect("record")).expect("rotated");
    assert_eq!(stored.refresh_token(), "rotated");
    assert_eq!(store.writes.load(Ordering::SeqCst), 1);
    assert_eq!(exchange.calls.load(Ordering::SeqCst), 1);
}

/// A new same-account generation wins over the stale inference snapshot.
#[tokio::test]
async fn authoritative_generation_avoids_reusing_rejected_refresh_token() {
    let old = credential("old", Some(1));
    let current = credential("already-renewed", None);
    let store = MemoryStore::new(&current, Publish::Apply);
    let exchange = Exchange::new();
    let adopted = resolve(
        &store,
        &exchange,
        &old,
        RefreshReason::Unauthorized,
        2,
        || false,
    )
    .await
    .expect("new generation");
    assert!(adopted == current);
    assert_eq!(exchange.calls.load(Ordering::SeqCst), 0);
    assert_eq!(store.writes.load(Ordering::SeqCst), 0);
    let unknown = MemoryStore::new(&old, Publish::Apply);
    let unknown_record = credential("unknown-expiry", None);
    *unknown.bytes.lock().expect("store") = Some(unknown_record.encode());
    resolve(
        &unknown,
        &exchange,
        &unknown_record,
        RefreshReason::Expired,
        u64::MAX,
        || false,
    )
    .await
    .expect("unknown is not expired");
    assert_eq!(exchange.calls.load(Ordering::SeqCst), 0);
}

/// Logout/account replacement cannot be resurrected by a stale rotated result.
#[tokio::test]
async fn late_logout_account_switch_and_unknown_publication_stop_adoption() {
    let old = credential("old", Some(1));
    let other = Credential::from_login(
        TokenResponse {
            access_token: "other".into(),
            refresh_token: Some("other-refresh".into()),
            expires_in: None,
        },
        "account-two".into(),
        0,
    )
    .expect("other login");
    for (publication, expected) in [
        (Publish::Remove, Error::Missing),
        (Publish::Replace(other), Error::AccountChanged),
        (Publish::Unknown, Error::PublicationUnknown),
    ] {
        let store = MemoryStore::new(&old, publication);
        let exchange = Exchange::new();
        let result = resolve(&store, &exchange, &old, RefreshReason::Expired, 2, || false).await;
        assert_eq!(result.err(), Some(expected));
        assert_eq!(exchange.calls.load(Ordering::SeqCst), 1);
        assert_eq!(store.writes.load(Ordering::SeqCst), 1);
    }
}

/// Cancellation before exchange does nothing; after rotation it saves the
/// token.
#[tokio::test]
async fn cancellation_does_not_discard_already_rotated_credentials() {
    let old = credential("old", Some(1));
    let store = MemoryStore::new(&old, Publish::Apply);
    let mut exchange = Exchange::new();
    assert_eq!(
        resolve(&store, &exchange, &old, RefreshReason::Expired, 2, || true)
            .await
            .err(),
        Some(Error::Canceled)
    );
    assert_eq!(exchange.calls.load(Ordering::SeqCst), 0);
    assert_eq!(store.writes.load(Ordering::SeqCst), 0);
    exchange.cancel_on_exchange = true;
    let adopted = resolve(&store, &exchange, &old, RefreshReason::Expired, 2, || {
        exchange.canceled.load(Ordering::SeqCst)
    })
    .await
    .expect("publication completes despite canceled waiter");
    assert_eq!(adopted.refresh_token(), "rotated");
    assert!(exchange.canceled.load(Ordering::SeqCst));
    assert_eq!(store.writes.load(Ordering::SeqCst), 1);
}

/// An ambiguous exchange stops without CAS or another token request.
#[tokio::test]
async fn lost_exchange_response_is_not_retried() {
    let old = credential("old", Some(1));
    let store = MemoryStore::new(&old, Publish::Apply);
    let mut exchange = Exchange::new();
    exchange.failure = Some(oauth::Error::Transport);
    assert_eq!(
        resolve(&store, &exchange, &old, RefreshReason::Expired, 2, || false)
            .await
            .err(),
        Some(Error::Refresh(oauth::Error::Transport))
    );
    assert_eq!(exchange.calls.load(Ordering::SeqCst), 1);
    assert_eq!(store.writes.load(Ordering::SeqCst), 0);
}

/// Unknown expiry still permits one refresh after a matching unauthorized
/// result.
#[tokio::test]
async fn unauthorized_unknown_expiry_refreshes_only_the_rejected_generation() {
    let old = credential("unknown-expiry", None);
    let store = MemoryStore::new(&old, Publish::Apply);
    let exchange = Exchange::new();
    let adopted = resolve(
        &store,
        &exchange,
        &old,
        RefreshReason::Unauthorized,
        2,
        || false,
    )
    .await
    .expect("one rejected generation");
    assert_eq!(adopted.access_token(), "renewed");
    assert_eq!(exchange.calls.load(Ordering::SeqCst), 1);
}

/// A same-account login may win CAS; missing credentials never start a refresh.
#[tokio::test]
async fn same_account_replacement_is_adopted_but_missing_record_is_not_created() {
    let old = credential("old", Some(1));
    let winner = credential("new-login", None);
    let store = MemoryStore::new(&old, Publish::Replace(winner.clone()));
    let exchange = Exchange::new();
    let adopted = resolve(&store, &exchange, &old, RefreshReason::Expired, 2, || false)
        .await
        .expect("same-account winner");
    assert!(adopted == winner);
    let missing = MemoryStore::new(&old, Publish::Apply);
    *missing.bytes.lock().expect("store") = None;
    let exchange = Exchange::new();
    assert_eq!(
        resolve(&missing, &exchange, &old, RefreshReason::Expired, 2, || {
            false
        })
        .await
        .err(),
        Some(Error::Missing)
    );
    assert_eq!(exchange.calls.load(Ordering::SeqCst), 0);
    assert_eq!(missing.writes.load(Ordering::SeqCst), 0);
}

/// Two independent runtimes can both exchange before either reaches Secret CAS.
struct ConcurrentExchange {
    /// Force both exchanges to start from the same stored generation.
    arrived: Barrier,
    /// Count remote exchanges, intentionally not serialized by storage CAS.
    calls: AtomicUsize,
}

impl RefreshExchange for ConcurrentExchange {
    async fn exchange(&self, refresh_token: &str) -> Result<TokenResponse, oauth::Error> {
        assert_eq!(refresh_token, "single-use");
        let generation = self.calls.fetch_add(1, Ordering::SeqCst);
        self.arrived.wait().await;
        // This fixture permits both grants to succeed. An upstream that rejects
        // token reuse may instead force one independent caller to log in again.
        Ok(TokenResponse {
            access_token: format!("bearer-{generation}"),
            refresh_token: Some(format!("refresh-{generation}")),
            expires_in: Some(100),
        })
    }
}

/// CAS saves one winner but does not promise one remote exchange across
/// callers.
#[tokio::test]
async fn concurrent_exchanges_adopt_cas_winner_without_exactly_once_claim() {
    let old = credential("old", Some(1));
    let store = MemoryStore::new(&old, Publish::Apply);
    let exchange = ConcurrentExchange {
        arrived: Barrier::new(2),
        calls: AtomicUsize::new(0),
    };
    let (first, second) = tokio::join!(
        resolve(&store, &exchange, &old, RefreshReason::Expired, 2, || false),
        resolve(&store, &exchange, &old, RefreshReason::Expired, 2, || false)
    );
    let first = first.expect("first adoption");
    let second = second.expect("second adoption");
    assert!(first == second);
    assert_eq!(exchange.calls.load(Ordering::SeqCst), 2);
    assert_eq!(store.writes.load(Ordering::SeqCst), 2);
    assert_eq!(
        store.read().await.expect("read").expect("winner"),
        first.encode()
    );
}
