//! ID-token verification uses maintained JOSE primitives, not decoded claims.

use jsonwebtoken::jwk::{JwkSet, KeyAlgorithm, KeyOperations, PublicKeyUse};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;

use crate::authorization::valid_opaque;
use crate::{Error, ISSUER};

/// Claims retained only after signature and all required OIDC checks succeed.
#[derive(Clone, Deserialize)]
pub(crate) struct Identity {
    /// Stable account subject, paired with issued client for workspace
    /// identity.
    pub(crate) sub: String,
    /// Optional display hint, never used as an account key.
    pub(crate) email: Option<String>,
    /// Original authorization nonce; absent is invalid for sign-in.
    nonce: Option<String>,
    /// Issuance time required by the identity contract.
    iat: u64,
    /// Authorized party required for a multiple-audience ID token.
    azp: Option<String>,
    /// Audience shape retained to enforce multi-audience OIDC binding.
    aud: serde_json::Value,
}

/// Verify a fresh ID token against this attempt's issuer, client and nonce.
pub(crate) fn verify(
    token: &str,
    keys: &JwkSet,
    client_id: &str,
    nonce: Option<&str>,
    now_ms: u64,
) -> Result<Identity, Error> {
    let header = decode_header(token).map_err(|_| Error::InvalidIdentity)?;
    // Discovery's advertised signing algorithm is pinned, never selected from
    // an untrusted token's algorithm family.
    if header.alg != Algorithm::RS256 {
        return Err(Error::InvalidIdentity);
    }
    let key = keys
        .find(header.kid.as_deref().ok_or(Error::InvalidIdentity)?)
        .ok_or(Error::InvalidIdentity)?;
    if key
        .common
        .key_algorithm
        .is_some_and(|algorithm| algorithm != KeyAlgorithm::RS256)
        || key
            .common
            .public_key_use
            .as_ref()
            .is_some_and(|usage| *usage != PublicKeyUse::Signature)
        || key
            .common
            .key_operations
            .as_ref()
            .is_some_and(|operations| !operations.contains(&KeyOperations::Verify))
    {
        return Err(Error::InvalidIdentity);
    }
    let key = DecodingKey::from_jwk(key).map_err(|_| Error::InvalidIdentity)?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_issuer(&[ISSUER]);
    validation.set_audience(&[client_id]);
    validation.set_required_spec_claims(&["iss", "aud", "exp", "sub", "iat"]);
    validation.leeway = 5;
    validation.validate_nbf = true;
    let identity = decode::<Identity>(token, &key, &validation)
        .map_err(|_| Error::InvalidIdentity)?
        .claims;
    let multiple_audiences = identity.aud.as_array().is_some_and(|aud| aud.len() > 1);
    if !valid_opaque(&identity.sub)
        || identity.iat > now_ms / 1000 + 5
        || nonce.is_some_and(|expected| identity.nonce.as_deref() != Some(expected))
        || identity
            .azp
            .as_deref()
            .is_some_and(|party| party != client_id)
        || (multiple_audiences && identity.azp.as_deref() != Some(client_id))
    {
        return Err(Error::InvalidIdentity);
    }
    Ok(identity)
}
