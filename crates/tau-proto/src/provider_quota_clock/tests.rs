use super::{QuotaWindowSeconds, ServerOffsetMillis, SignedSeconds, UnixMillis, UnixSeconds};

macro_rules! assert_transparent_scalar_codec {
    ($test_name:ident, $type:ty, $scalar:ty, [$($value:expr),+ $(,)?]) => {
        /// Ensures this quota clock domain keeps its legacy scalar JSON and CBOR
        /// representation for zero, ordinary, and boundary values.
        #[test]
        fn $test_name() {
            $(
                let value: $scalar = $value;
                let wrapped = <$type>::new(value);
                let encoded = serde_json::to_string(&wrapped).expect("clock serializes to JSON");
                assert_eq!(encoded, serde_json::to_string(&value).expect("scalar serializes to JSON"));
                assert_eq!(
                    serde_json::from_str::<$type>(&encoded).expect("legacy scalar JSON decodes"),
                    wrapped
                );

                let mut wrapped_cbor = Vec::new();
                ciborium::into_writer(&wrapped, &mut wrapped_cbor).expect("clock serializes to CBOR");
                let mut scalar_cbor = Vec::new();
                ciborium::into_writer(&value, &mut scalar_cbor).expect("scalar serializes to CBOR");
                assert_eq!(wrapped_cbor, scalar_cbor);
                assert_eq!(
                    ciborium::from_reader::<$type, _>(wrapped_cbor.as_slice())
                        .expect("legacy scalar CBOR decodes"),
                    wrapped
                );
            )+
        }
    };
}

assert_transparent_scalar_codec!(
    unix_millis_preserves_scalar_codecs,
    UnixMillis,
    u64,
    [0, 1, u64::MAX]
);
assert_transparent_scalar_codec!(
    unix_seconds_preserves_scalar_codecs,
    UnixSeconds,
    u64,
    [0, 1, u64::MAX]
);
assert_transparent_scalar_codec!(
    quota_window_seconds_preserves_scalar_codecs,
    QuotaWindowSeconds,
    u64,
    [0, 1, u64::MAX]
);
assert_transparent_scalar_codec!(
    signed_seconds_preserves_scalar_codecs,
    SignedSeconds,
    i64,
    [i64::MIN, -1, 0, 1, i64::MAX]
);
assert_transparent_scalar_codec!(
    server_offset_millis_preserves_scalar_codecs,
    ServerOffsetMillis,
    i64,
    [i64::MIN, -1, 0, 1, i64::MAX]
);
