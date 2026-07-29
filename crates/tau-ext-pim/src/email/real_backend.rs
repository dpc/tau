use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_imap::imap_proto::types::NameAttribute;
use async_imap::types::Flag;
use async_imap::{Authenticator, Client, Session};
use futures_util::TryStreamExt;
use lettre::message::Mailbox;
use lettre::message::header::ContentType as LettreContentType;
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::transport::smtp::client::{AsyncSmtpConnection, CertificateStore, Tls, TlsParameters};
use lettre::transport::smtp::extension::ClientId;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use mail_parser::{Address as ParsedAddress, MessageParser, MimeHeaders};
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, RootCertStore};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::runtime::Runtime;
use tokio::time;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use super::{
    AuthMethod, AuthenticationResultsEvidence, BackendAttachment, BackendFolder, BackendMessage,
    BackendMessagePage, EmailBackend, EmailOauth2Provider, MessageFlagMutation, OutgoingMessage,
    READ_BODY_MAX_BYTES, StateStore, TlsMode, ValidatedAuthConfig, ValidatedConfig,
    ValidatedImapConfig, ValidatedSmtpConfig,
};
use crate::google_oauth::{GoogleOauthClient, GoogleOauthSecretConfig};

pub(super) const READ_MESSAGE_FETCH_MAX_BYTES: usize = READ_BODY_MAX_BYTES * 4;
pub(super) const METADATA_HEADER_FETCH_MAX_BYTES: usize = 32 * 1024;
const RECENT_SEARCH_FETCH_WINDOW: usize = 1000;
pub(super) const FETCH_METADATA_ITEMS: &str = "(UID FLAGS INTERNALDATE BODY.PEEK[HEADER]<0.32768>)";
pub(super) const FETCH_FULL_MESSAGE_ITEMS: &str =
    "(UID FLAGS INTERNALDATE RFC822.SIZE BODY.PEEK[]<0.262144>)";

/// Production IMAP/SMTP backend for configured email accounts.
pub struct RealEmailBackend {
    accounts: BTreeMap<String, RealAccount>,
    runtime: Runtime,
    oauth: Arc<GoogleOauthClient>,
}

#[derive(Clone)]
struct RealAccount {
    id: String,
    imap: Option<ValidatedImapConfig>,
    smtp: Option<ValidatedSmtpConfig>,
    auth: Option<ValidatedAuthConfig>,
    secrets: Arc<BTreeMap<String, tau_proto::SecretValue>>,
    state: StateStore,
    oauth: Arc<GoogleOauthClient>,
}

impl RealEmailBackend {
    /// Build a production backend from validated extension configuration.
    pub fn new(
        config: &ValidatedConfig,
        secrets: BTreeMap<String, tau_proto::SecretValue>,
        state: StateStore,
    ) -> Result<Self, String> {
        let runtime = Runtime::new()
            .map_err(|error| format!("internal_error: failed to start email runtime: {error}"))?;
        let oauth = Arc::new(GoogleOauthClient::new(secrets.clone()));
        let secrets = Arc::new(secrets);
        let accounts = config
            .accounts
            .iter()
            .map(|(id, account)| {
                (
                    id.clone(),
                    RealAccount {
                        id: id.clone(),
                        imap: account.imap.clone(),
                        smtp: account.smtp.clone(),
                        auth: account.auth.clone(),
                        secrets: Arc::clone(&secrets),
                        state: state.clone(),
                        oauth: Arc::clone(&oauth),
                    },
                )
            })
            .collect();
        Ok(Self {
            accounts,
            runtime,
            oauth,
        })
    }

    fn account(&self, id: &str) -> Result<RealAccount, String> {
        self.accounts
            .get(id)
            .cloned()
            .ok_or_else(|| "internal_error: account not found in backend".to_owned())
    }

    fn block_with_timeout<T, Fut>(&self, seconds: u64, fut: Fut) -> Result<T, String>
    where
        Fut: Future<Output = Result<T, String>>,
    {
        self.runtime.block_on(async move {
            match time::timeout(Duration::from_secs(seconds), fut).await {
                Ok(result) => result,
                Err(_) => Err("network_error: email backend operation timed out".to_owned()),
            }
        })
    }
}

impl EmailBackend for RealEmailBackend {
    fn list_folders(&self, account: &str) -> Result<Vec<BackendFolder>, String> {
        let account = self.account(account)?;
        let timeout_seconds = account.imap_config()?.timeout_seconds;
        self.block_with_timeout(timeout_seconds, async move {
            let mut session = connect_imap(&account).await?;
            let mut names = session.list(None, Some("*")).await.map_err(imap_error)?;
            let mut folders = Vec::new();
            while let Some(name) = names.try_next().await.map_err(imap_error)? {
                let selectable = !name.attributes().contains(&NameAttribute::NoSelect);
                folders.push(BackendFolder {
                    name: name.name().to_owned(),
                    delimiter: name.delimiter().unwrap_or("/").to_owned(),
                    selectable,
                });
            }
            drop(names);
            let _ = session.logout().await;
            Ok(folders)
        })
    }

    fn list_messages(&self, account: &str, folder: &str) -> Result<Vec<BackendMessage>, String> {
        self.list_messages_by_uid_page(account, folder, 100, 0)
            .map(|page| page.messages)
    }

    fn list_messages_by_uid_page(
        &self,
        account: &str,
        folder: &str,
        limit: usize,
        offset: usize,
    ) -> Result<BackendMessagePage, String> {
        let account = self.account(account)?;
        let timeout_seconds = account.imap_config()?.timeout_seconds;
        let folder = folder.to_owned();
        self.block_with_timeout(timeout_seconds, async move {
            list_messages_by_uid_page_async(&account, &folder, limit, offset).await
        })
    }

    fn list_recent_messages_page(
        &self,
        account: &str,
        folder: &str,
        limit: usize,
        offset: usize,
        days: u32,
    ) -> Result<BackendMessagePage, String> {
        let account = self.account(account)?;
        let timeout_seconds = account.imap_config()?.timeout_seconds;
        let folder = folder.to_owned();
        self.block_with_timeout(timeout_seconds, async move {
            list_recent_messages_page_async(&account, &folder, limit, offset, days).await
        })
    }

    fn message_metadata(
        &self,
        account: &str,
        folder: &str,
        uid: &str,
    ) -> Result<BackendMessage, String> {
        let account = self.account(account)?;
        let timeout_seconds = account.imap_config()?.timeout_seconds;
        let folder = folder.to_owned();
        let uid = uid.to_owned();
        self.block_with_timeout(timeout_seconds, async move {
            message_metadata_async(&account, &folder, &uid).await
        })
    }

    fn read_message(
        &self,
        account: &str,
        folder: &str,
        uid: &str,
    ) -> Result<BackendMessage, String> {
        let account = self.account(account)?;
        let timeout_seconds = account.imap_config()?.timeout_seconds;
        let folder = folder.to_owned();
        let uid = uid.to_owned();
        self.block_with_timeout(timeout_seconds, async move {
            read_message_async(&account, &folder, &uid).await
        })
    }

    fn update_message_flags(
        &mut self,
        account: &str,
        folder: &str,
        uid: &str,
        mutation: MessageFlagMutation,
    ) -> Result<(), String> {
        let account = self.account(account)?;
        let timeout_seconds = account.imap_config()?.timeout_seconds;
        let folder = folder.to_owned();
        let uid = uid.to_owned();
        self.block_with_timeout(timeout_seconds, async move {
            update_message_flags_async(&account, &folder, &uid, mutation).await
        })
    }

    fn move_message_to_trash(
        &mut self,
        account: &str,
        folder: &str,
        uid: &str,
    ) -> Result<String, String> {
        let account = self.account(account)?;
        let timeout_seconds = account.imap_config()?.timeout_seconds;
        let folder = folder.to_owned();
        let uid = uid.to_owned();
        self.block_with_timeout(timeout_seconds, async move {
            move_message_to_trash_async(&account, &folder, &uid).await
        })
    }

    fn send_message(&mut self, message: &OutgoingMessage) -> Result<String, String> {
        let account = self.account(&message.account)?;
        let timeout_seconds = account.smtp_config()?.timeout_seconds;
        let message = clone_outgoing_message(message);
        self.block_with_timeout(timeout_seconds, async move {
            send_message_async(&account, &message).await
        })
    }

    fn start_google_installed_app_auth(
        &self,
        account: &str,
    ) -> Result<(String, String, String, String, u64), String> {
        let account = self.account(account)?;
        let config = account.google_oauth_config()?;
        let started = self.oauth.start_gmail_installed_app_auth(config)?;
        Ok((
            started.authorization_url,
            started.state,
            started.pkce_verifier,
            started.redirect_uri,
            started.expires_in_secs,
        ))
    }

    fn finish_google_installed_app_auth(
        &self,
        account: &str,
        code: &str,
        pkce_verifier: &str,
        redirect_uri: &str,
    ) -> Result<(String, Option<String>, Option<u64>), String> {
        let account = self.account(account)?;
        let config = account.google_oauth_config()?;
        let finished =
            self.oauth
                .finish_installed_app_auth(config, code, pkce_verifier, redirect_uri)?;
        Ok((
            finished.refresh_token,
            finished.access_token,
            finished.expires_in_secs,
        ))
    }

    fn prime_google_access_token_cache(
        &self,
        account: &str,
        access_token: String,
        expires_in_secs: Option<u64>,
    ) -> Result<(), String> {
        self.oauth
            .prime_access_token_cache(account, access_token, expires_in_secs)
    }
}

impl RealAccount {
    fn imap_config(&self) -> Result<&ValidatedImapConfig, String> {
        self.imap
            .as_ref()
            .ok_or_else(|| "imap_error: account has no IMAP configuration".to_owned())
    }

    fn smtp_config(&self) -> Result<&ValidatedSmtpConfig, String> {
        self.smtp
            .as_ref()
            .ok_or_else(|| "smtp_error: account has no SMTP configuration".to_owned())
    }

    fn auth_config(&self) -> Result<&ValidatedAuthConfig, String> {
        self.auth
            .as_ref()
            .ok_or_else(|| "auth_error: account auth is not configured".to_owned())
    }

    fn google_oauth_config(&self) -> Result<GoogleOauthSecretConfig<'_>, String> {
        let auth = self.auth_config()?;
        if auth.method != AuthMethod::Oauth2 || auth.provider != Some(EmailOauth2Provider::Google) {
            return Err("auth_error: account is not configured for Google OAuth".to_owned());
        }
        let client_id_secret = auth.client_id_secret.as_deref().ok_or_else(|| {
            "auth_error: Google OAuth client id secret is not configured".to_owned()
        })?;
        Ok(GoogleOauthSecretConfig {
            client_id_secret,
            client_secret_secret: auth.client_secret_secret.as_deref(),
            refresh_token_secret: auth.refresh_token_secret.as_deref(),
        })
    }

    fn google_access_token(&self) -> Result<String, String> {
        let config = self.google_oauth_config()?;
        let stored_refresh_token = if config.refresh_token_secret.is_some() {
            None
        } else {
            Some(self.state.google_refresh_token(&self.id)?.ok_or_else(|| {
                format!(
                    "Google email account `{}` is not authorized; run `:email auth google start {}` and then `:email auth google finish {} <copied-url>`",
                    self.id, self.id, self.id
                )
            })?)
        };
        self.oauth.access_token(
            &self.id,
            config,
            stored_refresh_token.as_deref(),
            &format!(
                "Google email account `{}` is not authorized; run `:email auth google start {}` and then `:email auth google finish {} <copied-url>`",
                self.id, self.id, self.id
            ),
        )
    }

    fn invalidate_google_access_token(&self) -> Result<(), String> {
        self.oauth.invalidate_access_token(&self.id)
    }
}

#[derive(Debug)]
enum RealImapStream {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

struct Xoauth2Authenticator {
    payload: Vec<u8>,
}

impl Xoauth2Authenticator {
    fn new(login: &str, access_token: &str) -> Self {
        Self {
            payload: xoauth2_payload(login, access_token).into_bytes(),
        }
    }
}

impl Authenticator for Xoauth2Authenticator {
    type Response = Vec<u8>;

    fn process(&mut self, _challenge: &[u8]) -> Self::Response {
        self.payload.clone()
    }
}

impl AsyncRead for RealImapStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for RealImapStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_shutdown(cx),
        }
    }
}

async fn list_messages_by_uid_page_async(
    account: &RealAccount,
    folder: &str,
    limit: usize,
    offset: usize,
) -> Result<BackendMessagePage, String> {
    let mut session = connect_imap(account).await?;
    let mailbox = session.examine(folder).await.map_err(imap_error)?;
    // Preserve this behavior; the structural alternative is not semantics-neutral
    // here. ast-grep-ignore: unwrap-or-default
    let uidvalidity = mailbox
        .uid_validity
        .map(|value| value.to_string())
        .unwrap_or_default();
    let exists = mailbox.exists as usize;
    if exists <= offset || limit == 0 {
        let _ = session.logout().await;
        return Ok(BackendMessagePage {
            messages: Vec::new(),
            next_cursor: None,
            truncated: false,
        });
    }
    let remaining = exists - offset;
    let fetch_count = remaining.min(limit);
    let high_seq = exists - offset;
    let low_seq = high_seq - fetch_count + 1;
    let sequence_set = format!("{low_seq}:{high_seq}");

    let mut fetches = session
        .fetch(sequence_set, FETCH_METADATA_ITEMS)
        .await
        .map_err(imap_error)?;
    let mut messages = Vec::new();
    while let Some(fetch) = fetches.try_next().await.map_err(imap_error)? {
        messages.push(metadata_from_fetch(&fetch, &uidvalidity));
    }
    drop(fetches);
    let _ = session.logout().await;

    messages.sort_unstable_by(|left, right| {
        let left_uid = left.uid.parse::<u32>().unwrap_or(0);
        let right_uid = right.uid.parse::<u32>().unwrap_or(0);
        right_uid.cmp(&left_uid)
    });
    let truncated = offset.saturating_add(fetch_count) < exists;
    Ok(BackendMessagePage {
        messages,
        next_cursor: truncated.then(|| offset.saturating_add(fetch_count).to_string()),
        truncated,
    })
}

async fn list_recent_messages_page_async(
    account: &RealAccount,
    folder: &str,
    limit: usize,
    offset: usize,
    days: u32,
) -> Result<BackendMessagePage, String> {
    if limit == 0 {
        return Ok(BackendMessagePage {
            messages: Vec::new(),
            next_cursor: None,
            truncated: false,
        });
    }
    let mut session = connect_imap(account).await?;
    let mailbox = session.examine(folder).await.map_err(imap_error)?;
    // Preserve this behavior; the structural alternative is not semantics-neutral
    // here. ast-grep-ignore: unwrap-or-default
    let uidvalidity = mailbox
        .uid_validity
        .map(|value| value.to_string())
        .unwrap_or_default();
    let since = imap_since_date(days)?;
    let mut uids = session
        .uid_search(format!("SINCE {since}"))
        .await
        .map_err(imap_error)?
        .into_iter()
        .collect::<Vec<_>>();
    uids.sort_unstable_by(|left, right| right.cmp(left));
    let total_matches = uids.len();
    let fetch_start = offset.min(total_matches);
    let fetch_end = offset
        .saturating_add(limit)
        .min(total_matches)
        .min(fetch_start.saturating_add(RECENT_SEARCH_FETCH_WINDOW));
    let page_uids = &uids[fetch_start..fetch_end];
    if page_uids.is_empty() {
        let _ = session.logout().await;
        return Ok(BackendMessagePage {
            messages: Vec::new(),
            next_cursor: None,
            truncated: false,
        });
    }
    let uid_set = page_uids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let mut fetches = session
        .uid_fetch(uid_set, FETCH_METADATA_ITEMS)
        .await
        .map_err(imap_error)?;
    let mut messages = Vec::new();
    while let Some(fetch) = fetches.try_next().await.map_err(imap_error)? {
        // Preserve this behavior; the structural alternative is not semantics-neutral
        // here. ast-grep-ignore: unwrap-or-default
        let internal_timestamp = fetch
            .internal_date()
            .map(|date| date.timestamp())
            .unwrap_or_default();
        let uid = fetch.uid.unwrap_or(fetch.message);
        messages.push((
            internal_timestamp,
            uid,
            metadata_from_fetch(&fetch, &uidvalidity),
        ));
    }
    drop(fetches);
    let _ = session.logout().await;

    messages
        .sort_unstable_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
    let truncated = offset.saturating_add(limit) < total_matches;
    let next_offset = offset.saturating_add(limit);
    Ok(BackendMessagePage {
        messages: messages
            .into_iter()
            .take(limit)
            .map(|(_, _, message)| message)
            .collect(),
        next_cursor: truncated.then(|| next_offset.to_string()),
        truncated,
    })
}

fn imap_since_date(days: u32) -> Result<String, String> {
    // Preserve this behavior; the structural alternative is not semantics-neutral
    // here. ast-grep-ignore: silent-map-err
    let now_days = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "internal_error: system clock is before Unix epoch".to_owned())?
        .as_secs()
        / 86_400;
    let since_days = now_days.saturating_sub(u64::from(days.saturating_sub(1)));
    let (year, month, day) = civil_date_from_unix_days(since_days as i64);
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    Ok(format!("{day}-{}-{year}", MONTHS[month - 1]))
}

fn civil_date_from_unix_days(days: i64) -> (i32, usize, u8) {
    let z = days + 719_468;
    let era = if 0 <= z { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if month <= 2 { 1 } else { 0 };
    (year as i32, month as usize, day as u8)
}

async fn message_metadata_async(
    account: &RealAccount,
    folder: &str,
    uid: &str,
) -> Result<BackendMessage, String> {
    let mut session = connect_imap(account).await?;
    let mailbox = session.examine(folder).await.map_err(imap_error)?;
    // Preserve this behavior; the structural alternative is not semantics-neutral
    // here. ast-grep-ignore: unwrap-or-default
    let uidvalidity = mailbox
        .uid_validity
        .map(|value| value.to_string())
        .unwrap_or_default();
    let requested_uid = validated_uid_arg(uid)?;
    let uid_arg = requested_uid.to_string();
    let mut fetches = session
        .uid_fetch(&uid_arg, FETCH_METADATA_ITEMS)
        .await
        .map_err(imap_error)?;
    let message = match fetches.try_next().await.map_err(imap_error)? {
        Some(fetch) if fetch.uid == Some(requested_uid) => {
            metadata_from_fetch(&fetch, &uidvalidity)
        }
        Some(_) | None => return Err("message_not_found: message not found".to_owned()),
    };
    drop(fetches);
    let _ = session.logout().await;
    Ok(message)
}

async fn read_message_async(
    account: &RealAccount,
    folder: &str,
    uid: &str,
) -> Result<BackendMessage, String> {
    let mut session = connect_imap(account).await?;
    let mailbox = session.examine(folder).await.map_err(imap_error)?;
    // Preserve this behavior; the structural alternative is not semantics-neutral
    // here. ast-grep-ignore: unwrap-or-default
    let uidvalidity = mailbox
        .uid_validity
        .map(|value| value.to_string())
        .unwrap_or_default();
    let requested_uid = validated_uid_arg(uid)?;
    let uid_arg = requested_uid.to_string();
    let mut fetches = session
        .uid_fetch(&uid_arg, FETCH_FULL_MESSAGE_ITEMS)
        .await
        .map_err(imap_error)?;
    let message = match fetches.try_next().await.map_err(imap_error)? {
        Some(fetch) if fetch.uid == Some(requested_uid) => {
            let metadata = metadata_from_fetch(&fetch, &uidvalidity);
            let body = fetch
                .body()
                .ok_or_else(|| "message_not_found: message body not found".to_owned())?;
            let source_truncated = fetch
                .size
                .map(|size| (body.len() as u64) < u64::from(size))
                .unwrap_or(READ_MESSAGE_FETCH_MAX_BYTES <= body.len());
            let mut message = parse_backend_message_from_rfc822(&metadata, body);
            message.source_truncated = message.source_truncated || source_truncated;
            message
        }
        Some(_) | None => return Err("message_not_found: message not found".to_owned()),
    };
    drop(fetches);
    let _ = session.logout().await;
    Ok(message)
}

async fn update_message_flags_async(
    account: &RealAccount,
    folder: &str,
    uid: &str,
    mutation: MessageFlagMutation,
) -> Result<(), String> {
    let mut session = connect_imap(account).await?;
    session.select(folder).await.map_err(imap_error)?;
    let requested_uid = validated_uid_arg(uid)?;
    ensure_message_uid_exists(&mut session, requested_uid).await?;
    let uid_arg = requested_uid.to_string();
    let mut updates = session
        .uid_store(&uid_arg, mutation.imap_store_query())
        .await
        .map_err(imap_error)?;
    while updates.try_next().await.map_err(imap_error)?.is_some() {}
    drop(updates);
    let _ = session.logout().await;
    Ok(())
}

async fn move_message_to_trash_async(
    account: &RealAccount,
    folder: &str,
    uid: &str,
) -> Result<String, String> {
    let mut session = connect_imap(account).await?;
    let trash_folder = find_trash_folder(&mut session).await?;
    session.select(folder).await.map_err(imap_error)?;
    let requested_uid = validated_uid_arg(uid)?;
    ensure_message_uid_exists(&mut session, requested_uid).await?;
    let capabilities = session.capabilities().await.map_err(imap_error)?;
    let uid_arg = requested_uid.to_string();
    if capabilities.has_str("MOVE") {
        session
            .uid_mv(&uid_arg, &trash_folder)
            .await
            .map_err(imap_error)?;
    } else if capabilities.has_str("UIDPLUS") {
        session
            .uid_copy(&uid_arg, &trash_folder)
            .await
            .map_err(imap_error)?;
        let mut updates = session
            .uid_store(&uid_arg, "+FLAGS.SILENT (\\Deleted)")
            .await
            .map_err(imap_error)?;
        while updates.try_next().await.map_err(imap_error)?.is_some() {}
        drop(updates);
        {
            let expunges = session.uid_expunge(&uid_arg).await.map_err(imap_error)?;
            futures_util::pin_mut!(expunges);
            while expunges.try_next().await.map_err(imap_error)?.is_some() {}
        }
    } else {
        return Err(
            "imap_error: server does not support MOVE or UIDPLUS; refusing unsafe trash fallback"
                .to_owned(),
        );
    }
    let _ = session.logout().await;
    Ok(trash_folder)
}

async fn ensure_message_uid_exists(
    session: &mut Session<RealImapStream>,
    requested_uid: u32,
) -> Result<(), String> {
    let uid_arg = requested_uid.to_string();
    let mut fetches = session
        .uid_fetch(&uid_arg, "(UID)")
        .await
        .map_err(imap_error)?;
    let found = fetches
        .try_next()
        .await
        .map_err(imap_error)?
        .is_some_and(|fetch| fetch.uid == Some(requested_uid));
    drop(fetches);
    if found {
        Ok(())
    } else {
        Err("message_not_found: message not found".to_owned())
    }
}

async fn find_trash_folder(session: &mut Session<RealImapStream>) -> Result<String, String> {
    let mut names = session.list(None, Some("*")).await.map_err(imap_error)?;
    let mut fallback = None;
    while let Some(name) = names.try_next().await.map_err(imap_error)? {
        let selectable = !name.attributes().contains(&NameAttribute::NoSelect);
        if !selectable {
            continue;
        }
        let folder_name = name.name().to_owned();
        if name.attributes().contains(&NameAttribute::Trash) {
            return Ok(folder_name);
        }
        let delimiter = name.delimiter().unwrap_or("/");
        if fallback.is_none() && is_likely_trash_folder(&folder_name, delimiter) {
            fallback = Some(folder_name);
        }
    }
    fallback.ok_or_else(|| {
        "imap_error: trash mailbox not found; server did not advertise a selectable \\Trash folder"
            .to_owned()
    })
}

fn is_likely_trash_folder(name: &str, delimiter: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "trash" | "deleted items" | "deleted messages"
    ) {
        return true;
    }
    let suffix = format!("{delimiter}trash").to_ascii_lowercase();
    lower.ends_with(&suffix)
}

async fn send_message_async(
    account: &RealAccount,
    outgoing: &OutgoingMessage,
) -> Result<String, String> {
    let smtp = account.smtp_config()?;
    let message_id = generate_message_id(&smtp.host, outgoing);
    let email = build_lettre_message(outgoing, &message_id)?;
    if matches!(
        account.auth.as_ref().map(|auth| auth.method),
        Some(AuthMethod::Oauth2)
    ) {
        send_message_oauth2_async(account, &email).await?;
        return Ok(message_id);
    }
    let mut builder = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&smtp.host)
        .port(smtp.port)
        .timeout(Some(Duration::from_secs(smtp.timeout_seconds)))
        .tls(smtp_tls(&smtp.host, smtp.tls)?);
    if let Some(password) = resolve_password(account.auth.as_ref(), &account.secrets).await? {
        builder = builder.credentials(Credentials::new(smtp.login.clone(), password));
    }
    let mailer = builder.build();
    mailer.send(email).await.map_err(|error| {
        format!(
            "smtp_error: SMTP send via {}:{} failed: {error}",
            smtp.host, smtp.port
        )
    })?;
    Ok(message_id)
}

async fn send_message_oauth2_async(account: &RealAccount, email: &Message) -> Result<(), String> {
    let smtp = account.smtp_config()?;
    let mut access_token = account.google_access_token()?;
    let mut conn = match connect_smtp_for_auth(account).await {
        Ok(conn) => conn,
        Err(error) => {
            return Err(format!(
                "smtp_error: SMTP connection to {}:{} failed: {}",
                smtp.host,
                smtp.port,
                sanitized_backend_error(&error.to_string())
            ));
        }
    };
    if smtp_auth_xoauth2(&mut conn, smtp, &access_token)
        .await
        .is_err()
    {
        account.invalidate_google_access_token()?;
        access_token = account.google_access_token()?;
        conn = connect_smtp_for_auth(account).await.map_err(|error| {
            format!(
                "smtp_error: SMTP connection to {}:{} failed after auth retry: {}",
                smtp.host,
                smtp.port,
                sanitized_backend_error_redacting(&error.to_string(), &access_token)
            )
        })?;
        smtp_auth_xoauth2(&mut conn, smtp, &access_token)
            .await
            .map_err(|retry_error| {
                format!(
                    "auth_error: SMTP XOAUTH2 authentication failed for {}: {}",
                    smtp.login,
                    sanitized_backend_error_redacting(&retry_error.to_string(), &access_token)
                )
            })?;
    }
    conn.send(email.envelope(), &email.formatted())
        .await
        .map_err(|error| {
            format!(
                "smtp_error: SMTP send via {}:{} failed: {}",
                smtp.host,
                smtp.port,
                sanitized_backend_error(&error.to_string())
            )
        })?;
    let _ = conn.quit().await;
    Ok(())
}

async fn connect_smtp_for_auth(account: &RealAccount) -> Result<AsyncSmtpConnection, String> {
    let smtp = account.smtp_config()?;
    let client_id = ClientId::default();
    let tls_parameters = || {
        // Preserve this behavior; the structural alternative is not semantics-neutral
        // here. ast-grep-ignore: silent-map-err
        TlsParameters::builder(smtp.host.clone())
            .certificate_store(CertificateStore::WebpkiRoots)
            .build()
            .map_err(|_| "tls_error: failed to configure SMTP TLS".to_owned())
    };
    let mut conn = match smtp.tls {
        TlsMode::Required => AsyncSmtpConnection::connect_tokio1(
            (smtp.host.as_str(), smtp.port),
            Some(Duration::from_secs(smtp.timeout_seconds)),
            &client_id,
            Some(tls_parameters()?),
            None,
        )
        .await
        .map_err(|error| error.to_string())?,
        TlsMode::StartTls | TlsMode::None => AsyncSmtpConnection::connect_tokio1(
            (smtp.host.as_str(), smtp.port),
            Some(Duration::from_secs(smtp.timeout_seconds)),
            &client_id,
            None,
            None,
        )
        .await
        .map_err(|error| error.to_string())?,
    };
    if smtp.tls == TlsMode::StartTls {
        conn.starttls(tls_parameters()?, &client_id)
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(conn)
}

async fn smtp_auth_xoauth2(
    conn: &mut AsyncSmtpConnection,
    smtp: &ValidatedSmtpConfig,
    access_token: &str,
) -> Result<(), lettre::transport::smtp::Error> {
    conn.auth(
        &smtp_oauth_mechanisms(),
        &Credentials::new(smtp.login.clone(), access_token.to_owned()),
    )
    .await
    .map(|_| ())
}

async fn connect_imap(account: &RealAccount) -> Result<Session<RealImapStream>, String> {
    let imap = account.imap_config()?;
    let tcp = TcpStream::connect((imap.host.as_str(), imap.port))
        .await
        .map_err(|error| {
            format!(
                "network_error: IMAP connection to {}:{} failed: {error}",
                imap.host, imap.port
            )
        })?;
    let stream = match imap.tls {
        TlsMode::Required => RealImapStream::Tls(Box::new(tls_connect(&imap.host, tcp).await?)),
        TlsMode::StartTls | TlsMode::None => RealImapStream::Plain(tcp),
    };
    let mut client = Client::new(stream);
    read_imap_greeting(&mut client).await?;
    if imap.tls == TlsMode::StartTls {
        client
            .run_command_and_check_ok("STARTTLS", None)
            .await
            .map_err(imap_error)?;
        let tcp = match client.into_inner() {
            RealImapStream::Plain(tcp) => tcp,
            RealImapStream::Tls(_) => {
                return Err("tls_error: IMAP STARTTLS stream state was invalid".to_owned());
            }
        };
        client = Client::new(RealImapStream::Tls(Box::new(
            tls_connect(&imap.host, tcp).await?,
        )));
    }
    match account.auth.as_ref().map(|auth| auth.method) {
        Some(AuthMethod::Oauth2) => authenticate_imap_xoauth2(client, account, &imap.login).await,
        _ => {
            let password = resolve_password(account.auth.as_ref(), &account.secrets)
                .await?
                .ok_or_else(|| "auth_error: IMAP password source is not configured".to_owned())?;
            client.login(&imap.login, password).await.map_err(|error| {
                format!(
                    "auth_error: IMAP authentication failed for {}: {:?}",
                    imap.login, error.0
                )
            })
        }
    }
}

async fn authenticate_imap_xoauth2(
    mut client: Client<RealImapStream>,
    account: &RealAccount,
    login: &str,
) -> Result<Session<RealImapStream>, String> {
    for attempt in 0..2 {
        let access_token = account.google_access_token()?;
        let authenticator = Xoauth2Authenticator::new(login, &access_token);
        match client.authenticate("XOAUTH2", authenticator).await {
            Ok(session) => return Ok(session),
            Err((_, returned_client)) if attempt == 0 => {
                client = returned_client;
                account.invalidate_google_access_token()?;
            }
            Err(_) => {
                return Err(format!(
                    "auth_error: IMAP XOAUTH2 authentication failed for {}",
                    login
                ));
            }
        }
    }
    Err(format!(
        "auth_error: IMAP XOAUTH2 authentication failed for {}",
        login
    ))
}

async fn read_imap_greeting(client: &mut Client<RealImapStream>) -> Result<(), String> {
    client
        .read_response()
        .await
        .map_err(|error| format!("network_error: IMAP greeting failed: {error}"))?
        .ok_or_else(|| "network_error: IMAP server closed before greeting".to_owned())?;
    Ok(())
}

async fn tls_connect(host: &str, tcp: TcpStream) -> Result<TlsStream<TcpStream>, String> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let provider = rustls::crypto::ring::default_provider();
    // Preserve this behavior; the structural alternative is not semantics-neutral
    // here. ast-grep-ignore: silent-map-err
    let config = ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()
        .map_err(|_| "tls_error: failed to configure TLS versions".to_owned())?
        .with_root_certificates(roots)
        .with_no_client_auth();
    // Preserve this behavior; the structural alternative is not semantics-neutral
    // here. ast-grep-ignore: silent-map-err
    let server_name = ServerName::try_from(host.to_owned())
        .map_err(|_| "tls_error: invalid TLS server name".to_owned())?;
    TlsConnector::from(Arc::new(config))
        .connect(server_name, tcp)
        .await
        .map_err(|error| format!("tls_error: TLS handshake with {host} failed: {error}"))
}

fn smtp_tls(host: &str, mode: TlsMode) -> Result<Tls, String> {
    let params = || {
        // Preserve this behavior; the structural alternative is not semantics-neutral
        // here. ast-grep-ignore: silent-map-err
        TlsParameters::builder(host.to_owned())
            .certificate_store(CertificateStore::WebpkiRoots)
            .build()
            .map_err(|_| "tls_error: failed to configure SMTP TLS".to_owned())
    };
    match mode {
        TlsMode::Required => Ok(Tls::Wrapper(params()?)),
        TlsMode::StartTls => Ok(Tls::Required(params()?)),
        TlsMode::None => Ok(Tls::None),
    }
}

fn validated_uid_arg(uid: &str) -> Result<u32, String> {
    uid.parse::<u32>()
        .ok()
        .filter(|value| 0 < *value && uid.bytes().all(|byte| byte.is_ascii_digit()))
        .ok_or_else(|| "invalid_input: uid must be a positive integer".to_owned())
}

fn xoauth2_payload(login: &str, access_token: &str) -> String {
    format!("user={login}\x01auth=Bearer {access_token}\x01\x01")
}

fn sanitized_backend_error(value: &str) -> String {
    const MAX_CHARS: usize = 256;
    value
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .take(MAX_CHARS)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn sanitized_backend_error_redacting(value: &str, secret: &str) -> String {
    if secret.is_empty() {
        return sanitized_backend_error(value);
    }
    sanitized_backend_error(&value.replace(secret, "[redacted]"))
}

fn smtp_oauth_mechanisms() -> Vec<Mechanism> {
    vec![Mechanism::Xoauth2]
}

async fn resolve_password(
    auth: Option<&ValidatedAuthConfig>,
    secrets: &BTreeMap<String, tau_proto::SecretValue>,
) -> Result<Option<String>, String> {
    let Some(auth) = auth else {
        return Ok(None);
    };
    match auth.method {
        AuthMethod::None => Ok(None),
        AuthMethod::Oauth2 => {
            Err("auth_error: OAuth accounts use provider-specific access tokens".to_owned())
        }
        AuthMethod::Command => Err(
            "auth_error: password commands are no longer supported; use auth.password_secret"
                .to_owned(),
        ),
        AuthMethod::Password => {
            let Some(secret_name) = auth.password_secret.as_deref() else {
                return Err("auth_error: password secret is not configured".to_owned());
            };
            let Some(secret) = secrets.get(secret_name) else {
                return Err("auth_error: configured password secret was not provided".to_owned());
            };
            let value = secret.expose_secret();
            if value.is_empty() {
                return Err("auth_error: configured password secret is empty".to_owned());
            }
            Ok(Some(value.to_owned()))
        }
    }
}

fn metadata_from_fetch(fetch: &async_imap::types::Fetch, uidvalidity: &str) -> BackendMessage {
    let fallback = BackendMessage {
        uid: fetch.uid.unwrap_or(fetch.message).to_string(),
        uidvalidity: uidvalidity.to_owned(),
        // Preserve behavior at this site.
        // ast-grep-ignore: unwrap-or-default
        date: fetch
            .internal_date()
            .map(|date| date.to_rfc3339())
            .unwrap_or_default(),
        from: String::new(),
        to: Vec::new(),
        cc: Vec::new(),
        subject: String::new(),
        body_text: String::new(),
        source_truncated: false,
        flags: fetch.flags().map(flag_to_string).collect(),
        has_attachments: false,
        attachments: Vec::new(),
        message_id: None,
        auth_results: Vec::new(),
    };
    fetch
        .header()
        .map(|header| {
            let mut message = parse_backend_message_metadata_from_rfc822(&fallback, header);
            if METADATA_HEADER_FETCH_MAX_BYTES <= header.len() {
                message.source_truncated = true;
            }
            message
        })
        .unwrap_or(fallback)
}

fn parse_backend_message_metadata_from_rfc822(
    fallback: &BackendMessage,
    raw: &[u8],
) -> BackendMessage {
    let Some(parsed) = MessageParser::default().parse(raw) else {
        return fallback.clone();
    };
    let mut message = fallback.clone();
    apply_parsed_headers(&mut message, &parsed);
    message.auth_results = parse_authentication_results_headers(raw);
    message
}

pub(crate) fn parse_backend_message_from_rfc822(
    fallback: &BackendMessage,
    raw: &[u8],
) -> BackendMessage {
    let Some(parsed) = MessageParser::default().parse(raw) else {
        let mut message = fallback.clone();
        message.body_text = "[message body omitted: RFC822 parse failed]".to_owned();
        message.source_truncated = true;
        message.attachments.clear();
        return message;
    };
    let mut message = fallback.clone();
    apply_parsed_headers(&mut message, &parsed);
    message.auth_results = parse_authentication_results_headers(raw);
    message.body_text = parsed_body_text(&parsed);
    message.attachments = parsed
        .attachments()
        .map(|part| BackendAttachment {
            filename: part.attachment_name().map(str::to_owned),
            content_type: part.content_type().map(content_type_string),
            size_bytes: Some(part.len() as u64),
        })
        .collect();
    message.has_attachments = message.has_attachments || !message.attachments.is_empty();
    message
}

fn parse_authentication_results_headers(raw: &[u8]) -> Vec<AuthenticationResultsEvidence> {
    unfolded_header_lines(raw)
        .into_iter()
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("Authentication-Results")
                .then(|| parse_authentication_results_value(value.trim()))
                .flatten()
        })
        .collect()
}

fn unfolded_header_lines(raw: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(raw);
    let header_text = text
        .split_once("\r\n\r\n")
        .map(|(headers, _)| headers)
        .or_else(|| text.split_once("\n\n").map(|(headers, _)| headers))
        .unwrap_or(&text);
    let mut lines: Vec<String> = Vec::new();
    for line in header_text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            break;
        }
        if line.starts_with([' ', '\t']) {
            if let Some(previous) = lines.last_mut() {
                previous.push(' ');
                previous.push_str(line.trim());
            }
        } else {
            lines.push(line.to_owned());
        }
    }
    lines
}

fn parse_authentication_results_value(value: &str) -> Option<AuthenticationResultsEvidence> {
    let mut clauses = value.split(';').map(str::trim);
    let authserv_id = sanitize_auth_results_token(clauses.next()?)?;
    let mut evidence = AuthenticationResultsEvidence {
        authserv_id: authserv_id.to_ascii_lowercase(),
        ..Default::default()
    };
    for clause in clauses {
        let mut words = clause.split_whitespace();
        let Some(method_result) = words.next() else {
            continue;
        };
        let Some((method, result)) = method_result.split_once('=') else {
            continue;
        };
        let method = method.to_ascii_lowercase();
        let result = clean_auth_results_value(result)?.to_ascii_lowercase();
        match method.as_str() {
            "dmarc" => evidence.dmarc_result = Some(result),
            "dkim" => evidence.dkim_result = Some(result),
            _ => {}
        }
        for word in words {
            let Some((property, value)) = word.split_once('=') else {
                continue;
            };
            let property = property.to_ascii_lowercase();
            let value = clean_auth_results_value(value)?.to_ascii_lowercase();
            match (method.as_str(), property.as_str()) {
                ("dmarc", "header.from") => evidence.dmarc_header_from = Some(value),
                ("dkim", "header.d") => evidence.dkim_header_d = Some(value),
                _ => {}
            }
        }
    }
    Some(evidence)
}

fn sanitize_auth_results_token(value: &str) -> Option<&str> {
    let token = value.split_whitespace().next()?.trim_matches('"');
    (!token.is_empty()
        && !token
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, ';' | ',' | '<' | '>')))
    .then_some(token)
}

fn clean_auth_results_value(value: &str) -> Option<String> {
    let cleaned = value
        .trim()
        .trim_matches('"')
        .trim_end_matches(';')
        .trim_end_matches(',');
    (!cleaned.is_empty()
        && !cleaned
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, ';' | '<' | '>')))
    .then(|| cleaned.to_owned())
}

fn apply_parsed_headers(message: &mut BackendMessage, parsed: &mail_parser::Message<'_>) {
    if let Some(from) = parsed.from().and_then(parsed_address_first) {
        message.from = from;
    }
    let to = parsed_address_list(parsed.to());
    if !to.is_empty() {
        message.to = to;
    }
    let cc = parsed_address_list(parsed.cc());
    if !cc.is_empty() {
        message.cc = cc;
    }
    if let Some(subject) = parsed.subject() {
        message.subject = subject.to_owned();
    }
    if let Some(date) = parsed.date() {
        message.date = date.to_string();
    }
    if let Some(message_id) = parsed.message_id() {
        message.message_id = Some(message_id.to_owned());
    }
}

fn parsed_body_text(parsed: &mail_parser::Message<'_>) -> String {
    let mut parts = Vec::new();
    for index in 0..parsed.text_body_count() {
        if let Some(text) = parsed.body_text(index) {
            parts.push(text.into_owned());
        }
    }
    if parts.is_empty()
        && let Some(html) = parsed.body_html(0)
    {
        parts.push(html.into_owned());
    }
    parts.join("\n")
}

fn build_lettre_message(outgoing: &OutgoingMessage, message_id: &str) -> Result<Message, String> {
    let builder = Message::builder()
        .from(parse_mailbox_header(&outgoing.from, "From")?)
        .subject(outgoing.subject.clone())
        .message_id(Some(message_id.to_owned()))
        .header(LettreContentType::TEXT_PLAIN);
    let builder = outgoing.to.iter().try_fold(builder, |builder, recipient| {
        Ok::<_, String>(builder.to(parse_mailbox_header(recipient, "To")?))
    })?;
    let builder = outgoing.cc.iter().try_fold(builder, |builder, recipient| {
        Ok::<_, String>(builder.cc(parse_mailbox_header(recipient, "Cc")?))
    })?;
    let mut builder = outgoing
        .bcc
        .iter()
        .try_fold(builder, |builder, recipient| {
            Ok::<_, String>(builder.bcc(parse_mailbox_header(recipient, "Bcc")?))
        })?;
    if let Some(reply_to) = &outgoing.reply_to {
        builder = builder.reply_to(parse_mailbox_header(reply_to, "Reply-To")?);
    }
    if let Some(in_reply_to) = &outgoing.in_reply_to {
        builder = builder.in_reply_to(in_reply_to.clone());
    }
    // Preserve this behavior; the structural alternative is not semantics-neutral
    // here. ast-grep-ignore: silent-map-err
    builder
        .body(outgoing.body_text.clone())
        .map_err(|_| "smtp_error: failed to build email message".to_owned())
}

pub(crate) fn parse_mailbox_header(input: &str, field: &str) -> Result<Mailbox, String> {
    let raw = input.trim();
    if let (Some(start), Some(end)) = (raw.rfind('<'), raw.rfind('>'))
        && start < end
    {
        let name = raw[..start].trim().trim_matches('"').trim();
        let address = raw[start + 1..end].trim().trim_matches('"').trim();
        let (local, domain) = address
            .split_once('@')
            .ok_or_else(|| format!("invalid_input: invalid {field} address"))?;
        // Preserve this behavior; the structural alternative is not semantics-neutral
        // here. ast-grep-ignore: silent-map-err
        let email = lettre::Address::new(local, domain)
            .map_err(|_| format!("invalid_input: invalid {field} address"))?;
        let name = (!name.is_empty()).then(|| name.to_owned());
        return Ok(Mailbox::new(name, email));
    }
    // Preserve this behavior; the structural alternative is not semantics-neutral
    // here. ast-grep-ignore: silent-map-err
    raw.parse()
        .map_err(|_| format!("invalid_input: invalid {field} address"))
}

fn generate_message_id(host: &str, outgoing: &OutgoingMessage) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let fingerprint = super::stable_id("smtp", outgoing);
    let domain = sanitized_message_id_domain(host);
    format!("<tau-{nanos}-{fingerprint}@{domain}>")
}

fn sanitized_message_id_domain(host: &str) -> String {
    let domain = host
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-'))
        .collect::<String>();
    if domain.is_empty() {
        "tau.local".to_owned()
    } else {
        domain
    }
}

fn clone_outgoing_message(message: &OutgoingMessage) -> OutgoingMessage {
    OutgoingMessage {
        account: message.account.clone(),
        from: message.from.clone(),
        to: message.to.clone(),
        cc: message.cc.clone(),
        bcc: message.bcc.clone(),
        subject: message.subject.clone(),
        body_text: message.body_text.clone(),
        reply_to: message.reply_to.clone(),
        in_reply_to: message.in_reply_to.clone(),
    }
}

fn imap_error(error: async_imap::error::Error) -> String {
    match error {
        async_imap::error::Error::No(response) => {
            format!("imap_error: IMAP server rejected the command: {response:?}")
        }
        async_imap::error::Error::Bad(response) => {
            format!("imap_error: IMAP server rejected the command: {response:?}")
        }
        async_imap::error::Error::ConnectionLost => {
            "network_error: IMAP connection lost".to_owned()
        }
        async_imap::error::Error::Validate(_) => {
            "invalid_input: invalid IMAP command input".to_owned()
        }
        error => format!("network_error: IMAP operation failed: {error}"),
    }
}

fn parsed_address_first(address: &ParsedAddress<'_>) -> Option<String> {
    parsed_address_list(Some(address)).into_iter().next()
}

fn parsed_address_list(address: Option<&ParsedAddress<'_>>) -> Vec<String> {
    match address {
        Some(ParsedAddress::List(addresses)) => addresses
            .iter()
            .filter_map(|address| address.address.as_deref().map(str::to_owned))
            .collect(),
        Some(ParsedAddress::Group(groups)) => groups
            .iter()
            .flat_map(|group| group.addresses.iter())
            .filter_map(|address| address.address.as_deref().map(str::to_owned))
            .collect(),
        None => Vec::new(),
    }
}

fn content_type_string(content_type: &mail_parser::ContentType<'_>) -> String {
    match content_type.subtype() {
        Some(subtype) => format!("{}/{}", content_type.ctype(), subtype),
        None => content_type.ctype().to_owned(),
    }
}

fn flag_to_string(flag: Flag<'_>) -> String {
    match flag {
        Flag::Seen => "seen".to_owned(),
        Flag::Answered => "answered".to_owned(),
        Flag::Flagged => "flagged".to_owned(),
        Flag::Deleted => "deleted".to_owned(),
        Flag::Draft => "draft".to_owned(),
        Flag::Recent => "recent".to_owned(),
        Flag::MayCreate => "may_create".to_owned(),
        Flag::Custom(value) => value.trim_start_matches('\\').to_ascii_lowercase(),
    }
}

impl fmt::Debug for RealEmailBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RealEmailBackend")
            .field("accounts", &self.accounts.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ensures the IMAP XOAUTH2 SASL payload matches Gmail's documented
    /// `user=` and bearer-token control-A format exactly.
    #[test]
    fn xoauth2_payload_uses_gmail_sasl_format() {
        assert_eq!(
            xoauth2_payload("alice@example.com", "access-token"),
            "user=alice@example.com\x01auth=Bearer access-token\x01\x01"
        );
    }

    /// Ensures the SMTP OAuth path pins lettre to XOAUTH2 rather than allowing
    /// the default PLAIN/LOGIN mechanism list for bearer-token credentials.
    #[test]
    fn smtp_oauth_mechanism_selection_is_xoauth2_only() {
        assert_eq!(smtp_oauth_mechanisms(), vec![Mechanism::Xoauth2]);
    }

    /// Ensures SMTP diagnostics redact the exact bearer token before they can
    /// reach action/tool errors or logs.
    #[test]
    fn smtp_error_sanitizer_redacts_access_token() {
        let sanitized = sanitized_backend_error_redacting(
            "server rejected bearer ya29.secret-token during auth",
            "ya29.secret-token",
        );
        assert_eq!(sanitized, "server rejected bearer [redacted] during auth");
        assert!(!sanitized.contains("ya29.secret-token"));
    }
}
