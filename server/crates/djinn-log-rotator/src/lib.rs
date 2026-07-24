//! Durable, append-only pod-log segment storage.
//!
//! This crate deliberately owns only the policy-independent storage primitive.
//! Ingest, quotas, and retention are layered on it by later components.

mod http;
mod identity;
mod store;

pub use http::{AppState, MAX_RECORD_BYTES, StoreBackend, metrics_router, router};
pub use identity::{ContainerName, Namespace, PodUid, StreamIdentity};
pub use store::{
    Clock, Compressor, DEFAULT_MAX_AGE, DEFAULT_MAX_GLOBAL_LOGICAL_BYTES,
    DEFAULT_MAX_LOGICAL_BYTES, DEFAULT_MAX_STREAM_LOGICAL_BYTES, DEFAULT_MINIMUM_RESERVE_BYTES,
    EvictionReason, EvictionTransition, FilesystemCapacity, GzipCompressor, LogStore,
    ReserveTransition, StoreConfig, StoreError, SystemClock, SystemFilesystemCapacity,
    WritableState,
};
