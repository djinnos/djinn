mod actor;
mod handle;
mod types;

pub use handle::SlotPoolHandle;
pub use types::{
    ModelPoolStatus, PoolError, PoolMessage, PoolStatus, RunningTaskInfo, SlotFactory,
};

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // test: deadlined poll loops in integration tests
mod tests;
