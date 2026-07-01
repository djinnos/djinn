mod handle;
mod types;

pub use handle::SlotPoolHandle;
#[cfg(any(test, feature = "test-support"))]
pub use types::SlotFactory;
pub use types::{ModelPoolStatus, PoolError, PoolMessage, PoolStatus, RunningTaskInfo};

// Agent-side pool tests were compatibility coverage for the removed duplicate
// actor. Canonical pool behavior is covered in `djinn-slot::pool::tests`; this
// facade now contains only AgentContext→SlotContext construction glue.
