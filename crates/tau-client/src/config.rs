use serde::de::DeserializeOwned;

/// Decode a CBOR value into a typed configuration value.
pub(crate) fn parse_config<C: DeserializeOwned>(value: &tau_proto::CborValue) -> Result<C, String> {
    value.deserialized().map_err(|e| match e {
        ciborium::value::Error::Custom(msg) => msg,
    })
}
