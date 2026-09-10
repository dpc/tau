//! Shared mechanical implementation for distinct validated artifact identities.

/// Defines one semantic identifier with checked construction and
/// deserialization.
macro_rules! artifact_id {
    ($name:ident, $description:literal, $valid:path) => {
        #[doc = $description]
        #[derive(
            Clone, Eq, PartialEq, Ord, PartialOrd, Hash, serde::Serialize, serde::Deserialize,
        )]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(
            /// Validated canonical wire spelling.
            String,
        );

        impl $name {
            /// Validates the canonical wire spelling before creating an identity.
            pub fn parse(text: impl Into<String>) -> Result<Self, super::ArtifactError> {
                let text = text.into();
                if !$valid(&text) {
                    return Err(super::ArtifactError::Invalid);
                }
                Ok(Self(text))
            }

            /// Returns the canonical wire spelling for display or a boundary.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl TryFrom<String> for $name {
            type Error = super::ArtifactError;
            fn try_from(text: String) -> Result<Self, Self::Error> {
                Self::parse(text)
            }
        }
        impl std::str::FromStr for $name {
            type Err = super::ArtifactError;
            fn from_str(text: &str) -> Result<Self, Self::Err> {
                Self::parse(text)
            }
        }
        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
        impl std::ops::Deref for $name {
            type Target = str;
            fn deref(&self) -> &str {
                self.as_str()
            }
        }
        impl std::borrow::Borrow<str> for $name {
            fn borrow(&self) -> &str {
                self.as_str()
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(concat!(stringify!($name), "(<private>)"))
            }
        }
    };
}
pub(super) use artifact_id;
