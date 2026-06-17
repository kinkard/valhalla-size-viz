use dashmap::DashMap;
use rustc_hash::FxBuildHasher;

use crate::tiles::Encoding;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub level: u8,
    pub tile_id: u32,
    pub encoding: Encoding,
}

/// In-memory tile-size cache. `Some(bytes)` for a known size, `None` for a confirmed 404.
pub type SizeCache = DashMap<CacheKey, Option<u64>, FxBuildHasher>;

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn key(level: u8, tile_id: u32, encoding: Encoding) -> CacheKey {
        CacheKey {
            level,
            tile_id,
            encoding,
        }
    }

    fn new_cache() -> SizeCache {
        SizeCache::with_hasher(FxBuildHasher)
    }

    #[test]
    fn new_cache_is_empty() {
        let cache = new_cache();
        assert_eq!(cache.len(), 0);
        assert!(cache.get(&key(2, 818660, Encoding::Zstd)).is_none());
    }

    #[test]
    fn insert_and_get_round_trip() {
        let cache = new_cache();
        let k = key(2, 818660, Encoding::Zstd);
        cache.insert(k, Some(2435));
        assert_eq!(cache.get(&k).map(|v| *v.value()), Some(Some(2435)));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn insert_overwrites_existing_value() {
        let cache = new_cache();
        let k = key(1, 51234, Encoding::Gzip);
        cache.insert(k, Some(100));
        cache.insert(k, Some(200));
        assert_eq!(cache.get(&k).map(|v| *v.value()), Some(Some(200)));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn separate_entries_per_encoding() {
        let cache = new_cache();
        let identity_key = key(2, 818660, Encoding::Identity);
        let gzip_key = key(2, 818660, Encoding::Gzip);
        let zstd_key = key(2, 818660, Encoding::Zstd);

        cache.insert(identity_key, Some(10_000));
        cache.insert(gzip_key, Some(4_000));
        cache.insert(zstd_key, Some(2_435));

        assert_eq!(
            cache.get(&identity_key).map(|v| *v.value()),
            Some(Some(10_000))
        );
        assert_eq!(cache.get(&gzip_key).map(|v| *v.value()), Some(Some(4_000)));
        assert_eq!(cache.get(&zstd_key).map(|v| *v.value()), Some(Some(2_435)));
        assert_eq!(cache.len(), 3);
    }

    #[test]
    fn missing_tile_cached_as_none() {
        let cache = new_cache();
        let k = key(0, 529, Encoding::Identity);
        cache.insert(k, None);
        assert_eq!(cache.get(&k).map(|v| *v.value()), Some(None));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn unknown_key_returns_none() {
        let cache = new_cache();
        cache.insert(key(0, 1, Encoding::Zstd), Some(42));
        assert!(cache.get(&key(0, 2, Encoding::Zstd)).is_none());
        assert!(cache.get(&key(1, 1, Encoding::Zstd)).is_none());
        assert!(cache.get(&key(0, 1, Encoding::Gzip)).is_none());
    }
}
