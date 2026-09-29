use serde::ser::Error as _;
use serde::{Serialize, Serializer};

use super::encoded_size_with_limit;

/// Confirms an exact CBOR byte limit admits the record but one fewer byte
/// rejects it.
#[test]
fn encoded_size_respects_exact_byte_boundary() {
    let value = "bounded record";
    let mut encoded = Vec::new();
    tau_proto::encode_message(&mut encoded, &value).unwrap();
    let actual = encoded.len() as u64;
    assert_eq!(encoded_size_with_limit(&value, actual), Some(actual));
    assert_eq!(encoded_size_with_limit(&value, actual - 1), None);
}

/// Confirms encoder failures cannot be mistaken for valid record lengths.
#[test]
fn encoded_size_rejects_serialization_failure() {
    struct Failing;
    impl Serialize for Failing {
        fn serialize<S: Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            Err(S::Error::custom("deliberate failure"))
        }
    }
    assert_eq!(encoded_size_with_limit(&Failing, u64::MAX), None);
}
