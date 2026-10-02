//! Offline registration-retention and real loopback callback coverage.

use tokio::net::{TcpListener as AsyncListener, TcpStream};

use super::*;
use crate::chatgpt_plan::ChatGptPlanProfile;
use crate::chatgpt_plan::runtime::lock;

/// Synthetic protected credential; no production issuer or token is involved.
fn credential(token: &str) -> Credential {
    Credential::decode(
        &serde_json::to_vec(&serde_json::json!({
            "version":0, "client_id":"oaiapp_fixture", "subject":"fixture",
            "email":null, "scopes":[tau_provider_chatgpt::PLAN_SCOPE],
            "access_token":token, "refresh_token":format!("refresh-{token}"),
            "id_token":null, "expires_at_ms":crate::now_ms()+3_600_000
        }))
        .expect("fixture JSON"),
    )
    .expect("fixture credential")
}

/// A failed or canceled catalog/picker after successful authentication cannot
/// discard the issued registration, reset its Secret identity, or erase models.
#[test]
fn registration_survives_unfinished_model_selection() {
    let directory = tempfile::tempdir().expect("tempdir");
    let store = SetupStore::open_in(directory.path());
    let instance = "fixture".parse().expect("instance");
    let name = ProviderName::new("plan");
    let root = runtime_root(&store, &instance).expect("runtime root");
    let initial = store.snapshot(&instance).expect("initial");
    let first = credential("first");
    retain_registration(
        &store,
        &instance,
        &name,
        &first,
        ProfileTarget::State,
        &initial,
        &root,
    )
    .expect("retain before discovery");
    // Stop exactly where failed discovery or a canceled picker returns. The
    // next add can recover the saved client instead of dynamically registering.
    let retained = store.snapshot(&instance).expect("retained");
    assert_eq!(retained.profiles.len(), 1);
    let (profile, crate::ProviderCredential::Stored(reference)) =
        crate::parse_settings_profile(&name, &retained.profiles[0].contents).expect("profile")
    else {
        panic!("stored registration");
    };
    let BuiltinProviderProfile::ChatgptPlan(profile) = profile else {
        panic!("plan");
    };
    assert!(profile.models.is_empty());
    assert_eq!(
        saved(&store, &instance, &reference)
            .expect("saved")
            .client_id(),
        first.client_id()
    );

    let profile = BuiltinProviderProfile::ChatgptPlan(ChatGptPlanProfile {
        models: vec![
            serde_json::from_value(serde_json::json!({"id":"fixture","context_window":10000}))
                .expect("model"),
        ],
        credential: Some(first),
        session: None,
    });
    publish_registration(
        &store,
        &instance,
        &name,
        &profile,
        ProfileTarget::State,
        &retained,
        &root,
    )
    .expect("finish model selection");
    let configured = store.snapshot(&instance).expect("configured");
    let second = credential("second");
    retain_registration(
        &store,
        &instance,
        &name,
        &second,
        ProfileTarget::State,
        &configured,
        &root,
    )
    .expect("returning auth before failing discovery");
    let after = store.snapshot(&instance).expect("after aborted selection");
    assert_eq!(after.profiles[0].contents, configured.profiles[0].contents);
    assert_eq!(
        saved(&store, &instance, &reference)
            .expect("same identity")
            .access_token()
            .expect("bearer"),
        "second"
    );
    assert_eq!(after.credentials.len(), 1);
}

/// Completing a model picker is a settings update, not authority to roll back a
/// concurrent rotation or resurrect a session explicitly logged out meanwhile.
#[test]
fn model_selection_preserves_concurrent_rotation_and_logout() {
    for logout in [false, true] {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = SetupStore::open_in(directory.path());
        let instance = "fixture".parse().expect("instance");
        let name = ProviderName::new("plan");
        let root = runtime_root(&store, &instance).expect("runtime root");
        let initial = store.snapshot(&instance).expect("initial");
        let first = credential("browser-era");
        retain_registration(
            &store,
            &instance,
            &name,
            &first,
            ProfileTarget::State,
            &initial,
            &root,
        )
        .expect("retain browser grant");
        let selected = store.snapshot(&instance).expect("picker snapshot");
        let previous = &selected.profiles[0];
        let (_, crate::ProviderCredential::Stored(reference)) =
            crate::parse_settings_profile(&name, &previous.contents).expect("profile")
        else {
            panic!("stored registration");
        };
        let mut winner = credential("renewed");
        if logout {
            winner.sign_out();
        }
        {
            let _lock =
                lock(&root, reference.path(), &mut || false).expect("concurrent writer lock");
            store
                .publish_credential(
                    &instance,
                    &name,
                    previous.source,
                    &previous.contents,
                    &SecretWrite {
                        path: reference.path().clone(),
                        contents: SecretBytes::new(winner.encode().expect("winner bytes")),
                    },
                    None,
                )
                .expect("concurrent rotation or logout");
        }
        let profile = BuiltinProviderProfile::ChatgptPlan(ChatGptPlanProfile {
            models: vec![
                serde_json::from_value(serde_json::json!({
                    "id":"selected-model", "context_window":10000
                }))
                .expect("model"),
            ],
            credential: Some(first),
            session: None,
        });
        publish_registration(
            &store,
            &instance,
            &name,
            &profile,
            ProfileTarget::State,
            &selected,
            &root,
        )
        .expect("finish stale picker");
        let after = saved(&store, &instance, &reference).expect("authoritative credential");
        assert_eq!(
            after.encode().expect("after"),
            winner.encode().expect("winner")
        );
        let configured = store.snapshot(&instance).expect("settings");
        let (BuiltinProviderProfile::ChatgptPlan(profile), _) =
            crate::parse_settings_profile(&name, &configured.profiles[0].contents)
                .expect("profile")
        else {
            panic!("plan");
        };
        assert_eq!(profile.models[0].id.as_str(), "selected-model");
    }
}

/// Inspection is credential-free capability discovery, not an assertion that a
/// signed-out registration is currently usable for operational inference.
#[test]
fn inspection_preserves_unhydrated_models_without_publishing_them_live() {
    let profile = ChatGptPlanProfile {
        models: vec![
            serde_json::from_value(serde_json::json!({
                "id":"fixture", "context_window":10000
            }))
            .expect("model"),
        ],
        ..Default::default()
    };
    let mut profiles = crate::BuiltinProviderProfiles::default();
    profiles.providers.insert(
        ProviderName::new("plan"),
        BuiltinProviderProfile::ChatgptPlan(profile),
    );
    assert_eq!(
        crate::provider_catalog::models_for_inspection(&profiles).len(),
        1
    );
    assert!(crate::models_for_profiles(&profiles).is_empty());
}

/// The callback uses socket readiness and validates a complete real HTTP
/// request without involving a browser or an OAuth endpoint.
#[test]
fn callback_accepts_real_loopback_and_rejects_truncated_headers() {
    let executor = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    executor.block_on(async {
        for truncated in [false, true] {
            let listener = AsyncListener::bind(("127.0.0.1", 0)).await.expect("bind");
            let addr = listener.local_addr().expect("address");
            let (authorization, url) = Authorization::new(addr.port(), "host", None, false, crate::now_ms()).expect("authorization");
            let state = url.query_pairs().find(|(key, _)| key == "state").expect("state").1.into_owned();
            let client = tokio::spawn(async move {
                let mut stream = TcpStream::connect(addr).await.expect("connect");
                let request = format!(
                    "GET /auth/callback?state={state}&code=fixture&client_id=oaiapp_fixture HTTP/1.1\r\nHost: 127.0.0.1\r\n{}",
                    if truncated { "" } else { "\r\n" }
                );
                stream.write_all(request.as_bytes()).await.expect("send");
                stream.shutdown().await.expect("half close");
                let mut reply = Vec::new();
                stream.read_to_end(&mut reply).await.expect("reply");
            });
            assert_eq!(receive_callback(&listener, authorization).await.is_ok(), !truncated);
            client.await.expect("client");
        }
    });
}
