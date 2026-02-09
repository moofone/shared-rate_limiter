#[cfg(feature = "fast-hash")]
pub(crate) type BuildHasher = ahash::RandomState;

#[cfg(not(feature = "fast-hash"))]
pub(crate) type BuildHasher = std::collections::hash_map::RandomState;

pub(crate) type HashMap<K, V> = std::collections::HashMap<K, V, BuildHasher>;
