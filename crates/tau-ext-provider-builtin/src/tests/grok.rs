//! Offline native Grok profile and admission oracles; never contact xAI.

use tau_proto::TokenCount;
use tau_provider_grok::credential::{Credential, Error};
use tau_provider_grok::oauth::TokenResponse;

use super::*;
use crate::grok::RetryIdentity;

/// Credential-free fixture includes unknown future selectors without guessing.
fn settings() -> Vec<u8> {
    br#"{"kind":"grok","models":[{"id":"grok-4.7","context_window":500000,
        "function_tools":true,"reasoning_efforts":["low","high","future"]}],
        "credential":{"kind":"grok_oauth","identity":"0123456789abcdef0123456789abcdef"}}"#
        .to_vec()
}

/// Synthetic grant with an independently validated subject.
pub(crate) fn credential(access: &str, now: u64) -> Credential {
    Credential::from_login(
        TokenResponse {
            access_token: access.to_owned(),
            refresh_token: Some(format!("refresh-{access}")),
            expires_in: Some(3600),
        },
        "subject-a".to_owned(),
        now,
    )
    .expect("credential")
}

/// Parse and hydrate through the same closed settings and Secret machinery.
fn profiles(credential: &Credential) -> BuiltinProviderProfiles {
    let provider = ProviderName::new("grok");
    let (profile, reference) = parse_settings_profile(&provider, &settings()).expect("settings");
    let mut profiles = BuiltinProviderProfiles {
        providers: BTreeMap::from([(provider.clone(), profile)]),
        credentials: BTreeMap::from([(provider, reference)]),
        missing_logins: Default::default(),
    };
    hydrate_profile_credentials_with(&mut profiles, |_| {
        Ok(tau_proto::ExtensionDataValue::ReadFile {
            contents: credential.encode(),
        })
    });
    profiles
}

/// Queue one exact hydrated generation without starting a network worker.
pub(crate) fn stage(
    runtime: &mut ProviderRuntime<fn(Option<&ProviderName>) -> BuiltinProviderProfiles>,
    id: &str,
    credential: &Credential,
) -> Option<grok_runtime::Key> {
    stage_generation(runtime, id, credential, blake3::hash(&credential.encode()))
}

/// Preserve exact storage bytes when a fixture adopts noncanonical JSON.
pub(crate) fn stage_generation(
    runtime: &mut ProviderRuntime<fn(Option<&ProviderName>) -> BuiltinProviderProfiles>,
    id: &str,
    credential: &Credential,
    generation: blake3::Hash,
) -> Option<grok_runtime::Key> {
    let mut prompt = minimal_prompt();
    prompt.model = "grok/grok-4.7".parse().expect("model");
    runtime
        .credential_admission
        .admissions
        .push_back(PendingPromptAdmission {
            kind: PendingPromptAdmissionKind::Initial {
                agent_prompt_id: prompt.agent_prompt_id.clone(),
                prompt,
            },
            profiles: profiles(credential),
            request_id: Some(id.to_owned()),
            observations: Some(BTreeMap::from([(
                ProviderName::new("grok"),
                CredentialObservation::Contents(generation),
            )])),
            grok_refresh: None,
            oauth_refresh: None,
            oauth_forced: false,
            receipt_observation: None,
        });
    runtime.stage_grok_refresh(id);
    runtime
        .credential_admission
        .admissions
        .back()
        .expect("admission")
        .grok_refresh
        .clone()
}

/// Settings and Debug omit bearer material; native and generic slots cannot
/// mix.
#[test]
fn grok_profile_keeps_credentials_separate_and_declares_exact_snapshot() {
    let profiles = profiles(&credential("secret-access-canary", now_ms()));
    let profile = &profiles.providers[&ProviderName::new("grok")];
    let encoded = serde_json::to_string(profile).expect("serialize");
    assert!(!encoded.contains("secret-access-canary"));
    assert!(!format!("{profile:?}").contains("secret-access-canary"));
    let models = models_for_profiles(&profiles);
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id.to_string(), "grok/grok-4.7");
    assert!(models[0].supports_standalone_compaction);
    assert!(!models[0].supports_compaction);
    assert!(models[0].cache_policy.is_none());
    assert_eq!(models[0].context_window, TokenCount::new(500_000));
    for replacement in ["oauth", "api_key", "none"] {
        let wrong = String::from_utf8(settings())
            .expect("UTF-8")
            .replace("grok_oauth", replacement);
        assert!(parse_settings_profile(&ProviderName::new("grok"), wrong.as_bytes()).is_err());
    }
}

/// Missing local credentials withdraw models and give an actionable login path.
#[test]
fn grok_missing_secret_withdraws_models_and_login_restores_them() {
    let mut snapshot = profiles(&credential("old", now_ms()));
    hydrate_profile_credentials_with(&mut snapshot, |_| {
        Err(tau_client::ExtensionDataRpcError::Harness {
            kind: tau_proto::ExtensionDataErrorKind::NotFound,
            message: "missing".to_owned(),
        })
    });
    assert!(models_for_profiles(&snapshot).is_empty());
    assert!(snapshot.missing_login(&ProviderName::new("grok")));
    assert_eq!(
        models_for_profiles(&profiles(&credential("new", now_ms()))).len(),
        1
    );
}

/// Same-generation waiters join; failure suppresses consumed-generation reuse,
/// while an explicit fresh login can enter independently.
#[test]
fn grok_refresh_coalesces_and_failed_generation_requires_new_login() {
    let mut runtime = observation_test_runtime();
    runtime.diagnostics.receipt.suppress_oauth_worker = true;
    let old = credential("old", 0);
    let key = stage(&mut runtime, "a", &old).expect("expired flight");
    assert!(stage(&mut runtime, "b", &old).as_ref() == Some(&key));
    runtime.credential_admission.admissions.pop_front();
    runtime.cancel_unobserved_grok_refreshes();
    assert!(!runtime.credential_admission.grok.is_idle());
    runtime.finish_grok_refresh(key, None, None, Err(Error::PublicationUnknown));
    assert!(runtime.credential_admission.grok.is_idle());
    assert!(
        runtime.credential_admission.admissions[0]
            .profiles
            .missing_login(&ProviderName::new("grok"))
    );
    assert!(stage(&mut runtime, "c", &old).is_none());
    let fresh = credential("fresh", now_ms());
    assert!(stage(&mut runtime, "d", &fresh).is_none());
    let last = runtime
        .credential_admission
        .admissions
        .back()
        .expect("fresh admission");
    assert!(
        resolve_prompt_backend_without_refresh(
            &last.kind.model().clone(),
            &mut last.profiles.clone(),
            &mut OAuthRefreshRejectionCache::default(),
        )
        .is_some()
    );
}

/// Every waiter observes only the validated authoritative winner, including
/// those that joined after the canonical 401 was already consumed.
#[test]
fn grok_forced_refresh_joins_once_and_rejects_unchanged_bearer() {
    for same_bearer in [false, true] {
        let mut runtime = observation_test_runtime();
        runtime.diagnostics.receipt.suppress_oauth_worker = true;
        let old = credential("old", now_ms());
        let selected = profiles(&old);
        let BuiltinProviderProfile::Grok(profile) = &selected.providers[&ProviderName::new("grok")]
        else {
            panic!("native profile")
        };
        let identity = backend_profile_identity(&PromptBackend::Grok {
            profile: Arc::new(profile.clone()),
            model_index: 0,
        })
        .expect("identity");
        runtime
            .oauth_refresh_rejections
            .record_unauthorized(ProviderName::new("grok"), identity);
        let key = stage(&mut runtime, "first", &old).expect("forced flight");
        assert!(stage(&mut runtime, "second", &old).as_ref() == Some(&key));
        assert!(
            runtime
                .credential_admission
                .admissions
                .iter()
                .all(|admission| admission.oauth_forced)
        );
        let next = credential(if same_bearer { "old" } else { "new" }, now_ms());
        runtime.finish_grok_refresh(key, None, Some(blake3::hash(&next.encode())), Ok(next));
        for admission in &runtime.credential_admission.admissions {
            assert_eq!(
                admission
                    .profiles
                    .providers
                    .contains_key(&ProviderName::new("grok")),
                !same_bearer
            );
            assert_eq!(
                models_for_profiles(&admission.profiles).is_empty(),
                same_bearer
            );
            assert!(admission.grok_refresh.is_none());
        }
    }
}

/// A retained canceled flight must finish before its live newcomer restages;
/// already-consumed unauthorized authority stays with that waiter.
#[test]
fn grok_canceled_flight_newcomer_restages_without_false_logout() {
    for forced in [false, true] {
        let mut runtime = observation_test_runtime();
        runtime.diagnostics.receipt.suppress_oauth_worker = true;
        let old = credential("old", if forced { now_ms() } else { 0 });
        if forced {
            let selected = profiles(&old);
            let BuiltinProviderProfile::Grok(profile) =
                &selected.providers[&ProviderName::new("grok")]
            else {
                panic!("native profile");
            };
            runtime.oauth_refresh_rejections.record_unauthorized(
                ProviderName::new("grok"),
                backend_profile_identity(&PromptBackend::Grok {
                    profile: Arc::new(profile.clone()),
                    model_index: 0,
                })
                .expect("identity"),
            );
        }
        let key = stage(&mut runtime, "canceled", &old).expect("flight");
        runtime.credential_admission.admissions.clear();
        runtime.cancel_unobserved_grok_refreshes();
        assert!(stage(&mut runtime, "newcomer", &old).as_ref() == Some(&key));
        runtime.credential_admission.admissions[0].request_id = None;
        runtime.finish_grok_refresh(key.clone(), None, None, Err(Error::Canceled));
        let admission = &runtime.credential_admission.admissions[0];
        assert!(admission.grok_refresh.as_ref() == Some(&key));
        assert_eq!(admission.oauth_forced, forced);
        assert!(!admission.profiles.missing_login(&ProviderName::new("grok")));
        assert!(!runtime.credential_admission.grok.is_idle());
        let next = credential("rotated", now_ms());
        runtime.finish_grok_refresh(key, None, Some(blake3::hash(&next.encode())), Ok(next));
        let admission = &runtime.credential_admission.admissions[0];
        assert!(admission.grok_refresh.is_none());
        assert_eq!(models_for_profiles(&admission.profiles).len(), 1);
    }
}

/// Preserving omitted expiry is not authority to exchange an expired saved
/// replacement repeatedly; a fresh login remains usable.
#[test]
fn grok_expired_saved_replacement_is_suppressed() {
    let mut runtime = observation_test_runtime();
    runtime.diagnostics.receipt.suppress_oauth_worker = true;
    let key = stage(&mut runtime, "original", &credential("old", 0)).expect("flight");
    let replacement = credential("replacement-with-preserved-expiry", 0);
    runtime.finish_grok_refresh(
        key,
        None,
        Some(blake3::hash(&replacement.encode())),
        Ok(replacement.clone()),
    );
    assert!(
        runtime.credential_admission.admissions[0]
            .profiles
            .missing_login(&ProviderName::new("grok"))
    );
    for id in ["next", "again"] {
        assert!(stage(&mut runtime, id, &replacement).is_none());
        assert!(
            runtime
                .credential_admission
                .admissions
                .back()
                .expect("admission")
                .profiles
                .missing_login(&ProviderName::new("grok"))
        );
    }
    assert!(runtime.credential_admission.grok.is_idle());
}

/// Native image declarations distinguish ordinary image input from opt-in tool
/// output; invalid combinations never publish.
#[test]
fn grok_image_modalities_follow_exact_native_flags() {
    use tau_proto::InputModality;
    for image_input in [false, true] {
        for native_tool_images in [false, true] {
            for function_tools in [false, true] {
                let mut snapshot = profiles(&credential("current", now_ms()));
                let BuiltinProviderProfile::Grok(profile) = snapshot
                    .providers
                    .get_mut(&ProviderName::new("grok"))
                    .expect("profile")
                else {
                    panic!("native profile");
                };
                profile.models[0].image_input = image_input;
                profile.models[0].native_tool_images = native_tool_images;
                profile.models[0].function_tools = function_tools;
                if native_tool_images && (!image_input || !function_tools) {
                    assert!(profile.validate().is_err());
                    continue;
                }
                profile.validate().expect("valid capabilities");
                let models = models_for_profiles(&snapshot);
                if image_input {
                    assert_eq!(
                        models[0].input_modalities,
                        [InputModality::Text, InputModality::Image]
                    );
                }
                if native_tool_images {
                    assert_eq!(
                        models[0].tool_result_modalities,
                        [InputModality::Text, InputModality::Image]
                    );
                }
                assert_eq!(
                    models[0].input_modalities.contains(&InputModality::Image),
                    image_input
                );
                assert_eq!(
                    models[0]
                        .tool_result_modalities
                        .contains(&InputModality::Image),
                    native_tool_images
                );
            }
        }
    }
}

/// The logical automatic retry pin permits bearer rotation but rejects another
/// validated subject or OAuth registration.
#[test]
fn grok_automatic_retry_keeps_subject_and_registration_pin() {
    let backend = |record: Credential| {
        let snapshot = profiles(&record);
        let BuiltinProviderProfile::Grok(profile) = &snapshot.providers[&ProviderName::new("grok")]
        else {
            panic!("native profile");
        };
        PromptBackend::Grok {
            profile: Arc::new(profile.clone()),
            model_index: 0,
        }
    };
    let initial = backend(credential("initial", now_ms()));
    let mut job = crate::openai_tests::scheduled_job("grok-retry-pin", "grok");
    job.pinned_grok_identity = RetryIdentity::from_backend(&initial);
    assert!(job.automatic_retry_identity_matches(&backend(credential("rotated", now_ms()))));
    let other_subject = String::from_utf8(credential("other-login", now_ms()).encode())
        .expect("UTF-8")
        .replace("subject-a", "subject-b");
    assert!(!job.automatic_retry_identity_matches(&backend(
        Credential::decode(other_subject.as_bytes()).expect("other subject")
    )));
    let mut other_registration = profiles(&credential("same-subject", now_ms()));
    let BuiltinProviderProfile::Grok(profile) = other_registration
        .providers
        .get_mut(&ProviderName::new("grok"))
        .expect("profile")
    else {
        panic!("native profile");
    };
    profile.client_id = "another-client".to_owned();
    assert!(!job.automatic_retry_identity_matches(&PromptBackend::Grok {
        profile: Arc::new(profile.clone()),
        model_index: 0
    }));
}

/// Pre-exchange generation handoff carries consumed 401 authority only while
/// the authoritative generation still contains the rejected bearer.
#[test]
fn grok_generation_handoff_preserves_only_unchanged_bearer_refresh_authority() {
    for same_bearer in [false, true] {
        let mut runtime = observation_test_runtime();
        runtime.diagnostics.receipt.suppress_oauth_worker = true;
        let old = credential("old", now_ms());
        let selected = profiles(&old);
        let BuiltinProviderProfile::Grok(profile) = &selected.providers[&ProviderName::new("grok")]
        else {
            panic!("native profile");
        };
        runtime.oauth_refresh_rejections.record_unauthorized(
            ProviderName::new("grok"),
            backend_profile_identity(&PromptBackend::Grok {
                profile: Arc::new(profile.clone()),
                model_index: 0,
            })
            .expect("identity"),
        );
        let key = stage(&mut runtime, "old", &old).expect("forced flight");
        let mut next = serde_json::to_value(&old).expect("record");
        next["refresh_token"] = serde_json::json!("new-refresh");
        if !same_bearer {
            next["access_token"] = serde_json::json!("new-access");
        }
        let bytes = serde_json::to_vec(&next).expect("bytes");
        runtime.finish_grok_generation_changed(
            key,
            Credential::decode(&bytes).expect("new generation"),
            blake3::hash(&bytes),
        );
        let admission = &runtime.credential_admission.admissions[0];
        assert_eq!(admission.oauth_forced, same_bearer);
        assert_eq!(admission.grok_refresh.is_some(), same_bearer);
        assert!(!admission.profiles.missing_login(&ProviderName::new("grok")));
    }
}
