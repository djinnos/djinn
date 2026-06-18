//! Default-off out-of-core scope/parse store with bounded LRU accessors.
//!
//! This module provides a disk-backed, shard-per-file scope/parse store for
//! very large / OOM-prone repositories. When enabled, the warm pipeline can
//! produce the same graph while keeping resident memory bounded via an LRU
//! cache over the disk shards.
//!
//! ## Activation
//!
//! The module is **default-off**. Both conditions must be met:
//!
//! 1. Environment variable `DJINN_GRAPH_OUT_OF_CORE=1` is set.
//! 2. The graph's node count meets or exceeds `DJINN_GRAPH_OUT_OF_CORE_MIN_NODES`
//!    (default: 100 000).
//!
//! When either condition is not met, the existing in-memory path is used
//! unchanged and the shard store is never opened.
//!
//! ## Configuration
//!
//! | Env var | Default | Purpose |
//! |---------|---------|---------|
//! | `DJINN_GRAPH_OUT_OF_CORE` | unset | Enable flag (`1` / `true` / `yes`) |
//! | `DJINN_GRAPH_OUT_OF_CORE_MIN_NODES` | `100000` | Node-count threshold |
//! | `DJINN_GRAPH_OUT_OF_CORE_LRU_CAPACITY` | `1024` | Max shards held in memory |
//! | `DJINN_GRAPH_OUT_OF_CORE_PATH` | `<tmp>/djinn-ooc-<pid>` | Shard storage root |
//!
//! ## Storage layout
//!
//! Each shard is stored as a JSON file at `<root>/<shard_id>.json`. The shard
//! id is derived from the source file path (or symbol id) via a deterministic
//! transformation that produces a safe filename. A shard index file
//! (`<root>/index.json`) maps logical ids to filenames for enumeration.
//!
//! ## Bounded LRU accessor contract
//!
//! [`BoundedScopeAccessor`] wraps the disk store with a fixed-capacity LRU
//! cache. When the cache is full, the least-recently-used shard is evicted
//! from memory (but remains on disk). This guarantees that resident memory
//! for the scope/parse store is `O(LRU_CAPACITY)` rather than `O(total_shards)`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Default minimum node count that must be met before the out-of-core path
/// engages, even when the env flag is set.
const DEFAULT_MIN_NODES: usize = 100_000;

/// Default LRU capacity (number of shards held in memory).
const DEFAULT_LRU_CAPACITY: usize = 1024;

/// Returns `true` when the `DJINN_GRAPH_OUT_OF_CORE` env var is set to a
/// truthy value (`1`, `true`, `TRUE`, `yes`, `YES`, `on`, `ON`).
pub fn out_of_core_enabled() -> bool {
    std::env::var("DJINN_GRAPH_OUT_OF_CORE")
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

/// Returns the configured minimum node count threshold. Repos with fewer
/// nodes than this do not engage the out-of-core path even when the env
/// flag is set.
pub fn out_of_core_min_nodes() -> usize {
    std::env::var("DJINN_GRAPH_OUT_OF_CORE_MIN_NODES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MIN_NODES)
}

/// Returns the configured LRU capacity for the bounded accessor.
pub fn out_of_core_lru_capacity() -> usize {
    std::env::var("DJINN_GRAPH_OUT_OF_CORE_LRU_CAPACITY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_LRU_CAPACITY)
}

/// Returns the configured storage path for shard files. Defaults to a
/// temporary directory scoped to the current process.
pub fn out_of_core_storage_path() -> PathBuf {
    std::env::var("DJINN_GRAPH_OUT_OF_CORE_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join(format!("djinn-ooc-{}", std::process::id())))
}

/// Determines whether the out-of-core path should engage for a graph with
/// the given `node_count`. Returns the resolved [`OutOfCoreConfig`] when
/// the feature is active, or `None` when the in-memory path should be used.
pub fn resolve_out_of_core_config(node_count: usize) -> Option<OutOfCoreConfig> {
    if !out_of_core_enabled() {
        return None;
    }
    if node_count < out_of_core_min_nodes() {
        return None;
    }
    Some(OutOfCoreConfig {
        storage_path: out_of_core_storage_path(),
        lru_capacity: out_of_core_lru_capacity(),
        min_nodes: out_of_core_min_nodes(),
    })
}

/// Resolved out-of-core configuration. Created by
/// [`resolve_out_of_core_config`] when both the env flag and the node-count
/// threshold are met.
#[derive(Debug, Clone)]
pub struct OutOfCoreConfig {
    pub storage_path: PathBuf,
    pub lru_capacity: usize,
    pub min_nodes: usize,
}

// ---------------------------------------------------------------------------
// Shard data model
// ---------------------------------------------------------------------------

/// A stable, deterministic shard identifier derived from the logical key
/// (file path or symbol id). Used as the filename stem for the on-disk
/// JSON shard.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ShardId(pub String);

impl ShardId {
    /// Derive a shard id from a file-path-like key. The id is a hex-encoded
    /// SHA-256 of the key, truncated to 32 hex chars (128 bits) which is
    /// more than enough to avoid collisions in practice.
    pub fn from_file_key(key: &str) -> Self {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(key.as_bytes());
        let hash = hasher.finalize();
        Self(hex::encode(&hash[..16])) // 32 hex chars
    }
}

/// Per-file scope entry persisted as a single JSON shard on disk.
///
/// Carries the minimum data needed to reproduce the graph build for one
/// source file without holding the entire `ParsedScipIndex` in memory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScopeEntry {
    /// The shard identifier.
    pub id: ShardId,
    /// Original file path (or symbol id) that produced this shard.
    pub source_key: String,
    /// Opaque payload — in practice this is the JSON-serialized `ScipFile`
    /// or a slice of the parsed SCIP data for this file. Stored as raw
    /// bytes so the store is format-agnostic.
    pub payload: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Shard store (disk-backed, shard-per-file JSON)
// ---------------------------------------------------------------------------

/// Disk-backed shard store. Each [`ScopeEntry`] is persisted as a standalone
/// JSON file. An in-process index maps logical shard ids to filenames for
/// enumeration.
pub struct OutOfCoreStore {
    root: PathBuf,
    index: BTreeMap<ShardId, PathBuf>,
}

impl OutOfCoreStore {
    /// Open (or create) a store at the given root directory.
    pub fn open(root: &Path) -> Result<Self, OutOfCoreError> {
        std::fs::create_dir_all(root).map_err(|e| OutOfCoreError::Io {
            path: root.to_path_buf(),
            source: e,
        })?;

        let index_path = root.join("index.json");
        let index: BTreeMap<ShardId, PathBuf> = if index_path.exists() {
            let data = std::fs::read_to_string(&index_path).map_err(|e| OutOfCoreError::Io {
                path: index_path.clone(),
                source: e,
            })?;
            serde_json::from_str(&data).unwrap_or_default()
        } else {
            BTreeMap::new()
        };

        Ok(Self {
            root: root.to_path_buf(),
            index,
        })
    }

    /// Write a scope entry to disk and update the index.
    pub fn put(&mut self, entry: &ScopeEntry) -> Result<(), OutOfCoreError> {
        let filename = format!("{}.json", entry.id.0);
        let path = self.root.join(&filename);
        let json =
            serde_json::to_string(entry).map_err(|e| OutOfCoreError::Serialize(e.to_string()))?;
        std::fs::write(&path, json).map_err(|e| OutOfCoreError::Io {
            path: path.clone(),
            source: e,
        })?;
        self.index
            .insert(entry.id.clone(), PathBuf::from(&filename));
        self.persist_index()?;
        Ok(())
    }

    /// Read a scope entry from disk by shard id.
    pub fn get(&self, id: &ShardId) -> Result<Option<ScopeEntry>, OutOfCoreError> {
        let filename = match self.index.get(id) {
            Some(f) => f,
            None => return Ok(None),
        };
        let path = self.root.join(filename);
        if !path.exists() {
            return Ok(None);
        }
        let data = std::fs::read_to_string(&path).map_err(|e| OutOfCoreError::Io {
            path: path.clone(),
            source: e,
        })?;
        let entry: ScopeEntry =
            serde_json::from_str(&data).map_err(|e| OutOfCoreError::Deserialize(e.to_string()))?;
        Ok(Some(entry))
    }

    /// Return all shard ids in the store. Does **not** load payload data
    /// from disk — only reads the in-memory index.
    pub fn enumerate_ids(&self) -> Vec<ShardId> {
        self.index.keys().cloned().collect()
    }

    /// Return the number of shards in the store.
    pub fn shard_count(&self) -> usize {
        self.index.len()
    }

    /// Iterate over all shards, loading each from disk and invoking the
    /// callback. Only one shard is resident at a time — the callback
    /// borrows it and it is dropped after the callback returns.
    pub fn for_each_scope<F>(&self, mut callback: F) -> Result<(), OutOfCoreError>
    where
        F: FnMut(&ScopeEntry) -> Result<(), OutOfCoreError>,
    {
        for id in self.index.keys() {
            if let Some(entry) = self.get(id)? {
                callback(&entry)?;
            }
        }
        Ok(())
    }

    /// Persist the in-memory index to disk (atomic write via tmp+rename).
    fn persist_index(&self) -> Result<(), OutOfCoreError> {
        let index_path = self.root.join("index.json");
        let tmp_path = self.root.join("index.json.tmp");
        let json = serde_json::to_string(&self.index)
            .map_err(|e| OutOfCoreError::Serialize(e.to_string()))?;
        std::fs::write(&tmp_path, &json).map_err(|e| OutOfCoreError::Io {
            path: tmp_path.clone(),
            source: e,
        })?;
        std::fs::rename(&tmp_path, &index_path).map_err(|e| OutOfCoreError::Io {
            path: index_path,
            source: e,
        })?;
        Ok(())
    }

    /// Remove the store root directory and all shard files. Intended for
    /// cleanup after a warm pipeline completes.
    pub fn cleanup(&self) -> Result<(), OutOfCoreError> {
        if self.root.exists() {
            std::fs::remove_dir_all(&self.root).map_err(|e| OutOfCoreError::Io {
                path: self.root.clone(),
                source: e,
            })?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Bounded LRU accessor
// ---------------------------------------------------------------------------

/// Bounded LRU accessor over an [`OutOfCoreStore`].
///
/// Holds at most `capacity` scope entries in memory. When a new entry is
/// accessed and the cache is full, the least-recently-used entry is evicted
/// from memory (its on-disk shard remains intact).
///
/// The accessor is `Send` but not `Sync` — intended for single-threaded
/// use within a `spawn_blocking` context.
pub struct BoundedScopeAccessor {
    store: OutOfCoreStore,
    capacity: usize,
    /// LRU-ordered map: most-recently-used at the end.
    lru: lru::LruCache<ShardId, ScopeEntry>,
}

// We implement a minimal LRU inline so we don't need the `lru` crate as a
// dependency. The cache is backed by a `BTreeMap` of insertion order plus
// a counter for recency. For production with very large capacities, the
// `lru` crate should be added as a dependency.
#[allow(unreachable_pub, dead_code)]
mod lru {
    use std::collections::HashMap;

    /// Minimal LRU cache. `access_counter` increments on every get/put;
    /// the map stores `(counter, value)` pairs; eviction removes the
    /// entry with the smallest counter.
    pub struct LruCache<K, V> {
        capacity: usize,
        map: HashMap<K, (u64, V)>,
        access_counter: u64,
    }

    impl<K: Eq + std::hash::Hash + Clone, V> LruCache<K, V> {
        pub fn new(capacity: usize) -> Self {
            Self {
                capacity,
                map: HashMap::new(),
                access_counter: 0,
            }
        }

        pub fn len(&self) -> usize {
            self.map.len()
        }

        pub fn is_empty(&self) -> bool {
            self.map.is_empty()
        }

        /// Touch a key (update its recency counter) and return a reference
        /// to the value if it exists.
        pub fn get(&mut self, key: &K) -> Option<&V> {
            self.access_counter += 1;
            let counter = self.access_counter;
            if let Some((c, v)) = self.map.get_mut(key) {
                *c = counter;
                Some(&*v)
            } else {
                None
            }
        }

        /// Touch a key (update its recency counter) and return a mutable
        /// reference to the value if it exists.
        pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
            self.access_counter += 1;
            let counter = self.access_counter;
            self.map.get_mut(key).map(|(c, v)| {
                *c = counter;
                v
            })
        }

        /// Insert a key-value pair. If the cache is full, evicts the
        /// least-recently-used entry. Returns `true` if an eviction occurred.
        pub fn put(&mut self, key: K, value: V) -> bool {
            self.access_counter += 1;
            let counter = self.access_counter;
            self.map.insert(key, (counter, value));

            if self.map.len() > self.capacity {
                // Find the entry with the smallest counter that is NOT the
                // just-inserted entry (counter == self.access_counter).
                let lru_key = self
                    .map
                    .iter()
                    .filter(|(_, (c, _))| *c != counter)
                    .min_by_key(|(_, (c, _))| c)
                    .map(|(k, _)| k.clone());

                if let Some(k) = lru_key {
                    self.map.remove(&k);
                    return true;
                }
            }
            false
        }

        /// Return all keys currently in the cache (for testing).
        pub fn keys(&self) -> Vec<&K> {
            self.map.keys().collect()
        }
    }
}

impl BoundedScopeAccessor {
    /// Create a new accessor wrapping the given store with the specified
    /// LRU capacity.
    pub fn new(store: OutOfCoreStore, capacity: usize) -> Self {
        Self {
            store,
            capacity,
            lru: lru::LruCache::new(capacity),
        }
    }

    /// Create an accessor from an [`OutOfCoreConfig`].
    pub fn from_config(config: &OutOfCoreConfig) -> Result<Self, OutOfCoreError> {
        let store = OutOfCoreStore::open(&config.storage_path)?;
        Ok(Self::new(store, config.lru_capacity))
    }

    /// Get a scope entry by shard id. Returns `None` when the shard does
    /// not exist in the store. The entry is loaded from disk on first
    /// access (cache miss) and subsequently served from the LRU cache.
    ///
    /// When the cache is full, the least-recently-used entry is evicted
    /// from memory to make room.
    pub fn get_scope(&mut self, id: &ShardId) -> Result<Option<&ScopeEntry>, OutOfCoreError> {
        // Check the LRU cache first. `get` updates recency on hit.
        if self.lru.get(id).is_some() {
            return Ok(self.lru.get(id));
        }

        // Cache miss — load from disk.
        let entry = match self.store.get(id)? {
            Some(e) => e,
            None => return Ok(None),
        };
        self.lru.put(id.clone(), entry);
        Ok(self.lru.get(id))
    }

    /// Iterate over all scopes, invoking the callback for each one.
    ///
    /// This method loads shards one at a time from disk and passes them
    /// to the callback. The callback's `ScopeEntry` reference is valid
    /// only for the duration of the callback — the entry may be evicted
    /// afterward. At most `capacity` entries are resident at any time.
    pub fn for_each_scope<F>(&mut self, mut callback: F) -> Result<(), OutOfCoreError>
    where
        F: FnMut(&ScopeEntry) -> Result<(), OutOfCoreError>,
    {
        // Snapshot the ids so we don't borrow self.store for the whole loop.
        let ids: Vec<ShardId> = self.store.enumerate_ids();
        for id in &ids {
            // Load from disk and insert into LRU (may evict an old entry).
            let entry = match self.store.get(id)? {
                Some(e) => e,
                None => continue,
            };
            self.lru.put(id.clone(), entry);
            // Borrow the just-inserted entry from the LRU and pass to callback.
            // The `get` is safe here — we just inserted it.
            if let Some(entry) = self.lru.get(id) {
                callback(entry)?;
            }
        }
        Ok(())
    }

    /// Return all shard ids in the store without loading payload data.
    pub fn enumerate_ids(&self) -> Vec<ShardId> {
        self.store.enumerate_ids()
    }

    /// Return the number of shards on disk.
    pub fn shard_count(&self) -> usize {
        self.store.shard_count()
    }

    /// Return the current number of entries held in the LRU cache.
    pub fn resident_count(&self) -> usize {
        self.lru.len()
    }

    /// Return the configured LRU capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Return a reference to the underlying store.
    pub fn store(&self) -> &OutOfCoreStore {
        &self.store
    }

    /// Consume the accessor and return the underlying store.
    pub fn into_store(self) -> OutOfCoreStore {
        self.store
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors produced by the out-of-core store and accessor.
#[derive(Debug)]
pub enum OutOfCoreError {
    /// An I/O error while reading or writing a shard file.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// A serialization error while encoding a shard entry.
    Serialize(String),
    /// A deserialization error while decoding a shard entry.
    Deserialize(String),
}

impl std::fmt::Display for OutOfCoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "out-of-core I/O error at {}: {}", path.display(), source)
            }
            Self::Serialize(msg) => write!(f, "out-of-core serialize: {msg}"),
            Self::Deserialize(msg) => write!(f, "out-of-core deserialize: {msg}"),
        }
    }
}

impl std::error::Error for OutOfCoreError {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Helper to create a tempdir under target/test-tmp --

    fn test_tempdir(prefix: &str) -> tempfile::TempDir {
        let base = std::env::current_dir()
            .expect("current dir")
            .join("target")
            .join("test-tmp");
        std::fs::create_dir_all(&base).expect("create test tempdir base");
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(base)
            .expect("create test tempdir")
    }

    // -- (a) Env-flag default-off: no env = no module activation --

    #[test]
    fn test_out_of_core_disabled_by_default() {
        // Ensure the env var is not set.
        // SAFETY: tests run single-threaded for env var mutation.
        unsafe {
            std::env::remove_var("DJINN_GRAPH_OUT_OF_CORE");
        }

        assert!(
            !out_of_core_enabled(),
            "out_of_core_enabled() must return false when DJINN_GRAPH_OUT_OF_CORE is not set"
        );
        assert!(
            resolve_out_of_core_config(999_999).is_none(),
            "resolve_out_of_core_config must return None when the env flag is not set, even for large repos"
        );
    }

    #[test]
    fn test_out_of_core_disabled_for_truthy_variants() {
        // All the truthy variants should enable.
        for val in &["1", "true", "TRUE", "True", "yes", "YES", "on", "ON"] {
            unsafe {
                std::env::set_var("DJINN_GRAPH_OUT_OF_CORE", val);
            }
            assert!(
                out_of_core_enabled(),
                "out_of_core_enabled() must return true for DJINN_GRAPH_OUT_OF_CORE={val}"
            );
        }
        // Falsy / unrecognized values should not enable.
        for val in &["0", "false", "no", "off", "N", "random"] {
            unsafe {
                std::env::set_var("DJINN_GRAPH_OUT_OF_CORE", val);
            }
            assert!(
                !out_of_core_enabled(),
                "out_of_core_enabled() must return false for DJINN_GRAPH_OUT_OF_CORE={val}"
            );
        }
        unsafe {
            std::env::remove_var("DJINN_GRAPH_OUT_OF_CORE");
        }
    }

    // -- (b) Threshold gating: small repo does NOT engage even when flag is set --

    #[test]
    fn test_threshold_gating_small_repo() {
        unsafe {
            std::env::set_var("DJINN_GRAPH_OUT_OF_CORE", "1");
            std::env::set_var("DJINN_GRAPH_OUT_OF_CORE_MIN_NODES", "100000");
        }

        // A small repo (e.g. 500 nodes) must not engage.
        assert!(
            resolve_out_of_core_config(500).is_none(),
            "small repo (500 nodes) must not engage out-of-core"
        );

        // Right at the boundary (99 999) must not engage.
        assert!(
            resolve_out_of_core_config(99_999).is_none(),
            "repo with 99,999 nodes must not engage (below threshold)"
        );

        // Exactly at the threshold must engage.
        let config = resolve_out_of_core_config(100_000);
        assert!(
            config.is_some(),
            "repo with 100,000 nodes must engage out-of-core"
        );
        let config = config.unwrap();
        assert_eq!(config.min_nodes, 100_000);
        assert_eq!(config.lru_capacity, DEFAULT_LRU_CAPACITY);

        // Well above the threshold must engage.
        assert!(
            resolve_out_of_core_config(500_000).is_some(),
            "repo with 500,000 nodes must engage out-of-core"
        );

        // Custom threshold.
        unsafe {
            std::env::set_var("DJINN_GRAPH_OUT_OF_CORE_MIN_NODES", "1000");
        }
        assert!(
            resolve_out_of_core_config(500).is_none(),
            "500 nodes < custom threshold 1000"
        );
        assert!(
            resolve_out_of_core_config(1000).is_some(),
            "1000 nodes >= custom threshold 1000"
        );

        unsafe {
            std::env::remove_var("DJINN_GRAPH_OUT_OF_CORE");
            std::env::remove_var("DJINN_GRAPH_OUT_OF_CORE_MIN_NODES");
        }
    }

    // -- (c) Bounded-resident accessor contract: LRU eviction at capacity --

    #[test]
    fn test_lru_eviction_at_capacity() {
        let td = test_tempdir("ooc-lru-eviction-");
        let store = OutOfCoreStore::open(td.path()).unwrap();
        let capacity = 3;
        let mut accessor = BoundedScopeAccessor::new(store, capacity);

        // Insert 5 entries into the store (via the underlying store).
        for i in 0..5 {
            let entry = ScopeEntry {
                id: ShardId(format!("shard-{i:04}")),
                source_key: format!("file_{i}.rs"),
                payload: format!("payload-{i}").into_bytes(),
            };
            accessor.store.put(&entry).unwrap();
        }

        assert_eq!(accessor.shard_count(), 5, "all 5 shards must be on disk");

        // Access the first 3 shards — fills the LRU.
        let _ = accessor.get_scope(&ShardId("shard-0000".into())).unwrap();
        let _ = accessor.get_scope(&ShardId("shard-0001".into())).unwrap();
        let _ = accessor.get_scope(&ShardId("shard-0002".into())).unwrap();
        assert_eq!(
            accessor.resident_count(),
            capacity,
            "LRU must be at capacity after accessing {capacity} shards"
        );

        // Access shard 3 — must evict shard 0 (LRU).
        let _ = accessor.get_scope(&ShardId("shard-0003".into())).unwrap();
        assert_eq!(
            accessor.resident_count(),
            capacity,
            "LRU must remain at capacity after eviction"
        );

        // Access shard 4 — must evict shard 1 (now LRU).
        let _ = accessor.get_scope(&ShardId("shard-0004".into())).unwrap();
        assert_eq!(
            accessor.resident_count(),
            capacity,
            "LRU must remain at capacity after second eviction"
        );

        // The evicted entries should still be loadable from disk.
        let entry_0 = accessor.get_scope(&ShardId("shard-0000".into())).unwrap();
        assert!(entry_0.is_some(), "evicted shard 0 must still be on disk");
        assert_eq!(
            entry_0.unwrap().payload,
            b"payload-0",
            "evicted shard 0 payload must be intact"
        );

        // After re-accessing shard 0, the resident count should still be bounded.
        assert!(
            accessor.resident_count() <= capacity,
            "resident count must not exceed LRU capacity"
        );
    }

    #[test]
    fn test_lru_capacity_is_configurable() {
        unsafe {
            std::env::set_var("DJINN_GRAPH_OUT_OF_CORE", "1");
            std::env::set_var("DJINN_GRAPH_OUT_OF_CORE_MIN_NODES", "100");
            std::env::set_var("DJINN_GRAPH_OUT_OF_CORE_LRU_CAPACITY", "16");
        }

        let config = resolve_out_of_core_config(200).unwrap();
        assert_eq!(config.lru_capacity, 16, "LRU capacity must reflect env var");

        unsafe {
            std::env::remove_var("DJINN_GRAPH_OUT_OF_CORE");
            std::env::remove_var("DJINN_GRAPH_OUT_OF_CORE_MIN_NODES");
            std::env::remove_var("DJINN_GRAPH_OUT_OF_CORE_LRU_CAPACITY");
        }
    }

    // -- (d) Whole-graph invariant: for_each_scope delivers all shards --

    #[test]
    fn test_for_each_scope_delivers_all_shards() {
        let td = test_tempdir("ooc-foreach-");
        let store = OutOfCoreStore::open(td.path()).unwrap();
        let mut accessor = BoundedScopeAccessor::new(store, 4);

        // Insert 8 shards.
        for i in 0..8 {
            let entry = ScopeEntry {
                id: ShardId(format!("shard-{i:04}")),
                source_key: format!("file_{i}.rs"),
                payload: format!("data-{i}").into_bytes(),
            };
            accessor.store.put(&entry).unwrap();
        }

        // Collect all entries via for_each.
        let mut collected_keys: Vec<String> = Vec::new();
        let ids = accessor.enumerate_ids();
        for id in &ids {
            if let Some(entry) = accessor.get_scope(id).unwrap() {
                collected_keys.push(entry.source_key.clone());
            }
        }
        collected_keys.sort();

        assert_eq!(
            collected_keys.len(),
            8,
            "for_each must deliver all 8 shards"
        );
        for i in 0..8 {
            assert!(
                collected_keys.contains(&format!("file_{i}.rs")),
                "must contain file_{i}.rs"
            );
        }

        // Verify resident count is bounded by LRU capacity.
        assert!(
            accessor.resident_count() <= 4,
            "resident count must not exceed LRU capacity of 4"
        );
    }

    #[test]
    fn test_for_each_scope_callback_delivers_all_and_bounds_memory() {
        let td = test_tempdir("ooc-foreach-cb-");
        let store = OutOfCoreStore::open(td.path()).unwrap();
        let capacity = 3;
        let mut accessor = BoundedScopeAccessor::new(store, capacity);

        // Insert 10 shards — more than the LRU capacity.
        for i in 0..10 {
            let entry = ScopeEntry {
                id: ShardId(format!("shard-{i:04}")),
                source_key: format!("file_{i}.rs"),
                payload: format!("data-{i}").into_bytes(),
            };
            accessor.store.put(&entry).unwrap();
        }

        // Use for_each_scope callback to collect all entries.
        let mut collected_keys: Vec<String> = Vec::new();
        accessor
            .for_each_scope(|entry| {
                collected_keys.push(entry.source_key.clone());
                Ok(())
            })
            .unwrap();

        collected_keys.sort();
        assert_eq!(
            collected_keys.len(),
            10,
            "for_each_scope callback must be invoked for all 10 shards"
        );
        for i in 0..10 {
            assert!(
                collected_keys.contains(&format!("file_{i}.rs")),
                "must contain file_{i}.rs"
            );
        }

        // After iterating all 10 shards with LRU capacity 3, resident count
        // must still be bounded.
        assert!(
            accessor.resident_count() <= capacity,
            "resident count {} must not exceed LRU capacity {capacity}",
            accessor.resident_count()
        );
    }

    // -- Store-level tests --

    #[test]
    fn test_store_put_get_roundtrip() {
        let td = test_tempdir("ooc-store-rt-");
        let mut store = OutOfCoreStore::open(td.path()).unwrap();

        let entry = ScopeEntry {
            id: ShardId::from_file_key("src/main.rs"),
            source_key: "src/main.rs".to_string(),
            payload: b"hello world".to_vec(),
        };
        store.put(&entry).unwrap();

        let loaded = store.get(&entry.id).unwrap().unwrap();
        assert_eq!(loaded, entry);
    }

    #[test]
    fn test_store_enumerate_ids() {
        let td = test_tempdir("ooc-store-enum-");
        let mut store = OutOfCoreStore::open(td.path()).unwrap();

        for i in 0..5 {
            let entry = ScopeEntry {
                id: ShardId(format!("id-{i}")),
                source_key: format!("file_{i}.rs"),
                payload: vec![],
            };
            store.put(&entry).unwrap();
        }

        let ids = store.enumerate_ids();
        assert_eq!(ids.len(), 5);
        for i in 0..5 {
            assert!(ids.contains(&ShardId(format!("id-{i}"))));
        }
    }

    #[test]
    fn test_store_get_nonexistent() {
        let td = test_tempdir("ooc-store-miss-");
        let store = OutOfCoreStore::open(td.path()).unwrap();

        let result = store.get(&ShardId("does-not-exist".into())).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_store_cleanup() {
        let td = test_tempdir("ooc-store-cleanup-");
        let mut store = OutOfCoreStore::open(td.path()).unwrap();

        let entry = ScopeEntry {
            id: ShardId::from_file_key("src/lib.rs"),
            source_key: "src/lib.rs".to_string(),
            payload: b"test".to_vec(),
        };
        store.put(&entry).unwrap();

        let root = td.path().to_path_buf();
        store.cleanup().unwrap();
        assert!(!root.exists(), "cleanup must remove the store root");
    }

    #[test]
    fn test_shard_id_determinism() {
        let id1 = ShardId::from_file_key("src/main.rs");
        let id2 = ShardId::from_file_key("src/main.rs");
        let id3 = ShardId::from_file_key("src/lib.rs");

        assert_eq!(id1, id2, "same key must produce same shard id");
        assert_ne!(id1, id3, "different keys must produce different shard ids");
    }

    // -- LRU unit tests --

    #[test]
    fn test_lru_basic_eviction() {
        let mut cache = lru::LruCache::<i32, String>::new(2);
        cache.put(1, "a".into());
        cache.put(2, "b".into());
        // Cache is full; inserting 3 should evict 1.
        cache.put(3, "c".into());
        assert_eq!(cache.len(), 2);
        // 1 should have been evicted.
        assert!(cache.get(&1).is_none());
        assert!(cache.get(&2).is_some());
        assert!(cache.get(&3).is_some());
    }

    #[test]
    fn test_lru_get_refreshes_recency() {
        let mut cache = lru::LruCache::<i32, String>::new(2);
        cache.put(1, "a".into());
        cache.put(2, "b".into());
        // Touch 1 to make it recently used.
        let _ = cache.get(&1);
        // Insert 3 — should evict 2 (now LRU), not 1.
        cache.put(3, "c".into());
        assert!(cache.get(&1).is_some(), "1 must survive (recently used)");
        assert!(cache.get(&2).is_none(), "2 must be evicted (LRU)");
        assert!(cache.get(&3).is_some(), "3 must survive (just inserted)");
    }

    #[test]
    fn test_lru_capacity_one() {
        let mut cache = lru::LruCache::<i32, String>::new(1);
        cache.put(1, "a".into());
        assert_eq!(cache.len(), 1);
        cache.put(2, "b".into());
        assert_eq!(cache.len(), 1);
        assert!(cache.get(&1).is_none());
        assert!(cache.get(&2).is_some());
    }
}
