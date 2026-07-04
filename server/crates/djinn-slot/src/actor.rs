use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::host::SlotContext;
use crate::reply_loop::compaction_guard::CompactionCriticalSection;

use super::supervisor_runner::run_supervisor_dispatch;
use super::{SlotCommand, SlotError, SlotEvent};

type LifecycleFuture = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'static>>;
/// Signature for the slot's lifecycle runner. The final argument is the
/// optional resume-via-git lifecycle metadata selected at re-dispatch time
/// (mirrors `djinn_runtime::ResumeLifecycleMetadata`); `None` for the
/// default/off path so legacy runners keep compiling without changes.
type LifecycleRunner = Arc<
    dyn Fn(
            String,
            String,
            String,
            SlotContext,
            CancellationToken,
            CancellationToken,
            Option<serde_json::Value>,
        ) -> LifecycleFuture
        + Send
        + Sync,
>;

#[cfg(any(test, feature = "test-support"))]
pub type TestLifecycleRunner = LifecycleRunner;

struct ActiveLifecycle {
    task_id: String,
    join: tokio::task::JoinHandle<anyhow::Result<()>>,
    kill: CancellationToken,
    pause: CancellationToken,
    span: tracing::Span,
    killed: bool,
    compaction_cs: CompactionCriticalSection,
}

pub struct SlotActor {
    id: usize,
    model_id: String,
    receiver: mpsc::Receiver<SlotCommand>,
    event_tx: mpsc::Sender<SlotEvent>,
    app_state: SlotContext,
    cancel: CancellationToken,
    runner: LifecycleRunner,
    compaction_cs: CompactionCriticalSection,
}

impl SlotActor {
    pub async fn run(mut self) {
        let mut active: Option<ActiveLifecycle> = None;
        let mut drain_requested = false;
        loop {
            if let Some(mut running) = active.take() {
                tokio::select! {
                    _ = self.cancel.cancelled() => {
                        running.kill.cancel();
                        let _ = running.join.await;
                        break;
                    }
                    join_result = &mut running.join => {
                        // A failed lifecycle leaves no task-status transition
                        // behind, so surface it loudly with the task id —
                        // otherwise the task silently bounces back to `open`
                        // and the only signal is the coordinator's re-dispatch
                        // streak. Two distinct failure shapes: a JoinError
                        // (panic/abort) AND the runner *returning* `Err(..)`
                        // (an infra/setup failure from `run_supervisor_dispatch`
                        // — e.g. credential resolution). The latter was
                        // previously dropped on the floor, making dispatch-time
                        // failures invisible.
                        match &join_result {
                            Err(e) => tracing::error!(slot_id = self.id, model_id = %self.model_id, task_id = %running.task_id, error = %e, "slot lifecycle task panicked/aborted"),
                            Ok(Err(e)) => tracing::error!(slot_id = self.id, model_id = %self.model_id, task_id = %running.task_id, error = %format!("{e:#}"), "slot lifecycle returned error (dispatch/setup failure)"),
                            Ok(Ok(())) => {}
                        }
                        self.emit_completion_event(&running).await;
                        if drain_requested {
                            break;
                        }
                    }
                    cmd = self.receiver.recv() => {
                        match cmd {
                            Some(SlotCommand::RunTask { respond_to, .. })
                            | Some(SlotCommand::RunTaskWithResume { respond_to, .. }) => {
                                let _ = respond_to.send(Err(SlotError::SlotBusy));
                                active = Some(running);
                            }
                            Some(SlotCommand::Kill) => {
                                let span = tracing::info_span!(
                                    "djinn.slot.kill",
                                    slot_id = self.id,
                                    model_id = %self.model_id,
                                    task_id = %running.task_id,
                                );
                                span.in_scope(|| {
                                    tracing::info!(
                                        event = "slot.kill_requested",
                                        slot_id = self.id,
                                        model_id = %self.model_id,
                                        task_id = %running.task_id,
                                    );
                                });
                                running.killed = true;
                                running.kill.cancel();
                                active = Some(running);
                            }
                            Some(SlotCommand::Pause) => {
                                running.pause.cancel();
                                active = Some(running);
                            }
                            Some(SlotCommand::Drain) => {
                                drain_requested = true;
                                active = Some(running);
                            }
                            None => {
                                running.kill.cancel();
                                let _ = running.join.await;
                                break;
                            }
                        }
                    }
                }
            } else {
                tokio::select! {
                    _ = self.cancel.cancelled() => {
                        break;
                    }
                    cmd = self.receiver.recv() => {
                        match cmd {
                            Some(SlotCommand::RunTask { task_id, project_path, respond_to }) => {
                                let lifecycle = self.start_lifecycle(
                                    task_id,
                                    project_path,
                                    None,
                                );
                                let _ = respond_to.send(Ok(()));
                                active = Some(lifecycle);
                            }
                            Some(SlotCommand::RunTaskWithResume {
                                task_id,
                                project_path,
                                resume_lifecycle_metadata,
                                respond_to,
                            }) => {
                                let lifecycle = self.start_lifecycle(
                                    task_id,
                                    project_path,
                                    resume_lifecycle_metadata,
                                );
                                let _ = respond_to.send(Ok(()));
                                active = Some(lifecycle);
                            }
                            Some(SlotCommand::Kill) | Some(SlotCommand::Pause) => {
                                // No active lifecycle; command is a no-op.
                            }
                            Some(SlotCommand::Drain) | None => {
                                break;
                            }
                        }
                    }
                }
            }
        }
    }
    /// Spawn a lifecycle task for the slot, attaching the optional
    /// resume-via-git lifecycle metadata. Returns the active lifecycle
    /// handle so the outer loop can wait on its `join` and emit
    /// completion events.
    fn start_lifecycle(
        &self,
        task_id: String,
        project_path: String,
        resume_lifecycle_metadata: Option<serde_json::Value>,
    ) -> ActiveLifecycle {
        let kill = CancellationToken::new();
        let pause = CancellationToken::new();
        let span = tracing::info_span!(
            "djinn.slot.run_task",
            slot_id = self.id,
            model_id = %self.model_id,
            task_id = %task_id,
            has_resume_metadata = resume_lifecycle_metadata.is_some(),
        );
        let run = (self.runner)(
            task_id.clone(),
            project_path,
            self.model_id.clone(),
            self.app_state.clone(),
            kill.clone(),
            pause.clone(),
            resume_lifecycle_metadata,
        );
        let join = tokio::spawn(run.instrument(span.clone()));
        ActiveLifecycle {
            task_id,
            join,
            kill,
            pause,
            span,
            killed: false,
            compaction_cs: self.compaction_cs.clone(),
        }
    }
    async fn emit_completion_event(&self, running: &ActiveLifecycle) {
        let event = if running.killed {
            running.span.in_scope(|| {
                tracing::info!(
                    event = "slot.killed",
                    slot_id = self.id,
                    model_id = %self.model_id,
                    task_id = %running.task_id,
                );
            });
            SlotEvent::Killed {
                slot_id: self.id,
                model_id: self.model_id.clone(),
                task_id: running.task_id.clone(),
            }
        } else {
            running.span.in_scope(|| {
                tracing::info!(
                    event = "slot.free",
                    slot_id = self.id,
                    model_id = %self.model_id,
                    task_id = %running.task_id,
                );
            });
            SlotEvent::Free {
                slot_id: self.id,
                model_id: self.model_id.clone(),
                task_id: running.task_id.clone(),
            }
        };
        let _ = self.event_tx.send(event).await;
    }
}

#[derive(Debug, Clone)]
pub struct SlotHandle {
    id: usize,
    model_id: String,
    sender: mpsc::Sender<SlotCommand>,
}

impl SlotHandle {
    pub fn spawn(
        id: usize,
        model_id: String,
        event_tx: mpsc::Sender<SlotEvent>,
        app_state: SlotContext,
        cancel: CancellationToken,
    ) -> Self {
        // Task #7: default slot dispatch now routes through
        // `TaskRunSupervisor::run` (see `supervisor_runner`).  One slot
        // dispatch = one task-run that internally sequences the entire
        // role pipeline (planner → worker → reviewer → verifier for
        // NewTask, or the flow-specific sequence for Spike / Planning /
        // ReviewResponse / ConflictRetry).
        //
        // The legacy `run_task_lifecycle` path is kept behind
        // `#[allow(dead_code)]` for rollback and test coverage; see
        // `lifecycle_tests.rs` which exercises it directly.  Task #8 will
        // delete the worktree/lifecycle code entirely after soak.
        let runner: LifecycleRunner = Arc::new(
            |task_id, project_path, model_id, app_state, kill, pause, resume_lifecycle_metadata| {
                Box::pin(run_supervisor_dispatch(
                    task_id,
                    project_path,
                    model_id,
                    app_state,
                    kill,
                    pause,
                    resume_lifecycle_metadata,
                ))
            },
        );
        Self::spawn_with_runner(id, model_id, event_tx, app_state, cancel, runner)
    }
    fn spawn_with_runner(
        id: usize,
        model_id: String,
        event_tx: mpsc::Sender<SlotEvent>,
        app_state: SlotContext,
        cancel: CancellationToken,
        runner: LifecycleRunner,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(16);
        let compaction_cs = CompactionCriticalSection::new();
        let mut app_state = app_state;
        app_state.compaction_cs = compaction_cs.clone();
        let actor = SlotActor {
            id,
            model_id: model_id.clone(),
            receiver,
            event_tx,
            app_state,
            cancel,
            runner,
            compaction_cs,
        };
        tokio::spawn(actor.run());
        Self {
            id,
            model_id,
            sender,
        }
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn spawn_with_test_runner(
        id: usize,
        model_id: String,
        event_tx: mpsc::Sender<SlotEvent>,
        app_state: SlotContext,
        cancel: CancellationToken,
        runner: TestLifecycleRunner,
    ) -> Self {
        Self::spawn_with_runner(id, model_id, event_tx, app_state, cancel, runner)
    }
    pub fn id(&self) -> usize {
        self.id
    }
    pub fn model_id(&self) -> &str {
        &self.model_id
    }
    #[tracing::instrument(
        name = "djinn.slot.run_task",
        skip(self, project_path),
        fields(slot_id = self.id, model_id = %self.model_id, task_id = %task_id)
    )]
    pub async fn run_task(&self, task_id: String, project_path: String) -> Result<(), SlotError> {
        self.run_task_with_resume(task_id, project_path, None).await
    }
    /// Additive re-dispatch path used by the coordinator's
    /// `DispatchWithResume` slot-pool message. The `resume_lifecycle_metadata`
    /// blob rides the slot pipeline through `SlotCommand::RunTaskWithResume`
    /// so the lifecycle runner can put it on `TaskRunSpec::resume_lifecycle_metadata`.
    /// `None` matches the legacy `run_task` path byte-for-byte (and is what
    /// `run_task` itself delegates to), so disabled/default dispatch
    /// behavior is preserved.
    #[tracing::instrument(
        name = "djinn.slot.run_task_with_resume",
        skip(self, project_path, resume_lifecycle_metadata),
        fields(
            slot_id = self.id,
            model_id = %self.model_id,
            task_id = %task_id,
            has_resume_metadata = resume_lifecycle_metadata.is_some(),
        )
    )]
    pub async fn run_task_with_resume(
        &self,
        task_id: String,
        project_path: String,
        resume_lifecycle_metadata: Option<serde_json::Value>,
    ) -> Result<(), SlotError> {
        let (tx, rx) = oneshot::channel();
        let cmd = match resume_lifecycle_metadata {
            Some(metadata) => SlotCommand::RunTaskWithResume {
                task_id,
                project_path,
                resume_lifecycle_metadata: Some(metadata),
                respond_to: tx,
            },
            None => SlotCommand::RunTask {
                task_id,
                project_path,
                respond_to: tx,
            },
        };
        self.sender
            .send(cmd)
            .await
            .map_err(|_| SlotError::SessionFailed("slot actor channel closed".to_string()))?;
        rx.await
            .map_err(|_| SlotError::SessionFailed("slot actor did not ack dispatch".to_string()))?
    }
    #[tracing::instrument(
        name = "djinn.slot.kill",
        skip(self),
        fields(slot_id = self.id, model_id = %self.model_id)
    )]
    pub async fn kill(&self) -> Result<(), SlotError> {
        self.sender
            .send(SlotCommand::Kill)
            .await
            .map_err(|_| SlotError::SessionFailed("slot actor channel closed".to_string()))
    }
    pub async fn pause(&self) -> Result<(), SlotError> {
        self.sender
            .send(SlotCommand::Pause)
            .await
            .map_err(|_| SlotError::SessionFailed("slot actor channel closed".to_string()))
    }
    pub async fn drain(&self) -> Result<(), SlotError> {
        self.sender
            .send(SlotCommand::Drain)
            .await
            .map_err(|_| SlotError::SessionFailed("slot actor channel closed".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers;
    use std::collections::HashMap;
    use std::sync::{Arc as StdArc, Mutex};
    use std::time::Duration;
    use tempfile::TempDir;
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::Context;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{Layer, registry::LookupSpan};
    #[derive(Clone, Debug, Default)]
    struct RecordedSpan {
        name: String,
        fields: HashMap<String, String>,
    }
    #[derive(Clone, Debug, Default)]
    struct RecordedEvent {
        fields: HashMap<String, String>,
    }
    #[derive(Clone, Default)]
    struct RecordingLayer {
        spans: Arc<Mutex<Vec<RecordedSpan>>>,
        events: Arc<Mutex<Vec<RecordedEvent>>>,
    }
    impl RecordingLayer {
        fn spans(&self) -> Vec<RecordedSpan> {
            self.spans.lock().expect("recorded spans mutex").clone()
        }
        fn events(&self) -> Vec<RecordedEvent> {
            self.events.lock().expect("recorded events mutex").clone()
        }
    }
    #[derive(Default)]
    struct FieldRecorder {
        fields: HashMap<String, String>,
    }
    impl Visit for FieldRecorder {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.fields.insert(
                field.name().to_owned(),
                format!("{value:?}").trim_matches('"').to_owned(),
            );
        }
    }
    impl<S> Layer<S> for RecordingLayer
    where
        S: tracing::Subscriber,
        S: for<'lookup> LookupSpan<'lookup>,
    {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            id: &tracing::Id,
            ctx: Context<'_, S>,
        ) {
            let mut recorder = FieldRecorder::default();
            attrs.record(&mut recorder);
            if let Some(span) = ctx.span(id) {
                span.extensions_mut().insert(RecordedSpan {
                    name: attrs.metadata().name().to_owned(),
                    fields: recorder.fields,
                });
            }
        }
        fn on_record(
            &self,
            id: &tracing::Id,
            values: &tracing::span::Record<'_>,
            ctx: Context<'_, S>,
        ) {
            if let Some(span) = ctx.span(id) {
                let mut recorder = FieldRecorder::default();
                values.record(&mut recorder);
                if let Some(recorded) = span.extensions_mut().get_mut::<RecordedSpan>() {
                    recorded.fields.extend(recorder.fields);
                }
            }
        }
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            let mut recorder = FieldRecorder::default();
            event.record(&mut recorder);
            self.events
                .lock()
                .expect("recorded events mutex")
                .push(RecordedEvent {
                    fields: recorder.fields,
                });
        }
        fn on_close(&self, id: tracing::Id, ctx: Context<'_, S>) {
            if let Some(span) = ctx.span(&id)
                && let Some(recorded) = span.extensions().get::<RecordedSpan>()
            {
                self.spans
                    .lock()
                    .expect("recorded spans mutex")
                    .push(recorded.clone());
            }
        }
    }
    async fn tracing_lock() -> tokio::sync::OwnedMutexGuard<()> {
        static LOCK: std::sync::OnceLock<StdArc<tokio::sync::Mutex<()>>> =
            std::sync::OnceLock::new();
        LOCK.get_or_init(|| StdArc::new(tokio::sync::Mutex::new(())))
            .clone()
            .lock_owned()
            .await
    }
    fn test_app_state() -> (SlotContext, CancellationToken, TempDir) {
        let db = test_helpers::create_test_db();
        let cancel = CancellationToken::new();
        let temp = test_helpers::test_tempdir("djinn-slot-actor-");
        (
            test_helpers::agent_context_from_db(db, cancel.clone()),
            cancel,
            temp,
        )
    }
    #[tokio::test(flavor = "current_thread")]
    async fn run_task_completes_and_emits_free_event() {
        let _tracing_guard = tracing_lock().await;
        let layer = RecordingLayer::default();
        let subscriber = tracing_subscriber::registry().with(layer.clone());
        let _subscriber_guard = tracing::subscriber::set_default(subscriber);
        let (app_state, cancel, _temp) = test_app_state();
        let (event_tx, mut event_rx) = mpsc::channel(4);
        let runner: LifecycleRunner = Arc::new(
            |_task_id,
             _project_path,
             _model_id,
             _app_state,
             _kill,
             _pause,
             _resume_lifecycle_metadata| {
                Box::pin(async move {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    Ok(())
                })
            },
        );
        let slot = SlotHandle::spawn_with_runner(
            7,
            "test/mock".to_string(),
            event_tx,
            app_state,
            cancel,
            runner,
        );
        slot.run_task("task-123".to_string(), "/tmp/project".to_string())
            .await
            .expect("dispatch should be accepted");
        let evt = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .expect("event should arrive")
            .expect("event channel should stay open");
        match evt {
            SlotEvent::Free {
                slot_id,
                model_id,
                task_id,
            } => {
                assert_eq!(slot_id, 7);
                assert_eq!(model_id, "test/mock");
                assert_eq!(task_id, "task-123");
            }
            other => panic!("expected SlotEvent::Free, got {other:?}"),
        }
        let run_span = layer
            .spans()
            .into_iter()
            .find(|span| {
                span.name == "djinn.slot.run_task"
                    && span.fields.get("task_id").map(String::as_str) == Some("task-123")
            })
            .expect("djinn.slot.run_task span recorded");
        assert_eq!(
            run_span.fields.get("slot_id").map(String::as_str),
            Some("7")
        );
        assert_eq!(
            run_span.fields.get("model_id").map(String::as_str),
            Some("test/mock")
        );
        let free_event = layer
            .events()
            .into_iter()
            .find(|event| event.fields.get("event").map(String::as_str) == Some("slot.free"))
            .expect("slot.free child event recorded");
        assert_eq!(
            free_event.fields.get("task_id").map(String::as_str),
            Some("task-123")
        );
    }
    #[tokio::test(flavor = "current_thread")]
    async fn kill_emits_killed_event_and_kill_span_with_task_id() {
        let _tracing_guard = tracing_lock().await;
        let layer = RecordingLayer::default();
        let subscriber = tracing_subscriber::registry().with(layer.clone());
        let _subscriber_guard = tracing::subscriber::set_default(subscriber);
        let (app_state, cancel, _temp) = test_app_state();
        let (event_tx, mut event_rx) = mpsc::channel(4);
        let runner: LifecycleRunner = Arc::new(
            |_task_id,
             _project_path,
             _model_id,
             _app_state,
             kill,
             _pause,
             _resume_lifecycle_metadata| {
                Box::pin(async move {
                    kill.cancelled().await;
                    Ok(())
                })
            },
        );
        let slot = SlotHandle::spawn_with_runner(
            9,
            "test/kill-model".to_string(),
            event_tx,
            app_state,
            cancel,
            runner,
        );
        slot.run_task("task-kill".to_string(), "/tmp/project".to_string())
            .await
            .expect("dispatch should be accepted");
        slot.kill().await.expect("kill should be accepted");
        let evt = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .expect("event should arrive")
            .expect("event channel should stay open");
        match evt {
            SlotEvent::Killed {
                slot_id,
                model_id,
                task_id,
            } => {
                assert_eq!(slot_id, 9);
                assert_eq!(model_id, "test/kill-model");
                assert_eq!(task_id, "task-kill");
            }
            other => panic!("expected SlotEvent::Killed, got {other:?}"),
        }
        let kill_span = layer
            .spans()
            .into_iter()
            .find(|span| {
                span.name == "djinn.slot.kill"
                    && span.fields.get("task_id").map(String::as_str) == Some("task-kill")
            })
            .expect("djinn.slot.kill span recorded with task_id");
        assert_eq!(
            kill_span.fields.get("slot_id").map(String::as_str),
            Some("9")
        );
        assert_eq!(
            kill_span.fields.get("model_id").map(String::as_str),
            Some("test/kill-model")
        );
    }
    /// Additive re-dispatch path: the slot actor must hand the optional
    /// resume-via-git lifecycle metadata blob to the lifecycle runner so it
    /// lands on `TaskRunSpec::resume_lifecycle_metadata`. The runner records
    /// the metadata it received; the assertion catches the "selector was
    /// invoked and then dropped" failure mode (the bug the prior review
    /// flagged in the dispatch path).
    #[tokio::test(flavor = "current_thread")]
    async fn run_task_with_resume_forwards_metadata_to_runner() {
        let _tracing_guard = tracing_lock().await;
        let (app_state, cancel, _temp) = test_app_state();
        let (event_tx, mut event_rx) = mpsc::channel(4);
        // Shared slot to capture what the runner received.
        let observed: std::sync::Arc<std::sync::Mutex<Option<serde_json::Value>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let observed_clone = observed.clone();
        let runner: LifecycleRunner = Arc::new(
            move |_task_id,
                  _project_path,
                  _model_id,
                  _app_state,
                  _kill,
                  _pause,
                  resume_lifecycle_metadata| {
                let observed = observed_clone.clone();
                Box::pin(async move {
                    *observed.lock().expect("observed mutex") = resume_lifecycle_metadata;
                    Ok(())
                })
            },
        );
        let slot = SlotHandle::spawn_with_runner(
            11,
            "test/resume-model".to_string(),
            event_tx,
            app_state,
            cancel,
            runner,
        );
        // Build a representative resume metadata blob (mirrors
        // `djinn_runtime::ResumeLifecycleMetadata`).
        let metadata = serde_json::json!({
            "considered": true,
            "checkpoint_id": "ckpt-1",
            "commit_sha": "deadbeef",
            "selection_reason": "latest_safe_checkpoint",
            "source_kind": "task_branch_checkpoint",
            "target_ref": "refs/heads/task/test",
            "submit_or_review_id": "review-7",
            "prior_session_lineage": "session-prev",
            "skipped": [
                {"kind": "auto_submit", "reason": "auto_submit_not_accepted"}
            ]
        });
        slot.run_task_with_resume(
            "task-resume".to_string(),
            "/tmp/project".to_string(),
            Some(metadata.clone()),
        )
        .await
        .expect("dispatch with resume metadata should be accepted");
        // Wait for the lifecycle to complete (the runner returns Ok(()) on its
        // own after recording the metadata).
        let _ = tokio::time::timeout(Duration::from_secs(1), event_rx.recv()).await;
        let recorded = observed.lock().expect("observed mutex").clone();
        assert_eq!(
            recorded,
            Some(metadata),
            "slot pipeline must hand the resume metadata blob to the lifecycle runner \
             so downstream `TaskRunSpec::resume_lifecycle_metadata` carries the full selection"
        );
    }
    /// Default/off path: the legacy `run_task` entry point must thread
    /// `None` for the resume metadata, preserving the byte-for-byte
    /// existing dispatch contract. A bug here would silently resume tasks
    /// that the coordinator did not select for resume.
    #[tokio::test(flavor = "current_thread")]
    async fn run_task_default_off_threads_no_resume_metadata() {
        let _tracing_guard = tracing_lock().await;
        let (app_state, cancel, _temp) = test_app_state();
        let (event_tx, mut event_rx) = mpsc::channel(4);
        let observed: std::sync::Arc<std::sync::Mutex<Option<serde_json::Value>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let observed_clone = observed.clone();
        let runner: LifecycleRunner = Arc::new(
            move |_task_id,
                  _project_path,
                  _model_id,
                  _app_state,
                  _kill,
                  _pause,
                  resume_lifecycle_metadata| {
                let observed = observed_clone.clone();
                Box::pin(async move {
                    *observed.lock().expect("observed mutex") = resume_lifecycle_metadata;
                    Ok(())
                })
            },
        );
        let slot = SlotHandle::spawn_with_runner(
            12,
            "test/default-model".to_string(),
            event_tx,
            app_state,
            cancel,
            runner,
        );
        slot.run_task("task-default".to_string(), "/tmp/project".to_string())
            .await
            .expect("legacy dispatch should be accepted");
        let _ = tokio::time::timeout(Duration::from_secs(1), event_rx.recv()).await;
        let recorded = observed.lock().expect("observed mutex").clone();
        assert_eq!(
            recorded, None,
            "legacy run_task must thread `None` for resume metadata; \
             the disabled/default-off path must not silently inject selections"
        );
    }
}
