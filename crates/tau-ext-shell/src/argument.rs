//! CBOR argument-map helpers shared by every tool.

use tau_proto::CborValue;

pub(crate) fn argument_text(arguments: &CborValue, key: &str) -> Result<String, String> {
    optional_argument_text(arguments, key)?.ok_or_else(|| format!("missing string argument: {key}"))
}

pub(crate) fn optional_argument_text(
    arguments: &CborValue,
    key: &str,
) -> Result<Option<String>, String> {
    match arguments {
        CborValue::Map(entries) => entries
            .iter()
            .find_map(|(k, v)| match k {
                CborValue::Text(k) if k == key => Some(match v {
                    CborValue::Text(value) => Ok(Some(value.clone())),
                    _ => Err(format!("argument `{key}` must be a string")),
                }),
                _ => None,
            })
            .unwrap_or(Ok(None)),
        _ => Ok(None),
    }
}

pub(crate) fn optional_argument_int_strict(
    arguments: &CborValue,
    key: &str,
) -> Result<Option<i64>, String> {
    match arguments {
        CborValue::Map(entries) => entries
            .iter()
            .find_map(|(k, v)| match k {
                CborValue::Text(k) if k == key => Some(match v {
                    CborValue::Integer(n) => i128::from(*n).try_into().map(Some).map_err(|_| {
                        format!("argument `{key}` must fit in a signed 64-bit integer")
                    }),
                    _ => Err(format!("argument `{key}` must be an integer")),
                }),
                _ => None,
            })
            .unwrap_or(Ok(None)),
        _ => Ok(None),
    }
}

pub(crate) fn optional_argument_bool(
    arguments: &CborValue,
    key: &str,
) -> Result<Option<bool>, String> {
    match arguments {
        CborValue::Map(entries) => entries
            .iter()
            .find_map(|(k, v)| match k {
                CborValue::Text(k) if k == key => Some(match v {
                    CborValue::Bool(b) => Ok(Some(*b)),
                    _ => Err(format!("argument `{key}` must be a boolean")),
                }),
                _ => None,
            })
            .unwrap_or(Ok(None)),
        _ => Ok(None),
    }
}

pub(crate) fn cbor_map_int(map: &CborValue, key: &str) -> Option<i64> {
    match map {
        CborValue::Map(entries) => entries.iter().find_map(|(k, v)| match (k, v) {
            (CborValue::Text(k), CborValue::Integer(n)) if k == key => {
                i128::from(*n).try_into().ok()
            }
            _ => None,
        }),
        _ => None,
    }
}

/// Extract a string value from a CBOR map by key.
pub(crate) fn cbor_map_text<'a>(map: &'a CborValue, key: &str) -> Option<&'a str> {
    match map {
        CborValue::Map(entries) => entries.iter().find_map(|(k, v)| match (k, v) {
            (CborValue::Text(k), CborValue::Text(v)) if k == key => Some(v.as_str()),
            _ => None,
        }),
        _ => None,
    }
}

/// Extract an array value from a CBOR map by key.
pub(crate) fn argument_array<'a>(
    arguments: &'a CborValue,
    key: &str,
) -> Result<&'a [CborValue], String> {
    match arguments {
        CborValue::Map(entries) => {
            for (k, v) in entries {
                if let (CborValue::Text(k), CborValue::Array(arr)) = (k, v)
                    && k == key
                {
                    return Ok(arr);
                }
            }
            Err(format!("missing array argument: {key}"))
        }
        _ => Err(format!("missing array argument: {key}")),
    }
}
