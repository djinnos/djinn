//! Durable, append-only pod-log segment storage.
//!
//! This crate deliberately owns only the policy-independent storage primitive.
//! Ingest, quotas, and retention are layered on it by later components.

mod identity;
mod store;

pub use identity::{ContainerName, Namespace, PodUid, StreamIdentity};
pub use store::{
    Clock, Compressor, DEFAULT_MAX_LOGICAL_BYTES, GzipCompressor, LogStore, StoreConfig,
    StoreError, SystemClock,
};
