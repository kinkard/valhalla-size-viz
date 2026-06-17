use dashmap::DashMap;
use rustc_hash::FxBuildHasher;

use crate::tiles::Encoding;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub level: u8,
    pub tile_id: u32,
    pub encoding: Encoding,
}

pub struct SizeCache {
    entries: DashMap<CacheKey, Option<u64>, FxBuildHasher>,
}

impl SizeCache {
    pub fn new() -> Self {
        Self {
            entries: DashMap::with_hasher(FxBuildHasher),
        }
    }

    pub fn get(&self, key: CacheKey) -> Option<Option<u64>> {
        self.entries.get(&key).map(|v| *v)
    }

    pub fn insert(&self, key: CacheKey, value: Option<u64>) {
        self.entries.insert(key, value);
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

impl Default for SizeCache {
    fn default() -> Self {
        Self::new()
    }
}

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

    #[test]
    fn new_cache_is_empty() {
        let cache = SizeCache::new();
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.get(key(2, 818660, Encoding::Zstd)), None);
    }

    #[test]
    fn insert_and_get_round_trip() {
        let cache = SizeCache::new();
        let k = key(2, 818660, Encoding::Zstd);
        cache.insert(k, Some(2435));
        assert_eq!(cache.get(k), Some(Some(2435)));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn insert_overwrites_existing_value() {
        let cache = SizeCache::new();
        let k = key(1, 51234, Encoding::Gzip);
        cache.insert(k, Some(100));
        cache.insert(k, Some(200));
        assert_eq!(cache.get(k), Some(Some(200)));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn separate_entries_per_encoding() {
        let cache = SizeCache::new();
        let identity_key = key(2, 818660, Encoding::Identity);
        let gzip_key = key(2, 818660, Encoding::Gzip);
        let zstd_key = key(2, 818660, Encoding::Zstd);

        cache.insert(identity_key, Some(10_000));
        cache.insert(gzip_key, Some(4_000));
        cache.insert(zstd_key, Some(2_435));

        assert_eq!(cache.get(identity_key), Some(Some(10_000)));
        assert_eq!(cache.get(gzip_key), Some(Some(4_000)));
        assert_eq!(cache.get(zstd_key), Some(Some(2_435)));
        assert_eq!(cache.len(), 3);
    }

    #[test]
    fn missing_tile_cached_as_none() {
        let cache = SizeCache::new();
        let k = key(0, 529, Encoding::Identity);
        cache.insert(k, None);
        assert_eq!(cache.get(k), Some(None));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn unknown_key_returns_none() {
        let cache = SizeCache::new();
        cache.insert(key(0, 1, Encoding::Zstd), Some(42));
        assert_eq!(cache.get(key(0, 2, Encoding::Zstd)), None);
        assert_eq!(cache.get(key(1, 1, Encoding::Zstd)), None);
        assert_eq!(cache.get(key(0, 1, Encoding::Gzip)), None);
    }
}
