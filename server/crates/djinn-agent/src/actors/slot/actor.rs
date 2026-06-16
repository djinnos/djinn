use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::context::AgentContext;

use super::supervisor_runner::run_supervisor_dispatch;
use super::{SlotCommand, SlotError, SlotEvent};

type LifecycleFuture = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'static>>;
type LifecycleRunner = Arc<
    dyn Fn(
            String,
            String,
            String,
            AgentContext,
            CancellationToken,
            CancellationToken,
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
}

pub struct SlotActor {
    id: usize,
    model_id: String,
    receiver: mpsc::Receiver<SlotCommand>,
    event_tx: mpsc::Sender<SlotEvent>,
    app_state: AgentContext,
    cancel: CancellationToken,
    runner: LifecycleRunner,
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
                            Some(SlotCommand::RunTask { respond_to, .. }) => {
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
                                let kill = CancellationToken::new();
                                let pause = CancellationToken::new();
                                let span = tracing::info_span!(
                                    "djinn.slot.run_task",
                                    slot_id = self.id,
                                    model_id = %self.model_id,
                                    task_id = %task_id,
                                );
                                let run = (self.runner)(
                                    task_id.clone(),
                                    project_path,
                                    self.model_id.clone(),
                                    self.app_state.clone(),
                                    kill.clone(),
                                    pause.clone(),
                                );

                                let join = tokio::spawn(run.instrument(span.clone()));
                                let _ = respond_to.send(Ok(()));
                                active = Some(ActiveLifecycle {
                                    task_id,
                                    join,
                                    kill,
                                    pause,
                                    span,
                                    killed: false,
                                });
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
        app_state: AgentContext,
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
        let runner: LifecycleRunner =
            Arc::new(|task_id, project_path, model_id, app_state, kill, pause| {
                Box::pin(run_supervisor_dispatch(
                    task_id,
                    project_path,
                    model_id,
                    app_state,
                    kill,
                    pause,
                ))
            });
        Self::spawn_with_runner(id, model_id, event_tx, app_state, cancel, runner)
    }

    fn spawn_with_runner(
        id: usize,
        model_id: String,
        event_tx: mpsc::Sender<SlotEvent>,
        app_state: AgentContext,
        cancel: CancellationToken,
        runner: LifecycleRunner,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(16);
        let actor = SlotActor {
            id,
            model_id: model_id.clone(),
            receiver,
            event_tx,
            app_state,
            cancel,
            runner,
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
        app_state: AgentContext,
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
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(SlotCommand::RunTask {
                task_id,
                project_path,
                respond_to: tx,
            })
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
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    use tempfile::TempDir;
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::Context;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{Layer, registry::LookupSpan};

    use super::*;
    use crate::test_helpers;

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

    fn tracing_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    fn test_app_state() -> (AgentContext, CancellationToken, TempDir) {
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
        let _tracing_guard = tracing_lock();
        let layer = RecordingLayer::default();
        let subscriber = tracing_subscriber::registry().with(layer.clone());
        let _subscriber_guard = tracing::subscriber::set_default(subscriber);
        let (app_state, cancel, _temp) = test_app_state();
        let (event_tx, mut event_rx) = mpsc::channel(4);

        let runner: LifecycleRunner = Arc::new(
            |_task_id, _project_path, _model_id, _app_state, _kill, _pause| {
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
        let _tracing_guard = tracing_lock();
        let layer = RecordingLayer::default();
        let subscriber = tracing_subscriber::registry().with(layer.clone());
        let _subscriber_guard = tracing::subscriber::set_default(subscriber);
        let (app_state, cancel, _temp) = test_app_state();
        let (event_tx, mut event_rx) = mpsc::channel(4);

        let runner: LifecycleRunner = Arc::new(
            |_task_id, _project_path, _model_id, _app_state, kill, _pause| {
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
}
