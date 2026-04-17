//! In-process [`SessionRuntime`] stub used by integration tests.
//!
//! Phase 2 PR 1 — this is deliberately a stub.  `prepare` / `attach_stdio` /
//! `cancel` / `teardown` all succeed synchronously, and `teardown` returns a
//! canned [`TaskRunReport`] that the coordinator test harness can pattern
//! match on.  PR 4 replaces this stub with a real in-process supervisor
//! driver.
//!
//! Gated behind `cfg(any(test, feature = "test-runtime"))` so production
//! binaries never link it.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;

use async_trait::async_trait;

use crate::handle::RunHandle;
use crate::session_runtime::{RuntimeError, SessionRuntime};
use crate::spec::{TaskRunOutcome, TaskRunReport, TaskRunSpec};
use crate::stream::BiStream;

/// In-memory test runtime.  Captures the last [`TaskRunSpec`] it was asked
/// to prepare so assertions can inspect the coordinator's model-routing
/// plumbing without standing up a real container.
#[derive(Default)]
pub struct TestRuntime {
    last_spec: Mutex<Option<TaskRunSpec>>,
}

impl TestRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a clone of the most recent [`TaskRunSpec`] passed into
    /// `prepare`, or `None` if `prepare` has not been called yet.
    pub fn last_spec(&self) -> Option<TaskRunSpec> {
        self.last_spec.lock().expect("mutex poisoned").clone()
    }
}

#[async_trait]
impl SessionRuntime for TestRuntime {
    async fn prepare(&self, spec: &TaskRunSpec) -> Result<RunHandle, RuntimeError> {
        *self.last_spec.lock().expect("mutex poisoned") = Some(spec.clone());
        Ok(RunHandle {
            task_run_id: format!("test-{}", spec.task_id),
            container_id: None,
            pod_ref: None,
            ipc_socket: PathBuf::from("/tmp/djinn-test-runtime.sock"),
            started_at: SystemTime::now(),
        })
    }

    async fn attach_stdio(&self, _handle: &RunHandle) -> Result<BiStream, RuntimeError> {
        // Drop the other halves immediately — PR 1 callers (if any) only
        // need the shape to exist.  PR 4 wires a real pump.
        let (stream, _events_tx, _requests_rx) = BiStream::new_in_memory(16);
        Ok(stream)
    }

    async fn cancel(&self, _handle: &RunHandle) -> Result<(), RuntimeError> {
        Ok(())
    }

    async fn teardown(&self, handle: RunHandle) -> Result<TaskRunReport, RuntimeError> {
        Ok(TaskRunReport {
            task_run_id: handle.task_run_id,
            outcome: TaskRunOutcome::Closed {
                reason: "TestRuntime stub: no supervisor wired yet".to_string(),
            },
            stages_completed: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use djinn_core::models::TaskRunTrigger;

    use super::*;
    use crate::spec::SupervisorFlow;

    fn dummy_spec() -> TaskRunSpec {
        TaskRunSpec {
            task_id: "t1".into(),
            project_id: "p1".into(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".into(),
            task_branch: "djinn/t1".into(),
            flow: SupervisorFlow::Planning,
            model_id_per_role: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn prepare_and_teardown_roundtrip() {
        let rt = TestRuntime::new();
        let spec = dummy_spec();
        let handle = rt.prepare(&spec).await.expect("prepare");
        assert_eq!(handle.task_run_id, "test-t1");

        let _stream = rt.attach_stdio(&handle).await.expect("attach");
        rt.cancel(&handle).await.expect("cancel");

        let report = rt.teardown(handle).await.expect("teardown");
        assert_eq!(report.task_run_id, "test-t1");
        matches!(report.outcome, TaskRunOutcome::Closed { .. });
        assert_eq!(rt.last_spec().unwrap().task_id, "t1");
    }
}
