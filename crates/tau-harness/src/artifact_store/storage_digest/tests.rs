use super::*;

/// Physical locators must not admit public keys, presentations, or paths.
#[test]
fn storage_digest_has_only_full_lowercase_hex_spelling() {
    let hex = "0123456789abcdef".repeat(4);
    let digest = StorageDigest::parse(&hex).expect("full lowercase digest");
    assert_eq!(digest.as_hex(), hex);
    assert_eq!(digest.key().as_str(), &hex[..24]);
    assert_eq!(format!("{digest:?}"), "StorageDigest(<private>)");
    assert_eq!(
        StorageDigest::from_hash(blake3::hash(b"data")).as_hex(),
        blake3::hash(b"data").to_hex().as_str()
    );
    for invalid in [
        hex[..24].to_owned(),
        hex.to_uppercase(),
        format!("tau-artifact:{hex}"),
        format!("<tau-artifact:{hex}>"),
        format!("{hex}/data"),
        format!(" {hex}"),
        "g".repeat(64),
        "0".repeat(63),
        "0".repeat(65),
    ] {
        assert!(StorageDigest::parse(&invalid).is_err(), "{invalid}");
    }
}
