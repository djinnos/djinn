#[cfg(any(test, feature = "test-support"))]
mod actor;
mod handle;
mod types;

pub use handle::SlotPoolHandle;
#[cfg(any(test, feature = "test-support"))]
pub use types::SlotFactory;
pub use types::{ModelPoolStatus, PoolError, PoolMessage, PoolStatus, RunningTaskInfo};

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // test: deadlined poll loops in integration tests
mod tests;
