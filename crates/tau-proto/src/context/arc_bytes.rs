use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub(super) fn serialize<S>(data: &Arc<[u8]>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serde_bytes::Bytes::new(data).serialize(serializer)
}

pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<Arc<[u8]>, D::Error>
where
    D: Deserializer<'de>,
{
    serde_bytes::ByteBuf::deserialize(deserializer).map(|bytes| Arc::from(bytes.into_vec()))
}
