//! Offline contract oracles; fixture private key has no production authority.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde_json::{Value, json};

use crate::token_response::TokenResponse;

/// Valid signed identity grants without offline access have no renewable token,
/// remain saved, and cannot authorize plan catalog or inference access.
#[test]
fn identity_only_login_without_refresh_token_is_retained() {
    let now = now_ms();
    let signed = signed(&claims(now));
    let identity = crate::identity::verify(&signed, &keys(), "oaiapp_test", Some("nonce"), now)
        .expect("verified identity");
    let tokens = TokenResponse::parse(
        &serde_json::to_vec(&json!({
            "access_token":"identity-only",
            "id_token":signed,
            "token_type":"Bearer",
            "expires_in":3600,
            "scope":"openid profile email"
        }))
        .expect("token JSON"),
    )
    .expect("identity-only token set");
    let credential = Credential::from_login("oaiapp_test".into(), identity, tokens, now);
    let restored = Credential::decode(&credential.encode().expect("encode")).expect("decode");
    assert_eq!(restored.subject(), "subject");
    assert_eq!(restored.client_id(), "oaiapp_test");
    assert_eq!(restored.access_token(), Err(Error::PlanDisabled));
    assert_eq!(restored.refresh_token(), Err(Error::InvalidGrant));
}

use crate::authorization::Authorization;
use crate::credential::Credential;
use crate::{Error, ISSUER, PLAN_SCOPE};

/// Fixed test-only RSA modulus paired with the checked-in signing fixture.
const MODULUS: &str = "ED5A24AFB51D74ED73915D61D418E8CD304789C99A26305C123857F7D9B3041B98520BA9108F7BA703266CA97756B73CFD2E3D3B68C28AE9E60A6640FB2E31A8B1A58469F321D6840E87AC7EB6363831BD0F46F570363CEB6F44C956D6CE33D5B39F8E16F726B927D7447D07CCDAAD215DDF02D815AF65923E26978E49806CB6943D78F2A1113D07D2823F36EAEE7CD036ABAEB17A943AAA88BE4CA771D0586C4F9F00FBBCEFEF889F200BD196554245629CDC749972140F4FE3F571C10CAE8B0FBD093A3D126E49B561C5C77744A99D1D382A829F9C0DB997C17AEF85682831F91AFBE1EC357D6536DEA4A1E1BE351E857BF23FBB3B5404E2AB65176E3F2B71";

/// Current time keeps signature validation realistic without expiration
/// fixtures.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64
}

/// Build the public fixture JWK without using production discovery.
pub(crate) fn keys() -> JwkSet {
    let modulus: Vec<u8> = MODULUS
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            u8::from_str_radix(std::str::from_utf8(pair).expect("hex"), 16).expect("hex byte")
        })
        .collect();
    serde_json::from_value(json!({"keys":[{
        "kty":"RSA", "alg":"RS256", "kid":"fixture", "use":"sig",
        "n":base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(modulus),
        "e":"AQAB"
    }]}))
    .expect("fixture JWK")
}

/// Sign a claim set with a public test fixture, never a user credential.
pub(crate) fn signed(claims: &Value) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("fixture".into());
    encode(
        &header,
        claims,
        &EncodingKey::from_rsa_der(include_bytes!("fixtures/test-rsa.der")),
    )
    .expect("fixture signing")
}

/// Valid identity claims used by each focused rejection mutation.
pub(crate) fn claims(now: u64) -> Value {
    json!({
        "iss":ISSUER, "aud":"oaiapp_test", "sub":"subject",
        "iat":now / 1000, "exp":now / 1000 + 3600, "nonce":"nonce",
        "email":"test@example.invalid"
    })
}

/// Valid signatures alone must not bypass issuer/audience/time/nonce checks.
#[test]
fn identity_rejects_invalid_claims_and_signature() {
    let now = now_ms();
    let valid = claims(now);
    assert!(
        crate::identity::verify(&signed(&valid), &keys(), "oaiapp_test", Some("nonce"), now)
            .is_ok()
    );
    for (field, replacement) in [
        ("iss", json!("https://wrong.invalid")),
        ("aud", json!("oaiapp_other")),
        ("exp", json!(1)),
        ("iat", json!(now / 1000 + 60)),
        ("nonce", json!("wrong")),
        ("sub", json!("")),
        ("azp", json!("oaiapp_other")),
        ("aud", json!(["oaiapp_test", "another"])),
    ] {
        let mut invalid = valid.clone();
        invalid[field] = replacement;
        assert!(
            matches!(
                crate::identity::verify(
                    &signed(&invalid),
                    &keys(),
                    "oaiapp_test",
                    Some("nonce"),
                    now
                ),
                Err(Error::InvalidIdentity)
            ),
            "{field}"
        );
    }
    for field in ["iss", "aud", "sub", "exp", "iat", "nonce"] {
        let mut invalid = valid.clone();
        invalid.as_object_mut().expect("object").remove(field);
        assert!(
            crate::identity::verify(
                &signed(&invalid),
                &keys(),
                "oaiapp_test",
                Some("nonce"),
                now
            )
            .is_err(),
            "{field}"
        );
    }
    let token = signed(&valid);
    let (message, _) = token.rsplit_once('.').expect("JWT");
    assert!(
        crate::identity::verify(
            &format!("{message}.AA"),
            &keys(),
            "oaiapp_test",
            Some("nonce"),
            now
        )
        .is_err()
    );
}

/// First registration uses the dynamic entrypoint but only exchanges an issued
/// ID.
#[test]
fn registration_requires_issued_client_and_exact_callback() {
    for callback_case in [
        "valid",
        "missing_client",
        "dynamic",
        "wrong_state",
        "duplicate",
        "wrong_path",
        "denied",
    ] {
        let (auth, authorize) =
            Authorization::new(1455, "host-id", None, false, 0).expect("transaction");
        let query: std::collections::BTreeMap<_, _> =
            authorize.query_pairs().into_owned().collect();
        assert_eq!(query["client_id"], "dynamic_agent_client");
        assert_eq!(query["resource"], crate::RESOURCE);
        assert_eq!(query["agent_name_hint"], "Tau");
        assert!(
            query["scope"]
                .split_whitespace()
                .any(|scope| scope == PLAN_SCOPE)
        );
        let mut callback = url::Url::parse("http://127.0.0.1:1455/auth/callback").expect("URI");
        callback
            .query_pairs_mut()
            .append_pair(
                "state",
                if callback_case == "wrong_state" {
                    "wrong"
                } else {
                    &query["state"]
                },
            )
            .append_pair("code", "code");
        if callback_case != "missing_client" {
            callback.query_pairs_mut().append_pair(
                "client_id",
                if callback_case == "dynamic" {
                    "dynamic_agent_client"
                } else {
                    "oaiapp_test"
                },
            );
        }
        if callback_case == "duplicate" {
            callback.query_pairs_mut().append_pair("code", "other");
        }
        if callback_case == "wrong_path" {
            callback.set_path("/callback");
        }
        if callback_case == "denied" {
            callback
                .query_pairs_mut()
                .append_pair("error", "access_denied");
        }
        let result = auth.callback(&callback, 1);
        assert_eq!(result.is_ok(), callback_case == "valid", "{callback_case}");
    }
}

/// A rejected initial code restarts PKCE/OIDC state on the issued client,
/// without manufacturing a saved subject or registering another application.
#[test]
fn rejected_initial_code_retains_registration_but_replaces_transaction() {
    let now = now_ms();
    let (first, url) = Authorization::new(12345, "host", None, false, now).expect("authorize");
    let pairs = url
        .query_pairs()
        .collect::<std::collections::BTreeMap<_, _>>();
    let callback = url::Url::parse(&format!(
        "http://127.0.0.1:12345/auth/callback?state={}&code=abandoned&client_id=oaiapp_test",
        pairs["state"]
    ))
    .expect("callback");
    let callback = first.callback(&callback, now).expect("issued registration");
    let (retry, retry_url) = callback.restart(12346, "host", now).expect("restart");
    let retry_pairs = retry_url
        .query_pairs()
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(retry_pairs["client_id"], "oaiapp_test");
    assert!(!retry_pairs.contains_key("agent_name_hint"));
    assert_ne!(pairs["state"], retry_pairs["state"]);
    assert_ne!(pairs["nonce"], retry_pairs["nonce"]);
    assert_ne!(pairs["code_challenge"], retry_pairs["code_challenge"]);
    assert!(retry.subject.is_none());
    let callback = url::Url::parse(&format!(
        "http://127.0.0.1:12346/auth/callback?state={}&code=fresh",
        retry_pairs["state"]
    ))
    .expect("fresh callback");
    assert_eq!(
        retry
            .callback(&callback, now)
            .expect("returning issued client")
            .client_id,
        "oaiapp_test"
    );
}

/// Returning login cannot replace its workspace registration through callback
/// data.
#[test]
fn returning_login_pins_client_and_expires_transactions() {
    for (returned, time, accepted) in [
        (None, 1, true),
        (Some("oaiapp_test"), 1, true),
        (Some("oaiapp_other"), 1, false),
        (None, 600_000, false),
    ] {
        let (auth, url) =
            Authorization::new(1456, "host", Some(("oaiapp_test", "subject")), true, 0)
                .expect("transaction");
        let query: std::collections::BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        assert!(!query.contains_key("agent_name_hint"));
        assert_eq!(query["prompt"], "consent");
        let mut callback =
            url::Url::parse("http://127.0.0.1:1456/auth/callback").expect("callback");
        callback
            .query_pairs_mut()
            .append_pair("state", &query["state"])
            .append_pair("code", "code");
        if let Some(client) = returned {
            callback.query_pairs_mut().append_pair("client_id", client);
        }
        assert_eq!(auth.callback(&callback, time).is_ok(), accepted);
    }
}

/// Scope denial is retained as identity-only state and logout retains
/// registration.
#[test]
fn credential_permission_rotation_and_logout_are_separate_from_identity() {
    let now = now_ms();
    let identity = crate::identity::verify(
        &signed(&claims(now)),
        &keys(),
        "oaiapp_test",
        Some("nonce"),
        now,
    )
    .expect("identity");
    let tokens = |scope, bearer| {
        TokenResponse::parse(
            &serde_json::to_vec(&json!({
                "access_token":bearer, "refresh_token":"refresh",
                "id_token":"signed-token", "token_type":"Bearer",
                "expires_in":3600, "scope":scope
            }))
            .expect("JSON"),
        )
        .expect("tokens")
    };
    let mut credential = Credential::from_login(
        "oaiapp_test".into(),
        identity,
        tokens("openid", "first"),
        now,
    );
    assert_eq!(credential.access_token(), Err(Error::PlanDisabled));
    credential.replace(tokens(PLAN_SCOPE, "second"), now);
    assert_eq!(credential.access_token(), Ok("second"));
    assert!(!credential.needs_refresh(now));
    assert!(credential.needs_refresh(now + 3_600_000));
    let mut roundtrip =
        Credential::decode(&credential.encode().expect("encoded")).expect("decoded");
    roundtrip.sign_out();
    assert_eq!(roundtrip.client_id(), "oaiapp_test");
    assert_eq!(roundtrip.subject(), "subject");
    assert!(roundtrip.access_token().is_err());
    let encoded = String::from_utf8(roundtrip.encode().expect("encoded")).expect("UTF8");
    assert!(!encoded.contains("second"));
    assert!(!encoded.contains("signed-token"));
    assert!(!format!("{credential:?}").contains("second"));
}
