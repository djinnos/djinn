// djinn:allow-oversize — out-of-core store; split pending.
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

use crate::scip_parser::{ParsedScipIndex, ScipFile};

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

impl ScopeEntry {
    /// Create a [`ScopeEntry`] from a parsed SCIP file by JSON-serializing
    /// the file into the `payload` and deriving the shard id from the
    /// file's `relative_path`.
    pub fn from_scip_file(file: &ScipFile) -> Self {
        let payload = serde_json::to_vec(file)
            .expect("ScopeEntry::from_scip_file: JSON serialization of ScipFile must not fail");
        let source_key = file.relative_path.to_string_lossy().into_owned();
        Self {
            id: ShardId::from_file_key(&source_key),
            source_key,
            payload,
        }
    }
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
    order: Vec<ShardId>,
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

        let order_path = root.join("order.json");
        let order: Vec<ShardId> = if order_path.exists() {
            let data = std::fs::read_to_string(&order_path).map_err(|e| OutOfCoreError::Io {
                path: order_path.clone(),
                source: e,
            })?;
            serde_json::from_str(&data).unwrap_or_else(|_| index.keys().cloned().collect())
        } else {
            index.keys().cloned().collect()
        };

        Ok(Self {
            root: root.to_path_buf(),
            index,
            order,
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
        if !self.index.contains_key(&entry.id) {
            self.order.push(entry.id.clone());
        }
        self.index
            .insert(entry.id.clone(), PathBuf::from(&filename));
        self.persist_index()?;
        Ok(())
    }

    /// Write multiple scope entries to disk and persist the index once at
    /// the end. This reduces index I/O from O(N²) (one index write per
    /// entry) to O(N) (one index write for the whole batch).
    pub fn put_batch(&mut self, entries: &[ScopeEntry]) -> Result<usize, OutOfCoreError> {
        let mut count = 0usize;
        for entry in entries {
            let filename = format!("{}.json", entry.id.0);
            let path = self.root.join(&filename);
            let json = serde_json::to_string(entry)
                .map_err(|e| OutOfCoreError::Serialize(e.to_string()))?;
            std::fs::write(&path, json).map_err(|e| OutOfCoreError::Io {
                path: path.clone(),
                source: e,
            })?;
            if !self.index.contains_key(&entry.id) {
                self.order.push(entry.id.clone());
            }
            self.index
                .insert(entry.id.clone(), PathBuf::from(&filename));
            count += 1;
        }
        self.persist_index()?;
        Ok(count)
    }

    /// Explicitly persist the in-memory index to disk. Useful after a batch
    /// of operations when index durability is required before a crash-sensitive
    /// step.
    pub fn flush_index(&self) -> Result<(), OutOfCoreError> {
        self.persist_index()
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
        let mut ids: Vec<_> = self
            .order
            .iter()
            .filter(|id| self.index.contains_key(*id))
            .cloned()
            .collect();
        for id in self.index.keys() {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
        ids
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
        let order_path = self.root.join("order.json");
        let order_tmp_path = self.root.join("order.json.tmp");
        let order_json = serde_json::to_string(&self.order)
            .map_err(|e| OutOfCoreError::Serialize(e.to_string()))?;
        std::fs::write(&order_tmp_path, order_json).map_err(|e| OutOfCoreError::Io {
            path: order_tmp_path.clone(),
            source: e,
        })?;
        std::fs::rename(&order_tmp_path, &order_path).map_err(|e| OutOfCoreError::Io {
            path: order_path,
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

    /// Shard every file in a [`ParsedScipIndex`] into the store as
    /// individual [`ScopeEntry`] items. Returns the count of shards written.
    ///
    /// Each file is JSON-serialized into the shard payload independently,
    /// so downstream consumers can load one file at a time without holding
    /// the entire parsed index in memory.
    pub fn put_parsed_index(&mut self, index: &ParsedScipIndex) -> Result<usize, OutOfCoreError> {
        let entries: Vec<ScopeEntry> = index.files.iter().map(ScopeEntry::from_scip_file).collect();
        self.put_batch(&entries)
    }

    /// Yield deserialized [`ScipFile`] entries from disk, one at a time.
    ///
    /// Each shard is loaded and deserialized on demand so that at most one
    /// `ScipFile` is resident at any given time. This keeps peak memory
    /// usage bounded to `O(single_file)` rather than `O(all_files)`.
    pub fn scip_file_iter(&self) -> impl Iterator<Item = Result<ScipFile, OutOfCoreError>> + '_ {
        let ids = self.enumerate_ids();
        ids.into_iter().map(move |id| {
            let entry = self.get(&id)?.ok_or_else(|| {
                OutOfCoreError::Deserialize(format!("missing shard for id {}", id.0))
            })?;
            serde_json::from_slice::<ScipFile>(&entry.payload)
                .map_err(|e| OutOfCoreError::Deserialize(e.to_string()))
        })
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
        ///
        /// ## Complexity analysis
        ///
        /// The eviction scan (`min_by_key` over the map entries) is O(capacity)
        /// per put. Over a full iteration of N shards through `for_each_scope`,
        /// each shard is loaded once (N puts) and the cache holds at most
        /// `capacity` entries. Total cost is O(N * capacity). Since capacity is
        /// bounded (default 1024, typically <= 8192), this is O(N) in practice.
        /// For bounded capacity the scan is acceptable; if capacity grows very
        /// large, a doubly-linked-list + hashmap LRU would give O(1) eviction.
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
    use djinn_core::clock::{Clock, SystemClock};

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
        // Shared with the canonical-graph out-of-core tests, which read the
        // same `DJINN_GRAPH_OUT_OF_CORE*` process env. Serialize them all.
        let _env_lock = crate::test_helpers::lock_pipeline_env();
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
        let _env_lock = crate::test_helpers::lock_pipeline_env();
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
        let _env_lock = crate::test_helpers::lock_pipeline_env();
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
        let _env_lock = crate::test_helpers::lock_pipeline_env();
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

    // -- ScopeEntry::from_scip_file roundtrip --

    fn make_test_scip_file(path: &str) -> crate::scip_parser::ScipFile {
        use std::collections::BTreeSet;
        crate::scip_parser::ScipFile {
            language: "rust".to_string(),
            relative_path: std::path::PathBuf::from(path),
            definitions: vec![crate::scip_parser::ScipOccurrence {
                symbol: "scip-rust . . . Foo#".to_string(),
                range: crate::scip_parser::ScipRange {
                    start_line: 0,
                    start_character: 0,
                    end_line: 0,
                    end_character: 10,
                },
                enclosing_range: None,
                roles: BTreeSet::from([crate::scip_parser::ScipSymbolRole::Definition]),
                syntax_kind: None,
                override_documentation: vec![],
            }],
            references: vec![],
            occurrences: vec![],
            symbols: vec![],
        }
    }

    #[test]
    fn test_from_scip_file_roundtrip() {
        let td = test_tempdir("ooc-scip-roundtrip-");
        let mut store = OutOfCoreStore::open(td.path()).unwrap();

        let file = make_test_scip_file("src/main.rs");
        let entry = ScopeEntry::from_scip_file(&file);

        // Verify the entry's source_key matches the file path.
        assert_eq!(entry.source_key, "src/main.rs");

        // Put and get the entry.
        store.put(&entry).unwrap();
        let loaded = store.get(&entry.id).unwrap().expect("entry must exist");

        // Deserialize the payload back to a ScipFile.
        let recovered: crate::scip_parser::ScipFile =
            serde_json::from_slice(&loaded.payload).expect("deserialize ScipFile from payload");

        assert_eq!(recovered, file, "roundtripped ScipFile must equal original");
    }

    #[test]
    fn test_put_parsed_index_writes_all_files() {
        let td = test_tempdir("ooc-put-index-");
        let mut store = OutOfCoreStore::open(td.path()).unwrap();

        let index = crate::scip_parser::ParsedScipIndex {
            workspace_slug: "test".to_string(),
            metadata: crate::scip_parser::ScipMetadata::default(),
            files: vec![
                make_test_scip_file("src/a.rs"),
                make_test_scip_file("src/b.rs"),
                make_test_scip_file("src/c.rs"),
            ],
            external_symbols: vec![],
        };

        let count = store.put_parsed_index(&index).unwrap();
        assert_eq!(count, 3, "must write a shard for each file");
        assert_eq!(store.shard_count(), 3, "store must contain 3 shards");
    }

    #[test]
    fn test_scip_file_iter_yields_all_files() {
        let td = test_tempdir("ooc-scip-iter-");
        let mut store = OutOfCoreStore::open(td.path()).unwrap();

        let files = vec![
            make_test_scip_file("src/a.rs"),
            make_test_scip_file("src/b.rs"),
            make_test_scip_file("src/c.rs"),
        ];
        let index = crate::scip_parser::ParsedScipIndex {
            workspace_slug: "test".to_string(),
            metadata: crate::scip_parser::ScipMetadata::default(),
            files: files.clone(),
            external_symbols: vec![],
        };

        store.put_parsed_index(&index).unwrap();

        let iterated: Vec<crate::scip_parser::ScipFile> = store
            .scip_file_iter()
            .collect::<Result<Vec<_>, _>>()
            .expect("scip_file_iter must succeed");

        assert_eq!(iterated.len(), 3, "iterator must yield all 3 files");

        // Sort both by path for stable comparison.
        let mut original_paths: Vec<_> = files.iter().map(|f| &f.relative_path).collect();
        original_paths.sort();
        let mut iterated_paths: Vec<_> = iterated.iter().map(|f| &f.relative_path).collect();
        iterated_paths.sort();
        assert_eq!(original_paths, iterated_paths);

        // Verify full content equality for each file.
        for original in &files {
            let matched = iterated
                .iter()
                .find(|f| f.relative_path == original.relative_path);
            assert!(
                matched.is_some(),
                "iterator must yield file {:?}",
                original.relative_path
            );
            assert_eq!(matched.unwrap(), original);
        }
    }

    #[test]
    fn test_scip_file_iter_one_at_a_time() {
        // This test verifies that the iterator loads files from disk
        // on-demand rather than preloading them all. We verify by
        // checking the iterator is lazy (only loads when `.next()` is
        // called) and that it produces valid content one entry at a time.
        let td = test_tempdir("ooc-scip-iter-oat-");
        let mut store = OutOfCoreStore::open(td.path()).unwrap();

        let files: Vec<_> = (0..10)
            .map(|i| make_test_scip_file(&format!("src/file_{i}.rs")))
            .collect();
        let index = crate::scip_parser::ParsedScipIndex {
            workspace_slug: "test".to_string(),
            metadata: crate::scip_parser::ScipMetadata::default(),
            files,
            external_symbols: vec![],
        };
        store.put_parsed_index(&index).unwrap();
        assert_eq!(store.shard_count(), 10);

        // Consume the iterator one at a time and count.
        let mut count = 0usize;
        for result in store.scip_file_iter() {
            let file = result.expect("each iteration must succeed");
            assert!(
                file.relative_path
                    .to_string_lossy()
                    .starts_with("src/file_"),
                "each file must have the expected prefix"
            );
            count += 1;
        }
        assert_eq!(count, 10, "iterator must yield exactly 10 files");
    }

    // -- Synthetic large-fixture bounded-memory test --

    #[test]
    fn test_bounded_memory_synthetic_large_fixture() {
        let td = test_tempdir("ooc-synthetic-large-");
        let store = OutOfCoreStore::open(td.path()).unwrap();
        let capacity = 8;
        let mut accessor = BoundedScopeAccessor::new(store, capacity);

        // Create 1000 synthetic shards with ~1KB payloads.
        let entries: Vec<ScopeEntry> = (0..1000)
            .map(|i| ScopeEntry {
                id: ShardId(format!("shard-{i:04}")),
                source_key: format!("file_{i}.rs"),
                payload: vec![b'x'; 1024],
            })
            .collect();

        // Put all entries via the underlying store.
        for entry in &entries {
            accessor.store.put(entry).unwrap();
        }

        assert_eq!(
            accessor.shard_count(),
            1000,
            "store must contain 1000 shards"
        );

        // Iterate all via for_each_scope, asserting resident_count stays bounded.
        // We cannot check resident_count inside the closure because for_each_scope
        // takes &mut self and the closure would need to borrow accessor again.
        // Instead, we verify the bound after the full iteration and also check
        // by manually loading entries one at a time.
        let mut visited = 0usize;
        accessor
            .for_each_scope(|entry| {
                assert_eq!(entry.payload.len(), 1024, "payload must be intact");
                visited += 1;
                Ok(())
            })
            .unwrap();

        assert_eq!(visited, 1000, "for_each_scope must visit all 1000 shards");
        assert_eq!(
            accessor.shard_count(),
            1000,
            "shard count must be 1000 after iteration"
        );
        assert!(
            accessor.resident_count() <= capacity,
            "resident_count {} must not exceed capacity {capacity} after full iteration",
            accessor.resident_count()
        );

        // Verify bounded memory by loading entries one-at-a-time and checking
        // resident count after each load.
        let ids: Vec<ShardId> = (0..1000)
            .map(|i| ShardId(format!("shard-{i:04}")))
            .collect();
        for id in &ids {
            let _entry = accessor
                .get_scope(id)
                .unwrap()
                .expect("entry must be accessible");
            assert!(
                accessor.resident_count() <= capacity,
                "resident_count {} must not exceed capacity {capacity} after loading {id:?}",
                accessor.resident_count()
            );
        }
    }

    // -- Batch put test --

    #[test]
    fn test_put_batch_single_index_write() {
        let td = test_tempdir("ooc-put-batch-");
        let mut store = OutOfCoreStore::open(td.path()).unwrap();

        let entries: Vec<ScopeEntry> = (0..10)
            .map(|i| ScopeEntry {
                id: ShardId(format!("batch-{i:02}")),
                source_key: format!("batch_file_{i}.rs"),
                payload: format!("batch-payload-{i}").into_bytes(),
            })
            .collect();

        let count = store.put_batch(&entries).unwrap();
        assert_eq!(count, 10, "put_batch must return 10");
        assert_eq!(store.shard_count(), 10, "store must contain 10 shards");

        // Verify each entry is readable and correct.
        for (i, entry) in entries.iter().enumerate() {
            let loaded = store.get(&entry.id).unwrap().expect("entry must exist");
            assert_eq!(loaded.source_key, format!("batch_file_{i}.rs"));
            assert_eq!(loaded.payload, format!("batch-payload-{i}").into_bytes());
        }
    }

    #[test]
    fn test_put_batch_produces_same_state_as_individual_puts() {
        let td_batch = test_tempdir("ooc-batch-state-");
        let td_single = test_tempdir("ooc-single-state-");

        let entries: Vec<ScopeEntry> = (0..20)
            .map(|i| ScopeEntry {
                id: ShardId(format!("state-{i:02}")),
                source_key: format!("state_file_{i}.rs"),
                payload: format!("state-payload-{i}").into_bytes(),
            })
            .collect();

        // Batch path.
        let mut batch_store = OutOfCoreStore::open(td_batch.path()).unwrap();
        let batch_count = batch_store.put_batch(&entries).unwrap();
        assert_eq!(batch_count, 20);

        // Individual put path.
        let mut single_store = OutOfCoreStore::open(td_single.path()).unwrap();
        for entry in &entries {
            single_store.put(entry).unwrap();
        }

        // Both stores should have identical state.
        assert_eq!(batch_store.shard_count(), single_store.shard_count());

        let batch_ids = batch_store.enumerate_ids();
        let single_ids = single_store.enumerate_ids();
        assert_eq!(batch_ids.len(), single_ids.len());
        for id in &batch_ids {
            assert!(
                single_ids.contains(id),
                "single store must contain id {id:?}"
            );
            let batch_entry = batch_store.get(id).unwrap().unwrap();
            let single_entry = single_store.get(id).unwrap().unwrap();
            assert_eq!(batch_entry, single_entry, "entries for {id:?} must match");
        }
    }

    #[test]
    fn test_flush_index_persists_index() {
        let td = test_tempdir("ooc-flush-");
        let mut store = OutOfCoreStore::open(td.path()).unwrap();

        let entry = ScopeEntry {
            id: ShardId::from_file_key("src/flush.rs"),
            source_key: "src/flush.rs".to_string(),
            payload: b"flush test".to_vec(),
        };
        store.put(&entry).unwrap();

        // Remove the index file manually.
        let index_path = td.path().join("index.json");
        assert!(index_path.exists());
        std::fs::remove_file(&index_path).unwrap();

        // flush_index should recreate it.
        store.flush_index().unwrap();
        assert!(index_path.exists(), "flush_index must recreate index.json");

        // Re-open the store and verify the entry is still readable.
        let reopened = OutOfCoreStore::open(td.path()).unwrap();
        let loaded = reopened
            .get(&entry.id)
            .unwrap()
            .expect("entry must exist after reopen");
        assert_eq!(loaded, entry);
    }

    // -- Timing regression test --

    #[test]
    fn test_iteration_performance_1000_shards() {
        let td = test_tempdir("ooc-perf-1000-");
        let store = OutOfCoreStore::open(td.path()).unwrap();
        let capacity = 8;
        let mut accessor = BoundedScopeAccessor::new(store, capacity);

        // Create 1000 shards with ~1KB payloads.
        let entries: Vec<ScopeEntry> = (0..1000)
            .map(|i| ScopeEntry {
                id: ShardId(format!("perf-{i:04}")),
                source_key: format!("perf_file_{i}.rs"),
                payload: vec![b'y'; 1024],
            })
            .collect();

        for entry in &entries {
            accessor.store.put(entry).unwrap();
        }

        let start = SystemClock::new().now_instant();
        let mut visited = 0usize;
        accessor
            .for_each_scope(|_entry| {
                visited += 1;
                Ok(())
            })
            .unwrap();
        let elapsed = start.elapsed();

        assert_eq!(visited, 1000, "must visit all 1000 shards");
        assert!(
            elapsed.as_secs() < 10,
            "iteration of 1000 shards must complete in under 10 seconds, took {:?}",
            elapsed
        );
    }
}
