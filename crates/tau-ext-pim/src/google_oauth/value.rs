//! Private semantic values carried through Google OAuth flows.

use std::fmt;

macro_rules! oauth_value {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        pub struct $name(String);

        impl $name {
            fn new(value: String) -> Self {
                Self(value)
            }

            /// Borrow the value at an explicitly authorized disclosure boundary.
            fn expose(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(concat!(stringify!($name), "(<redacted>)"))
            }
        }
    };
}

oauth_value!(
    AccessToken,
    "A validated short-lived Google OAuth bearer credential."
);
oauth_value!(
    RefreshToken,
    "A validated or configured long-lived Google OAuth refresh credential."
);
oauth_value!(
    DeviceCode,
    "A validated Google device-flow code retained for token exchange."
);
oauth_value!(
    UserCode,
    "A validated Google device-flow code intended for explicit user display."
);
oauth_value!(
    AuthorizationCode,
    "A validated Google installed-app authorization code."
);
oauth_value!(
    OauthState,
    "A generated or validated OAuth state value used for CSRF binding."
);
oauth_value!(
    PkceVerifier,
    "A generated or validated RFC 7636 PKCE verifier."
);
oauth_value!(
    LoopbackRedirectUri,
    "A generated or validated installed-app loopback redirect URI."
);

impl Clone for AccessToken {
    fn clone(&self) -> Self {
        Self::new(self.0.clone())
    }
}

impl Clone for RefreshToken {
    fn clone(&self) -> Self {
        Self::new(self.0.clone())
    }
}

impl AccessToken {
    /// Retain a provider response value after the existing OAuth validation.
    pub(crate) fn from_validated_provider(value: String) -> Self {
        Self::new(value)
    }

    /// Borrow the bearer credential at a provider adapter.
    pub(crate) fn expose_for_provider(&self) -> &str {
        self.expose()
    }
}

impl RefreshToken {
    /// Retain a provider response value after the existing OAuth validation.
    pub(crate) fn from_validated_provider(value: String) -> Self {
        Self::new(value)
    }

    /// Project an existing validated private persistence DTO field.
    pub(crate) fn from_validated_persistence(value: String) -> Self {
        Self::new(value)
    }

    /// Retain a configured-secret value under the existing config authority.
    pub(crate) fn from_configured_secret(value: String) -> Self {
        Self::new(value)
    }

    /// Borrow the refresh credential for provider token exchange.
    pub(crate) fn expose_for_provider(&self) -> &str {
        self.expose()
    }

    /// Borrow the refresh credential for the existing private persistence DTO.
    pub(crate) fn expose_for_persistence(&self) -> &str {
        self.expose()
    }
}

impl DeviceCode {
    /// Retain a provider response value after the existing OAuth validation.
    pub(crate) fn from_validated_provider(value: String) -> Self {
        Self::new(value)
    }

    /// Project an existing validated private persistence DTO field.
    pub(crate) fn from_validated_persistence(value: String) -> Self {
        Self::new(value)
    }

    /// Borrow the device code for provider token exchange.
    pub(crate) fn expose_for_provider(&self) -> &str {
        self.expose()
    }

    /// Borrow the device code for the existing private persistence DTO.
    pub(crate) fn expose_for_persistence(&self) -> &str {
        self.expose()
    }
}

impl UserCode {
    /// Retain a provider response value after the existing OAuth validation.
    pub(crate) fn from_validated_provider(value: String) -> Self {
        Self::new(value)
    }

    /// Borrow the user code for its sole standalone display boundary.
    pub(crate) fn expose_for_user_display(&self) -> &str {
        self.expose()
    }

    /// Borrow the user code for the existing private persistence DTO.
    pub(crate) fn expose_for_persistence(&self) -> &str {
        self.expose()
    }
}

impl AuthorizationCode {
    /// Retain a pasted-redirect code after all existing redirect validation.
    pub(crate) fn from_validated_redirect(value: String) -> Self {
        Self::new(value)
    }

    /// Borrow the authorization code for provider token exchange.
    pub(crate) fn expose_for_provider(&self) -> &str {
        self.expose()
    }
}

impl OauthState {
    /// Retain a value produced by the existing state generator.
    pub(crate) fn from_generator(value: String) -> Self {
        Self::new(value)
    }

    /// Project an existing validated private persistence DTO field.
    pub(crate) fn from_validated_persistence(value: String) -> Self {
        Self::new(value)
    }

    /// Borrow the state value while constructing the intentional authorization
    /// URL.
    pub(crate) fn expose_for_authorization_url(&self) -> &str {
        self.expose()
    }

    /// Borrow the state value for pasted-redirect validation.
    pub(crate) fn expose_for_redirect_validation(&self) -> &str {
        self.expose()
    }

    /// Borrow the state value for the existing private persistence DTO.
    pub(crate) fn expose_for_persistence(&self) -> &str {
        self.expose()
    }
}

impl PkceVerifier {
    /// Retain a value produced by the existing PKCE generator.
    pub(crate) fn from_generator(value: String) -> Self {
        Self::new(value)
    }

    /// Project an existing validated private persistence DTO field.
    pub(crate) fn from_validated_persistence(value: String) -> Self {
        Self::new(value)
    }

    /// Borrow the verifier to derive the authorization URL's PKCE challenge.
    pub(crate) fn expose_for_challenge(&self) -> &str {
        self.expose()
    }

    /// Borrow the verifier for provider token exchange.
    pub(crate) fn expose_for_provider(&self) -> &str {
        self.expose()
    }

    /// Borrow the verifier for the existing private persistence DTO.
    pub(crate) fn expose_for_persistence(&self) -> &str {
        self.expose()
    }
}

impl LoopbackRedirectUri {
    /// Retain a URI produced by the existing loopback generator.
    pub(crate) fn from_generator(value: String) -> Self {
        Self::new(value)
    }

    /// Project an existing validated private persistence DTO field.
    pub(crate) fn from_validated_persistence(value: String) -> Self {
        Self::new(value)
    }

    /// Borrow the URI while constructing the intentional authorization URL.
    pub(crate) fn expose_for_authorization_url(&self) -> &str {
        self.expose()
    }

    /// Borrow the URI for provider token exchange.
    pub(crate) fn expose_for_provider(&self) -> &str {
        self.expose()
    }

    /// Borrow the URI for pasted-redirect target validation.
    pub(crate) fn expose_for_redirect_validation(&self) -> &str {
        self.expose()
    }

    /// Borrow the URI for the existing private persistence DTO.
    pub(crate) fn expose_for_persistence(&self) -> &str {
        self.expose()
    }
}
