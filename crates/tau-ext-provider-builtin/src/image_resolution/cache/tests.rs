use super::*;

/// Independent entry and encoded-byte caps must evict even with shared consumer
/// Arcs; cache eviction never invalidates a request's already verified bytes.
#[test]
fn cache_enforces_entry_and_byte_limits_without_revoking_consumers() {
    let key = |index: u64| -> ArtifactKey { format!("{index:024x}").parse().expect("key") };
    let mut cache = Cache::default();
    for index in 0..MAX_CACHE_ENTRIES as u64 {
        cache.insert(key(index), Arc::from([42_u8]));
    }
    let retained = cache.get(&key(0)).expect("resident");
    cache.insert(key(64), Arc::from([43_u8]));
    assert!(cache.get(&key(1)).is_none(), "least recently used entry");
    assert!(cache.get(&key(0)).is_some(), "hit updates local recency");
    cache.clear();
    assert_eq!(&*retained, &[42]);
    for index in 0..9 {
        cache.insert(key(index), vec![index as u8; 8 * 1024 * 1024].into());
    }
    assert!(cache.get(&key(0)).is_none());
    assert_eq!(cache.bytes, MAX_CACHE_BYTES);
    assert_eq!(cache.entries.len(), 8);
}
