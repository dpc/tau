//! Interactive public sign-in and protected account registration publication.

use std::fs::{File, Permissions};
use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::Duration;

use tau_config::provider_settings::{ProviderCredentialReference, ProviderCredentialSlot};
use tau_provider_chatgpt::authorization::{Authorization, Callback};
use tau_provider_chatgpt::credential::Credential;
use tau_provider_chatgpt::oauth::Client;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener as AsyncListener;
use tokio::runtime::Builder;
use tokio::time::{Instant, timeout, timeout_at};

use crate::setup_store::{
    CredentialSetup, ProfileTarget, SecretBytes, SecretWrite, SetupProfile, SetupStore,
};
use crate::{BuiltinProviderProfile, ProviderName, ProviderSetupInput};
#[cfg(test)]
mod tests;

/// Add a new account registration and select an account-specific model.
pub(crate) fn add(
    network: &tau_provider::OutboundNetworkPolicy,
    instance: &tau_proto::ExtensionName,
    target: ProfileTarget,
) -> Result<(), Box<dyn std::error::Error>> {
    if target == ProfileTarget::Stdout {
        return Err("ChatGPT plan sign-in requires a persistent --state or --config profile; --output - is unsupported".into());
    }
    let name = crate::prompt_provider_name("chatgpt-plan")?;
    let store = SetupStore::open_default()?;
    let root = runtime_root(&store, instance)?;
    let snapshot = store.snapshot(instance)?;
    let existing = snapshot.profiles.iter().find(|profile| profile.provider == name)
        .map(|profile| -> Result<Credential, Box<dyn std::error::Error>> {
            let (parsed, reference) = crate::parse_settings_profile(&name, &profile.contents)
                .map_err(|_| "invalid existing provider profile")?;
            if !matches!(parsed, BuiltinProviderProfile::ChatgptPlan(_)) {
                return Err("Use a separate provider name; existing non-plan credentials are not imported".into());
            }
            let crate::ProviderCredential::Stored(reference) = reference else { return Err("missing registration".into()); };
            saved(&store, instance, &reference)
        }).transpose()?;
    let credential = authenticate(network, &root, existing.as_ref())?;
    retain_registration(
        &store,
        instance,
        &name,
        &credential,
        target,
        &snapshot,
        &root,
    )?;
    // A later discovery failure or canceled picker leaves this validated
    // registration recoverable under the same issued client and Secret path.
    let snapshot = store.snapshot(instance)?;
    if credential.access_token().is_err() {
        eprintln!(
            "Sign-in retained with ChatGPT plan usage disabled. No inference is available. Repeat `tau provider add chatgpt-plan` with this same provider name to grant permission and choose models."
        );
        return Ok(());
    }
    let executor = Builder::new_current_thread().enable_all().build()?;
    let catalog = executor.block_on(tau_provider_chatgpt::catalog::fetch(network, &credential))?;
    if catalog.is_empty() {
        return Err("This ChatGPT account has no displayable models.".into());
    }
    let choices = catalog
        .iter()
        .map(|model| format!("{} ({})", model.display_name, model.slug))
        .collect::<Vec<_>>();
    let index = dialoguer::Select::new()
        .with_prompt("ChatGPT account model")
        .items(&choices)
        .interact()?;
    let selected = &catalog[index];
    let context_window = match selected.context_window {
        Some(context) => context,
        None => dialoguer::Input::<u32>::new()
            .with_prompt(
                "Model context window in tokens (required; consult the model's documentation)",
            )
            .validate_with(|value: &u32| {
                if *value > 0 {
                    Ok(())
                } else {
                    Err("must be positive")
                }
            })
            .interact_text()?,
    };
    let model = serde_json::from_value(serde_json::json!({
        "id": selected.slug,
        "display_name": selected.display_name,
        "context_window": context_window
    }))?;
    publish_registration(
        &store,
        instance,
        &name,
        &BuiltinProviderProfile::ChatgptPlan(super::ChatGptPlanProfile {
            models: vec![model],
            credential: Some(credential),
            session: None,
        }),
        target,
        &snapshot,
        &root,
    )?;
    Ok(())
}

/// Save validated identity before any catalog/picker step. Returning sign-in
/// changes only Secret bytes; existing model settings and references stay
/// exact.
fn retain_registration(
    store: &SetupStore,
    instance: &tau_proto::ExtensionName,
    name: &ProviderName,
    credential: &Credential,
    target: ProfileTarget,
    snapshot: &crate::setup_store::SetupSnapshot,
    root: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(previous) = snapshot
        .profiles
        .iter()
        .find(|profile| profile.provider == *name)
    {
        let (BuiltinProviderProfile::ChatgptPlan(_), crate::ProviderCredential::Stored(reference)) =
            crate::parse_settings_profile(name, &previous.contents)
                .map_err(|_| "invalid previous registration")?
        else {
            return Err("existing provider is not a ChatGPT plan registration".into());
        };
        let _lock = super::runtime::lock(root, reference.path(), &mut || false)?;
        let current = saved(store, instance, &reference)?;
        if current.client_id() != credential.client_id()
            || current.subject() != credential.subject()
        {
            return Err("ChatGPT account changed during sign-in; nothing was replaced".into());
        }
        store.publish_credential(
            instance,
            name,
            previous.source,
            &previous.contents,
            &SecretWrite {
                path: reference.path().clone(),
                contents: SecretBytes::new(credential.encode()?),
            },
            None,
        )?;
        return Ok(());
    }
    publish_registration(
        store,
        instance,
        name,
        &BuiltinProviderProfile::ChatgptPlan(super::ChatGptPlanProfile {
            models: Vec::new(),
            credential: Some(credential.clone()),
            session: None,
        }),
        target,
        snapshot,
        root,
    )
}

/// Re-adding a disabled registration preserves its Secret identity and lock
/// domain rather than copying a renewable session into an independent slot.
fn publish_registration(
    store: &SetupStore,
    instance: &tau_proto::ExtensionName,
    name: &ProviderName,
    profile: &BuiltinProviderProfile,
    target: ProfileTarget,
    snapshot: &crate::setup_store::SetupSnapshot,
    root: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut payload =
        crate::provider_setup_payload(name, profile, ProviderSetupInput::ProfileOAuth)?;
    let previous = snapshot
        .profiles
        .iter()
        .find(|profile| profile.provider == *name);
    let mut selected = None;
    if let Some(previous) = previous {
        let (_, crate::ProviderCredential::Stored(reference)) =
            crate::parse_settings_profile(name, &previous.contents)
                .map_err(|_| "invalid previous registration")?
        else {
            return Err("existing registration has no stored credential".into());
        };
        let bytes = snapshot
            .credentials
            .get(&(
                reference.identity().clone(),
                ProviderCredentialSlot::ChatGptPlan,
            ))
            .ok_or("existing registration missing")?;
        selected = Some((reference.clone(), Credential::decode(bytes)?));
        let mut settings: serde_json::Value = serde_json::from_slice(&payload.settings)?;
        settings["credential"] = reference.to_value();
        payload.settings = serde_json::to_vec_pretty(&settings)?;
        let CredentialSetup::Stored { secret, .. } = &mut payload.credential else {
            return Err("missing new sign-in credential".into());
        };
        secret.path = reference.path().clone();
    }
    let CredentialSetup::Stored { secret, .. } = &mut payload.credential else {
        return Err("missing new sign-in credential".into());
    };
    let _lock = super::runtime::lock(root, &secret.path, &mut || false)?;
    if let Some((reference, selected)) = selected {
        let current = saved(store, instance, &reference)?;
        if current.client_id() != selected.client_id() || current.subject() != selected.subject() {
            return Err("ChatGPT account changed during setup; nothing was replaced".into());
        }
        // Model selection is not reauthentication. Preserve a concurrent
        // rotation or logout rather than reinstalling browser-era tokens.
        secret.contents = SecretBytes::new(current.encode()?);
    }
    let settings = payload.settings.clone();
    let path = store.apply_to_snapshot(
        &crate::setup_store::ProviderSetupPlan {
            extension_instance: instance.clone(),
            provider: name.clone(),
            settings: payload.settings,
            credential: payload.credential,
        },
        target,
        snapshot,
    )?;
    if target == ProfileTarget::Stdout {
        crate::write_dotfiles_profile(
            &settings,
            crate::CredentialPublication::Published,
            &mut std::io::stdout().lock(),
            &mut std::io::stderr().lock(),
        )?;
    } else if let Some(path) = path {
        eprintln!("ChatGPT plan provider '{name}' saved in {}", path.display());
    }
    eprintln!("Restart Tau for settings changes to take effect.");
    Ok(())
}

/// Reauthorize the existing issued registration; publication is coordinated but
/// the interactive browser wait never holds a credential lock.
pub(crate) fn login(
    network: &tau_provider::OutboundNetworkPolicy,
    instance: &tau_proto::ExtensionName,
    store: &SetupStore,
    name: &ProviderName,
    profile: &SetupProfile,
    reference: &ProviderCredentialReference,
) -> Result<(), Box<dyn std::error::Error>> {
    let selected = saved(store, instance, reference)?;
    let root = runtime_root(store, instance)?;
    let next = authenticate(network, &root, Some(&selected))?;
    let _lock = super::runtime::lock(&root, reference.path(), &mut || false)?;
    let current = saved(store, instance, reference)?;
    if current.client_id() != selected.client_id() || current.subject() != selected.subject() {
        return Err("ChatGPT registration changed during sign-in; nothing was replaced".into());
    }
    store.publish_credential(
        instance,
        name,
        profile.source,
        &profile.contents,
        &SecretWrite {
            path: reference.path().clone(),
            contents: SecretBytes::new(next.encode()?),
        },
        None,
    )?;
    eprintln!("ChatGPT registration signed in. Plan permission must be granted before inference.");
    Ok(())
}

/// Revoke and clear only the chosen registration, retaining its issued
/// identity. Returns false for an unrelated provider so its existing logout
/// stays intact.
pub(crate) fn logout(
    args: &[String],
    instance: &tau_proto::ExtensionName,
    network: &tau_provider::OutboundNetworkPolicy,
) -> Result<bool, Box<dyn std::error::Error>> {
    let [name] = args else {
        return Err("tau provider logout requires exactly one NAME".into());
    };
    let name = ProviderName::try_new(name.clone())?;
    let store = SetupStore::open_default()?;
    let snapshot = store.snapshot(instance)?;
    let profiles = snapshot
        .profiles
        .iter()
        .filter(|profile| profile.provider == name)
        .collect::<Vec<_>>();
    let [profile] = profiles.as_slice() else {
        return Err("provider profile is missing or duplicated".into());
    };
    let (parsed, reference) = crate::parse_settings_profile(&name, &profile.contents)
        .map_err(|_| "invalid provider profile")?;
    if !matches!(parsed, BuiltinProviderProfile::ChatgptPlan(_)) {
        return Ok(false);
    }
    let crate::ProviderCredential::Stored(reference) = reference else {
        return Err("missing ChatGPT registration".into());
    };
    let root = runtime_root(&store, instance)?;
    let _lock = super::runtime::lock(&root, reference.path(), &mut || false)?;
    let mut credential = saved(&store, instance, &reference)?;
    let executor = Builder::new_current_thread().enable_all().build()?;
    let confirmed = executor
        .block_on(Client::new(network)?.revoke(&credential))
        .is_ok();
    credential.sign_out();
    store.publish_credential(
        instance,
        &name,
        profile.source,
        &profile.contents,
        &SecretWrite {
            path: reference.path().clone(),
            contents: SecretBytes::new(credential.encode()?),
        },
        None,
    )?;
    if confirmed {
        eprintln!("Signed out of this ChatGPT registration; renewable session revoked.");
    } else {
        eprintln!(
            "Signed out locally. Remote revocation was not confirmed; disconnect the app in ChatGPT Settings if needed."
        );
    }
    Ok(true)
}

/// Load only this slot; missing registration requires adding a new account,
/// never inventing a replacement client for a returning login.
fn saved(
    store: &SetupStore,
    instance: &tau_proto::ExtensionName,
    reference: &ProviderCredentialReference,
) -> Result<Credential, Box<dyn std::error::Error>> {
    let snapshot = store.snapshot(instance)?;
    let bytes = snapshot
        .credentials
        .get(&(
            reference.identity().clone(),
            ProviderCredentialSlot::ChatGptPlan,
        ))
        .ok_or("ChatGPT registration is missing; add a new chatgpt-plan provider")?;
    Ok(Credential::decode(bytes)?)
}

/// The runtime and setup resolve to the same state-root/instance lock domain.
fn runtime_root(
    store: &SetupStore,
    instance: &tau_proto::ExtensionName,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt as _;
    let root = store.extension_runtime_root(instance)?;
    std::fs::create_dir_all(&root)?;
    if std::fs::symlink_metadata(&root)?.file_type().is_symlink() {
        return Err("extension state root must not be a symlink".into());
    }
    std::fs::set_permissions(&root, Permissions::from_mode(0o700))?;
    Ok(root)
}

/// Persist one opaque host identity before authorizing any registration.
fn host_id(root: &std::path::Path) -> Result<String, Box<dyn std::error::Error>> {
    let _lock = super::runtime::lock(
        root,
        &tau_proto::ExtensionDataPath::new("chatgpt-host"),
        &mut || false,
    )?;
    let path = root.join("chatgpt-plan-host-id");
    if path.exists() {
        let mut value = String::new();
        File::open(path)?.take(257).read_to_string(&mut value)?;
        if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
            return Err("invalid saved ChatGPT host identity".into());
        }
        return Ok(value);
    }
    let value = tau_provider_chatgpt::authorization::new_host_id();
    let mut temporary = tempfile::NamedTempFile::new_in(root)?;
    temporary.write_all(value.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary.persist_noclobber(path)?;
    File::open(root)?.sync_all()?;
    Ok(value)
}

/// Start a real loopback listener before showing the authorization URL.
fn authenticate(
    network: &tau_provider::OutboundNetworkPolicy,
    root: &std::path::Path,
    selected: Option<&Credential>,
) -> Result<Credential, Box<dyn std::error::Error>> {
    let host = host_id(root)?;
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    listener.set_nonblocking(true)?;
    let (authorization, url) = Authorization::new(
        listener.local_addr()?.port(),
        &host,
        selected.map(|credential| (credential.client_id(), credential.subject())),
        selected.is_some_and(|credential| credential.access_token().is_err()),
        crate::now_ms(),
    )?;
    eprintln!(
        "Continue with ChatGPT:\n{url}\nWaiting up to ten minutes for the loopback callback."
    );
    let runtime = Builder::new_current_thread().enable_all().build()?;
    let client = Client::new(network)?;
    let listener = {
        let _entered = runtime.enter();
        AsyncListener::from_std(listener)?
    };
    let callback = runtime.block_on(receive_callback(&listener, authorization))?;
    let restart = callback.restart(listener.local_addr()?.port(), &host, crate::now_ms())?;
    let credential = match runtime.block_on(client.exchange(callback)) {
        Ok(credential) => credential,
        Err(tau_provider_chatgpt::Error::InvalidGrant) => {
            eprintln!(
                "The authorization code expired or was rejected. The issued registration is retained for one fresh sign-in:\n{}",
                restart.1
            );
            let callback = runtime.block_on(receive_callback(&listener, restart.0))?;
            runtime.block_on(client.exchange(callback))?
        }
        Err(error) => return Err(error.into()),
    };
    eprintln!(
        "Validated ChatGPT account: {}",
        credential.email().unwrap_or("account without email")
    );
    Ok(credential)
}

/// Read bounded loopback HTTP headers; never echo callback queries or tokens.
async fn receive_callback(
    listener: &AsyncListener,
    authorization: Authorization,
) -> Result<Callback, Box<dyn std::error::Error>> {
    let (mut stream, _) = timeout(Duration::from_secs(600), listener.accept()).await??;
    let header_deadline = Instant::now() + Duration::from_secs(2);
    let mut bytes = Vec::new();
    let mut byte = [0; 1];
    while bytes.len() < 16 * 1024 && !bytes.ends_with(b"\r\n\r\n") {
        if timeout_at(header_deadline, stream.read(&mut byte)).await?? == 0 {
            break;
        }
        bytes.push(byte[0]);
    }
    if !bytes.ends_with(b"\r\n\r\n") {
        return Err("incomplete callback headers".into());
    }
    let line = std::str::from_utf8(&bytes)?
        .lines()
        .next()
        .ok_or("empty callback")?;
    let mut fields = line.split_whitespace();
    if fields.next() != Some("GET") {
        return Err("invalid callback method".into());
    }
    let target = fields
        .next()
        .filter(|target| target.starts_with("/auth/callback?"))
        .ok_or("invalid callback path")?;
    let url = url::Url::parse(&format!(
        "http://127.0.0.1:{}{target}",
        listener.local_addr()?.port()
    ))?;
    let result = authorization.callback(&url, crate::now_ms());
    let _ = timeout(Duration::from_secs(2), stream.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nCallback received. Return to Tau to check sign-in."
    )).await;
    result.map_err(Into::into)
}
