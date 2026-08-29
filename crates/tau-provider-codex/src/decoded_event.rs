//! One-pass semantic decoding with an exact borrowed raw item projection.

use std::ops::Range;

use serde_json::Value;

#[cfg(test)]
thread_local! {
    static SEMANTIC_DECODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static RAW_INDEX_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// One decoded provider event and the exact raw spans needed for replay.
pub(super) struct DecodedEvent<'a> {
    /// Semantic event projection shared by every consumer.
    value: Value,
    /// Original provider bytes.
    raw: &'a str,
    /// Exact top-level `item` value, when uniquely present.
    item: Option<Range<usize>>,
}

impl<'a> DecodedEvent<'a> {
    /// Decode one event and index raw sidecars without interpreting them again.
    pub(super) fn decode(raw: &'a str) -> Result<Self, serde_json::Error> {
        #[cfg(test)]
        SEMANTIC_DECODES.set(SEMANTIC_DECODES.get() + 1);
        let value = serde_json::from_str(raw)?;
        #[cfg(test)]
        RAW_INDEX_SCANS.set(RAW_INDEX_SCANS.get() + 1);
        // The previous best-effort `RawValue` projection omitted the sidecar
        // when `item` was duplicated, while the semantic `Value` kept the last
        // member. Preserve that split behavior.
        let item = object_member(raw, "item").unwrap_or(None);
        Ok(Self { value, raw, item })
    }

    /// Return the semantic projection.
    pub(super) fn value(&self) -> &Value {
        &self.value
    }

    /// Return the exact top-level output item.
    pub(super) fn raw_item(&self) -> Option<&'a str> {
        self.item.as_ref().map(|range| &self.raw[range.clone()])
    }
}

/// Reset per-thread decode counters for production-callpath tests.
#[cfg(test)]
pub(super) fn reset_test_counts() {
    SEMANTIC_DECODES.set(0);
    RAW_INDEX_SCANS.set(0);
}

/// Return per-thread semantic-decode and raw-index counts.
#[cfg(test)]
pub(super) fn test_counts() -> (usize, usize) {
    (SEMANTIC_DECODES.get(), RAW_INDEX_SCANS.get())
}

#[derive(Clone, Copy)]
enum RawIndexError {
    Duplicate,
    Invalid,
}

fn object_member(raw: &str, target: &str) -> Result<Option<Range<usize>>, RawIndexError> {
    let bytes = raw.as_bytes();
    let mut cursor = skip_ws(bytes, 0);
    if bytes.get(cursor) != Some(&b'{') {
        return Ok(None);
    }
    cursor += 1;
    let mut found = None;
    loop {
        cursor = skip_ws(bytes, cursor);
        if bytes.get(cursor) == Some(&b'}') {
            return Ok(found);
        }
        let key_start = cursor;
        let key_end = string_end(bytes, cursor)?;
        cursor = skip_ws(bytes, key_end);
        if bytes.get(cursor) != Some(&b':') {
            return Err(RawIndexError::Invalid);
        }
        cursor = skip_ws(bytes, cursor + 1);
        let value_start = cursor;
        let value_end = value_end(bytes, cursor, 0)?;
        if json_key_matches(&raw[key_start..key_end], target)
            && found.replace(value_start..value_end).is_some()
        {
            return Err(RawIndexError::Duplicate);
        }
        cursor = skip_ws(bytes, value_end);
        match bytes.get(cursor) {
            Some(b',') => cursor += 1,
            Some(b'}') => return Ok(found),
            _ => return Err(RawIndexError::Invalid),
        }
    }
}

fn value_end(bytes: &[u8], cursor: usize, depth: u8) -> Result<usize, RawIndexError> {
    if depth == 128 {
        return Err(RawIndexError::Invalid);
    }
    match bytes.get(cursor) {
        Some(b'"') => string_end(bytes, cursor),
        Some(b'{') => compound_end(bytes, cursor, b'}', depth),
        Some(b'[') => compound_end(bytes, cursor, b']', depth),
        Some(_) => {
            let mut end = cursor;
            while let Some(byte) = bytes.get(end) {
                if matches!(byte, b',' | b'}' | b']' | b' ' | b'\n' | b'\r' | b'\t') {
                    break;
                }
                end += 1;
            }
            (cursor < end).then_some(end).ok_or(RawIndexError::Invalid)
        }
        None => Err(RawIndexError::Invalid),
    }
}

fn compound_end(
    bytes: &[u8],
    mut cursor: usize,
    close: u8,
    depth: u8,
) -> Result<usize, RawIndexError> {
    cursor += 1;
    loop {
        cursor = skip_ws(bytes, cursor);
        if bytes.get(cursor) == Some(&close) {
            return Ok(cursor + 1);
        }
        if close == b'}' {
            cursor = string_end(bytes, cursor)?;
            cursor = skip_ws(bytes, cursor);
            if bytes.get(cursor) != Some(&b':') {
                return Err(RawIndexError::Invalid);
            }
            cursor = skip_ws(bytes, cursor + 1);
        }
        cursor = value_end(bytes, cursor, depth + 1)?;
        cursor = skip_ws(bytes, cursor);
        match bytes.get(cursor) {
            Some(b',') => cursor += 1,
            Some(byte) if *byte == close => return Ok(cursor + 1),
            _ => return Err(RawIndexError::Invalid),
        }
    }
}

fn string_end(bytes: &[u8], mut cursor: usize) -> Result<usize, RawIndexError> {
    if bytes.get(cursor) != Some(&b'"') {
        return Err(RawIndexError::Invalid);
    }
    cursor += 1;
    while let Some(byte) = bytes.get(cursor) {
        match byte {
            b'"' => return Ok(cursor + 1),
            b'\\' => cursor += 2,
            _ => cursor += 1,
        }
    }
    Err(RawIndexError::Invalid)
}

fn skip_ws(bytes: &[u8], mut cursor: usize) -> usize {
    while bytes
        .get(cursor)
        .is_some_and(|byte| matches!(byte, b' ' | b'\n' | b'\r' | b'\t'))
    {
        cursor += 1;
    }
    cursor
}

fn json_key_matches(raw_key: &str, target: &str) -> bool {
    let mut raw = raw_key.as_bytes()[1..raw_key.len() - 1].iter().copied();
    let mut expected = target.bytes();
    loop {
        match (raw.next(), expected.next()) {
            (None, None) => return true,
            (None, Some(_)) | (Some(_), None) => return false,
            (Some(b'\\'), Some(expected)) => {
                let Some(escape) = raw.next() else {
                    return false;
                };
                if escape == b'u' {
                    let mut code = 0_u16;
                    for _ in 0..4 {
                        let Some(digit) = raw.next().and_then(hex_digit) else {
                            return false;
                        };
                        code = code * 16 + u16::from(digit);
                    }
                    if code != u16::from(expected) {
                        return false;
                    }
                } else if escape != expected {
                    return false;
                }
            }
            (Some(actual), Some(expected)) if actual == expected => {}
            _ => return false,
        }
    }
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
