/// Encode one opaque model-visible id component.
pub(crate) fn encode_component(value: &str) -> String {
    let mut out = String::new();
    // ast-grep-ignore: filter-in-loop
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(*byte as char);
        } else {
            // Preserve this behavior; the structural alternative is not semantics-neutral
            // here. ast-grep-ignore: push-str-format
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Decode one opaque model-visible id component.
pub(crate) fn decode_component(value: &str) -> Result<String, String> {
    let mut bytes = Vec::new();
    let mut iter = value.as_bytes().iter().copied();
    while let Some(byte) = iter.next() {
        if byte == b'%' {
            let hi = iter
                .next()
                .ok_or_else(|| "opaque id contains incomplete percent escape".to_owned())?;
            let lo = iter
                .next()
                .ok_or_else(|| "opaque id contains incomplete percent escape".to_owned())?;
            let hi = hex_value(hi)
                .ok_or_else(|| "opaque id contains invalid percent escape".to_owned())?;
            let lo = hex_value(lo)
                .ok_or_else(|| "opaque id contains invalid percent escape".to_owned())?;
            bytes.push((hi << 4) | lo);
        } else {
            bytes.push(byte);
        }
    }
    // Preserve this behavior; the structural alternative is not semantics-neutral
    // here. ast-grep-ignore: silent-map-err
    String::from_utf8(bytes).map_err(|_| "opaque id is not valid UTF-8".to_owned())
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
