use super::*;

/// Opaque data decoding enforces the exact root-inclusive array depth boundary.
#[test]
fn decode_enforces_depth_boundary() {
    assert!(decode(&nested_arrays(MESSAGE_EXTENSION_DATA_MAX_DEPTH)).is_ok());
    assert!(decode(&nested_arrays(MESSAGE_EXTENSION_DATA_MAX_DEPTH + 1)).is_err());
}

/// Opaque data decoding counts both map keys and values as independent nodes.
#[test]
fn decode_counts_map_keys_and_values() {
    let accepted = CborValue::Map(
        (0..(MESSAGE_EXTENSION_DATA_MAX_NODES - 1) / 2)
            .map(|index| (CborValue::Integer(index.into()), CborValue::Null))
            .collect(),
    );
    let rejected = CborValue::Map(
        (0..MESSAGE_EXTENSION_DATA_MAX_NODES.div_ceil(2))
            .map(|index| (CborValue::Integer(index.into()), CborValue::Null))
            .collect(),
    );
    assert!(decode(&accepted).is_ok());
    assert!(decode(&rejected).is_err());
}

/// Tags and tagged values each consume one node, and nested tags obey the same
/// root-inclusive depth boundary as containers.
#[test]
fn decode_counts_tag_nodes_and_depth() {
    let tags = (MESSAGE_EXTENSION_DATA_MAX_NODES - 2) / 2;
    let mut accepted = vec![CborValue::Tag(1, Box::new(CborValue::Null)); tags];
    accepted.push(CborValue::Null);
    let mut rejected = accepted.clone();
    rejected.push(CborValue::Null);
    assert!(decode(&CborValue::Array(accepted)).is_ok());
    assert!(decode(&CborValue::Array(rejected)).is_err());

    let accepted = nested_tags(MESSAGE_EXTENSION_DATA_MAX_DEPTH);
    assert_eq!(
        decode(&accepted).expect("tag depth boundary").value(),
        &accepted
    );
    assert!(decode(&nested_tags(MESSAGE_EXTENSION_DATA_MAX_DEPTH + 1)).is_err());
}

/// Encoded-size validation measures the standalone opaque value and rejects
/// the first byte string whose encoding exceeds the byte budget.
#[test]
fn decode_enforces_encoded_byte_boundary() {
    let accepted_len = largest_byte_string();
    assert!(decode(&CborValue::Bytes(vec![0; accepted_len])).is_ok());
    assert!(decode(&CborValue::Bytes(vec![0; accepted_len + 1])).is_err());
}

/// Materialized construction applies the same depth, node, and encoded-byte
/// bounds as wire decoding.
#[test]
fn constructor_enforces_all_bounds() {
    assert!(MessageExtensionData::new(nested_arrays(MESSAGE_EXTENSION_DATA_MAX_DEPTH)).is_ok());
    assert_eq!(
        MessageExtensionData::new(nested_arrays(MESSAGE_EXTENSION_DATA_MAX_DEPTH + 1)),
        Err(MessageExtensionDataError::Depth)
    );
    assert!(
        MessageExtensionData::new(CborValue::Array(vec![
            CborValue::Null;
            MESSAGE_EXTENSION_DATA_MAX_NODES - 1
        ]))
        .is_ok()
    );
    assert_eq!(
        MessageExtensionData::new(CborValue::Array(vec![
            CborValue::Null;
            MESSAGE_EXTENSION_DATA_MAX_NODES
        ])),
        Err(MessageExtensionDataError::Nodes)
    );
    let accepted_len = largest_byte_string();
    assert!(MessageExtensionData::new(CborValue::Bytes(vec![0; accepted_len])).is_ok());
    assert_eq!(
        MessageExtensionData::new(CborValue::Bytes(vec![0; accepted_len + 1])),
        Err(MessageExtensionDataError::EncodedBytes)
    );
}

/// Decode one standalone opaque CBOR value through its bounded wire adapter.
fn decode(value: &CborValue) -> Result<MessageExtensionData, ciborium::de::Error<std::io::Error>> {
    let mut encoded = Vec::new();
    ciborium::into_writer(value, &mut encoded).expect("encode fixture");
    ciborium::from_reader(encoded.as_slice())
}

/// Construct a value with the requested root-inclusive array depth.
fn nested_arrays(depth: usize) -> CborValue {
    (1..depth).fold(CborValue::Null, |value, _| CborValue::Array(vec![value]))
}

/// Construct a value with the requested root-inclusive tag depth.
fn nested_tags(depth: usize) -> CborValue {
    (1..depth).fold(CborValue::Null, |value, _| {
        CborValue::Tag(1, Box::new(value))
    })
}

/// Find the largest byte-string payload whose standalone encoding is bounded.
fn largest_byte_string() -> usize {
    let mut low = 0;
    let mut high = MESSAGE_EXTENSION_DATA_MAX_BYTES;
    while low < high {
        let middle = (low + high).div_ceil(2);
        let mut encoded = Vec::new();
        ciborium::into_writer(&CborValue::Bytes(vec![0; middle]), &mut encoded)
            .expect("encode standalone value");
        if encoded.len() <= MESSAGE_EXTENSION_DATA_MAX_BYTES {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    low
}
