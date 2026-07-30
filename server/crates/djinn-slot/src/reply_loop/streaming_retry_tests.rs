//! Regression coverage for the bounded retry of **transient mid-stream**
//! provider errors (`reply_loop::streaming`).
//!
//! Before this existed, a single transient upstream overload event destroyed an
//! entire agent session: `consume_provider_stream` special-cased only
//! context-length and orphaned-tool-call errors, and every other stream error
//! fell straight through to `return Err(...)`. The HTTP-level `'retry` loop in
//! `djinn_provider::provider::client` could not help — it breaks out of the
//! retry loop the moment the provider answers `200 OK`, so an error arriving as
//! an SSE *event* has already left that code path behind.
//!
//! The retry budget is **error-class-aware**: throttle-class failures
//! ([`ProviderError::is_throttle`] — rate limit / quota / overload) get the
//! deep `MAX_THROTTLE_STREAM_EVENT_RETRIES` schedule (~3 minutes of patience,
//! reaching the 30s backoff cap), because capacity-shedding episodes last
//! minutes; every other retryable class keeps the shallow
//! `MAX_STREAM_EVENT_RETRIES`. The 2026-07-30 incident behind the split:
//! overnight `server_is_overloaded` bursts killed planner sessions ~11 seconds
//! after start — the shallow retry burned all three requests inside one burst.
//!
//! Every test here asserts a **session-level side effect** (did the session
//! survive? how many provider requests were actually issued? how long did it
//! really wait?), never that a predicate returns `true`.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use djinn_db::SessionRepository;
use djinn_db::repositories::session::CreateSessionParams;
use djinn_provider::message::{ContentBlock, Conversation, Message};
use djinn_provider::provider::{LlmProvider, ProviderError, StreamEvent, ToolChoice};
use futures::stream;
use tokio_util::sync::CancellationToken;

use super::streaming::{MAX_STREAM_EVENT_RETRIES, MAX_THROTTLE_STREAM_EVENT_RETRIES};
use super::turn::{ReplyLoopContext, run_reply_loop};
use crate::test_helpers;

/// The literal upstream payload from the `2gq7` incident:
/// `{"type":"error","error":{"type":"service_unavailable_error",
///   "code":"server_is_overloaded","message":"Our servers are currently
///   overloaded. Please try again later."}}`
const OVERLOAD_CODE: &str = "server_is_overloaded";
const OVERLOAD_MESSAGE: &str = "Our servers are currently overloaded. Please try again later.";

/// Build the error the OpenAI Responses adapter produces for that payload:
/// a typed [`ProviderError`] source carrying the readable message as context.
///
/// Deliberately routed through [`ProviderError::from_stream_error`] rather than
/// naming a variant, so this stays faithful to the production classification
/// path (and to any future re-classification of the overload code).
fn overload_stream_error() -> anyhow::Error {
    anyhow::Error::new(ProviderError::from_stream_error(
        Some(OVERLOAD_CODE),
        OVERLOAD_MESSAGE,
    ))
    .context(format!("{OVERLOAD_CODE}: {OVERLOAD_MESSAGE}"))
}

/// A non-retryable failure, used as the negative control.
fn auth_stream_error() -> anyhow::Error {
    anyhow::Error::new(ProviderError::from_stream_error(
        Some("invalid_api_key"),
        "Incorrect API key provided.",
    ))
    .context("invalid_api_key: Incorrect API key provided.")
}

/// A retryable **fault** (provider-internal 5xx) — retried, but only on the
/// shallow budget: a fault is not a queue you can wait out.
fn server_fault_stream_error() -> anyhow::Error {
    anyhow::Error::new(ProviderError::from_stream_error(
        Some("server_error"),
        "An error occurred.",
    ))
    .context("server_error: An error occurred.")
}

/// Server-supplied Retry-After used by the flooring scenario. Two minutes:
/// far above every backoff step (including the 30s cap), so a wait of this
/// length is only explicable by the floor being honored.
const RETRY_AFTER_FLOOR_MS: u64 = 120_000;

/// A throttle carrying an explicit server-supplied Retry-After, which must
/// floor the backoff delay.
fn floored_throttle_stream_error() -> anyhow::Error {
    anyhow::Error::new(ProviderError::RateLimit {
        retry_after_ms: Some(RETRY_AFTER_FLOOR_MS),
    })
    .context("rate_limit_exceeded: Retry-After supplied.")
}

/// What a single `LlmProvider::stream()` call yields.
#[derive(Clone)]
enum TurnScript {
    /// The stream's very first item is an error — nothing was emitted, so the
    /// round is safely restartable.
    ErrorFirst(&'static str),
    /// A text delta reaches the transcript, *then* the stream errors. Retrying
    /// here would duplicate downstream output, so it must not be retried.
    TextThenError(&'static str, &'static str),
    /// A complete, successful text turn.
    Text(&'static str),
}

fn scripted_error(kind: &str) -> anyhow::Error {
    match kind {
        "overload" => overload_stream_error(),
        "auth" => auth_stream_error(),
        "server_fault" => server_fault_stream_error(),
        "floored_throttle" => floored_throttle_stream_error(),
        other => panic!("unknown scripted error kind {other}"),
    }
}

/// An `LlmProvider` that plays a fixed script of turns and then repeats `tail`
/// forever, counting how many times `stream()` was actually called.
///
/// The call count is the load-bearing assertion: it proves whether a retry did
/// or did not re-issue the request.
struct ScriptedStreamProvider {
    turns: Mutex<VecDeque<TurnScript>>,
    tail: TurnScript,
    stream_calls: AtomicUsize,
}

impl ScriptedStreamProvider {
    fn new(turns: Vec<TurnScript>, tail: TurnScript) -> Self {
        Self {
            turns: Mutex::new(turns.into()),
            tail,
            stream_calls: AtomicUsize::new(0),
        }
    }

    /// Number of provider requests issued — i.e. `1 + retries actually taken`.
    fn stream_calls(&self) -> usize {
        self.stream_calls.load(Ordering::SeqCst)
    }
}

impl LlmProvider for ScriptedStreamProvider {
    fn name(&self) -> &str {
        "scripted-stream"
    }

    fn stream<'a>(
        &'a self,
        _conversation: &'a Conversation,
        _tools: &'a [serde_json::Value],
        _tool_choice: Option<ToolChoice>,
    ) -> Pin<
        Box<
            dyn futures::Future<
                    Output = anyhow::Result<
                        Pin<Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>>,
                    >,
                > + Send
                + 'a,
        >,
    > {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        let script = {
            let mut turns = self.turns.lock().expect("scripted turns mutex");
            turns.pop_front().unwrap_or_else(|| self.tail.clone())
        };
        Box::pin(async move {
            let events: Vec<anyhow::Result<StreamEvent>> = match script {
                TurnScript::ErrorFirst(kind) => vec![Err(scripted_error(kind))],
                TurnScript::TextThenError(text, kind) => vec![
                    Ok(StreamEvent::Delta(ContentBlock::Text {
                        text: text.to_string(),
                    })),
                    Err(scripted_error(kind)),
                ],
                TurnScript::Text(text) => vec![
                    Ok(StreamEvent::Delta(ContentBlock::Text {
                        text: text.to_string(),
                    })),
                    Ok(StreamEvent::Done),
                ],
            };
            Ok(Box::pin(stream::iter(events))
                as Pin<
                    Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>,
                >)
        })
    }
}

/// Minimal reply-loop fixture: a real in-memory DB with the project / epic /
/// task / task-run / session rows the loop's persistence path requires.
struct Fixture {
    slot_ctx: crate::host::SlotContext,
    project_path: String,
    task_id: String,
    session_id: String,
    cancel: CancellationToken,
    conversation: Conversation,
}

impl Fixture {
    async fn new() -> Self {
        let cancel = CancellationToken::new();
        let db = test_helpers::create_test_db();
        let slot_ctx = test_helpers::agent_context_from_db(db.clone(), cancel.clone());
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;
        let task_run_id = uuid::Uuid::now_v7().to_string();
        djinn_db::repositories::task_run::TaskRunRepository::new(db.clone())
            .create(djinn_db::repositories::task_run::CreateTaskRunParams {
                id: &task_run_id,
                project_id: &project.id,
                task_id: &task.id,
                trigger_type: "dispatch",
                status: Some("running"),
                workspace_path: Some("/tmp"),
                mirror_ref: None,
                dispatch_group_id: None,
            })
            .await
            .expect("create active task run");
        let session = SessionRepository::new(db.clone(), slot_ctx.event_bus.clone())
            .create(CreateSessionParams {
                project_id: &project.id,
                task_id: Some(&task.id),
                model: "test/mock-model",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .expect("create session");
        let project_path =
            djinn_core::paths::project_dir(&project.github_owner, &project.github_repo)
                .to_string_lossy()
                .into_owned();
        let mut conversation = Conversation::new();
        conversation.push(Message::system("You are a worker."));
        conversation.push(Message::user("Do the task."));
        Self {
            slot_ctx,
            project_path,
            task_id: task.id.clone(),
            session_id: session.id.clone(),
            cancel,
            conversation,
        }
    }

    async fn run(&mut self, provider: &ScriptedStreamProvider) -> anyhow::Result<()> {
        let worktree_path = std::path::PathBuf::from("/tmp");
        let (result, _output, _in, _out, _cr, _cw) = run_reply_loop(
            ReplyLoopContext {
                compaction_cs: &super::CompactionCriticalSection::new(),
                provider,
                tools: &[],
                task_id: &self.task_id,
                task_short_id: "t1",
                session_id: &self.session_id,
                project_path: &self.project_path,
                worktree_path: &worktree_path,
                role_name: "worker",
                finalize_tool_names: &["submit_work", "request_planner"],
                context_window: 100_000,
                model_id: "test/mock-model",
                cancel: &self.cancel,
                global_cancel: &self.cancel,
                ctx: &self.slot_ctx,
                active_skill_names: &[],
                active_mcp_server_names: &[],
                max_turns_override: None,
            },
            &mut self.conversation,
            false,
        )
        .await;
        result
    }
}

/// Drive `consume_provider_stream` directly with `provider` under a paused
/// tokio clock, returning the stream-consumer result plus the **virtual**
/// time the retry schedule spent waiting.
///
/// The reply-loop fixture above cannot host the deep-budget scenarios: its
/// persistence path performs real Postgres I/O between turns, and under a
/// paused clock sqlx's pool-acquire timeout auto-advances past real TCP I/O
/// and fires spuriously (see the `provider_phase_scripted_reply_loop_scenarios`
/// note in `reply_loop/tests.rs`). `consume_provider_stream` itself performs
/// no DB I/O — the test `SlotContext`'s host callbacks are no-ops — so the
/// clock is paused only around the drive (DB/pool setup stays under real
/// time), letting a ~3-minute backoff schedule run instantly while
/// `tokio::time::Instant` still measures exactly how long the session would
/// really have waited.
async fn drive_stream_directly(
    provider: &ScriptedStreamProvider,
) -> (
    anyhow::Result<super::streaming::StreamTurnState>,
    std::time::Duration,
) {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    use super::streaming::{StreamLoopContext, consume_provider_stream};

    // DB + context setup under REAL time: pool establishment is TCP I/O.
    let db = test_helpers::create_test_db();
    db.ensure_initialized().await.expect("db init");
    let slot_ctx = test_helpers::agent_context_from_db(db, CancellationToken::new());
    let cancel = CancellationToken::new();
    let global_cancel = CancellationToken::new();
    let tool_metadata = super::tool_dispatch::tool_runtime_metadata(&[]);
    let conversation = Conversation::new();
    let dispatcher = slot_ctx
        .tool_dispatcher
        .as_deref()
        .expect("test SlotContext has a tool dispatcher");
    let phase_tracker = Arc::new(Mutex::new(super::phase::SessionPhaseTracker::new(
        &slot_ctx, "worker",
    )));
    let dispatch_ctx = super::tool_dispatch::ToolDispatchContext {
        ctx: &slot_ctx,
        task_id: "task",
        worktree_path: std::path::Path::new("/tmp"),
        role_name: "worker",
        tool_metadata: &tool_metadata,
        tool_dispatcher: dispatcher,
        otel_session: None,
        phase_tracker: None,
        cancel: &cancel,
    };
    let activity_ts = Arc::new(AtomicU64::new(0));
    let last_rpc_touch = Arc::new(AtomicU64::new(0));
    let last_token_flush = Arc::new(AtomicU64::new(0));
    let mut current_context_tokens = 0u32;
    let mut total_tokens_in = 0u32;
    let mut total_tokens_out = 0u32;
    let mut total_cache_read = 0u32;
    let mut total_cache_write = 0u32;
    let mut total_reasoning_out = 0u32;

    tokio::time::pause();
    let started = tokio::time::Instant::now();
    let stream = provider
        .stream(&conversation, &[], None)
        .await
        .expect("initial stream");
    let result = consume_provider_stream(StreamLoopContext {
        provider,
        stream,
        request_conversation: &conversation,
        request_tools: &[],
        request_tool_choice: None,
        tool_metadata: &tool_metadata,
        dispatch: &dispatch_ctx,
        phase_tracker: &phase_tracker,
        task_id: "task",
        session_id: "session",
        role_name: "worker",
        project_path: "/tmp",
        worktree_path: std::path::Path::new("/tmp"),
        context_window: 100_000,
        ctx: &slot_ctx,
        cancel: &cancel,
        global_cancel: &global_cancel,
        activity_ts: &activity_ts,
        last_rpc_touch: &last_rpc_touch,
        last_token_flush: &last_token_flush,
        compaction_attempts: 0,
        current_context_tokens: &mut current_context_tokens,
        total_tokens_in: &mut total_tokens_in,
        total_tokens_out: &mut total_tokens_out,
        total_cache_read: &mut total_cache_read,
        total_cache_write: &mut total_cache_write,
        total_reasoning_out: &mut total_reasoning_out,
    })
    .await;
    let waited = started.elapsed();
    tokio::time::resume();
    (result, waited)
}

/// AC1 — the core assertion. A stream that dies on a transient
/// `server_is_overloaded` event and then succeeds on the retry produces a
/// session that **completes successfully**.
///
/// Without the retry this returns `Err` on the very first error event and the
/// second provider request is never issued (`stream_calls == 1`).
#[tokio::test]
async fn transient_mid_stream_overload_is_retried_and_the_session_survives() {
    let provider = ScriptedStreamProvider::new(
        vec![TurnScript::ErrorFirst("overload")],
        TurnScript::Text("All done."),
    );
    let mut fixture = Fixture::new().await;
    let result = fixture.run(&provider).await;

    assert!(
        result.is_ok(),
        "a transient mid-stream overload must not kill the session; got {:#}",
        result.unwrap_err()
    );
    assert_eq!(
        provider.stream_calls(),
        2,
        "the round must be re-issued exactly once after the transient error"
    );
    // The retried round's output really did land in the transcript.
    let assistant_text: String = fixture
        .conversation
        .messages
        .iter()
        .filter(|m| m.role == djinn_provider::message::Role::Assistant)
        .flat_map(|m| m.content.iter().filter_map(ContentBlock::as_text))
        .collect();
    assert!(
        assistant_text.contains("All done."),
        "the successful retry's assistant turn must be persisted; got {assistant_text:?}"
    );
}

/// AC2a — a throttle deeper than the old shallow cap still recovers the
/// session, end to end. Three consecutive `server_is_overloaded` events would
/// have exhausted the pre-class-aware budget (1 initial + 2 retries) and
/// killed the session; the throttle schedule rides them out and the fourth
/// request's output lands in the transcript.
///
/// Kept at the reply-loop level (real clock) so the assertion covers the full
/// session-level side effect; three 1s/2s/4s waits keep the real sleep budget
/// comparable to the suite's existing backoff tests.
#[tokio::test]
async fn overload_deeper_than_the_shallow_cap_still_recovers_the_session() {
    let provider = ScriptedStreamProvider::new(
        vec![
            TurnScript::ErrorFirst("overload"),
            TurnScript::ErrorFirst("overload"),
            TurnScript::ErrorFirst("overload"),
        ],
        TurnScript::Text("All done."),
    );
    let mut fixture = Fixture::new().await;
    let result = fixture.run(&provider).await;

    assert!(
        result.is_ok(),
        "an overload burst outlasting the shallow budget must not kill the session; got {:#}",
        result.unwrap_err()
    );
    assert_eq!(
        provider.stream_calls(),
        4,
        "the round must be re-issued once per overload event — one attempt past \
         the shallow cap of {} total requests",
        1 + MAX_STREAM_EVENT_RETRIES
    );
    let assistant_text: String = fixture
        .conversation
        .messages
        .iter()
        .filter(|m| m.role == djinn_provider::message::Role::Assistant)
        .flat_map(|m| m.content.iter().filter_map(ContentBlock::as_text))
        .collect();
    assert!(
        assistant_text.contains("All done."),
        "the successful deep retry's assistant turn must be persisted; got {assistant_text:?}"
    );
}

/// AC2b — the deep throttle budget is still **bounded**: a provider that sheds
/// load forever exhausts all `MAX_THROTTLE_STREAM_EVENT_RETRIES` attempts over
/// roughly three minutes of (virtual) waiting, then surfaces the ORIGINAL
/// error with the same `provider stream event failed: ...` context the
/// coordinator's classification already relies on.
///
/// The elapsed-time bounds are load-bearing in both directions: the lower
/// bound proves the schedule actually reaches the 30s backoff cap (patience on
/// the order of minutes, not the old ~3 seconds), and the upper bound proves
/// the patience cannot silently grow into the coordinator's 30-minute stall
/// budget.
#[tokio::test]
async fn persistent_throttle_exhausts_the_deep_budget_after_minutes_of_patience() {
    let provider = ScriptedStreamProvider::new(vec![], TurnScript::ErrorFirst("overload"));
    let (result, waited) = drive_stream_directly(&provider).await;

    // `StreamTurnState` has no `Debug` impl, so `expect_err` cannot be used.
    let Err(err) = result else {
        panic!("a persistently overloaded provider must still fail the round");
    };
    let rendered = format!("{err:#}");
    assert!(
        rendered.contains("provider stream event failed"),
        "the terminal context string must be unchanged; got {rendered}"
    );
    assert!(
        rendered.contains(OVERLOAD_MESSAGE),
        "the original upstream error must be preserved; got {rendered}"
    );
    assert!(
        err.downcast_ref::<ProviderError>().is_some(),
        "the typed ProviderError source must survive for downstream classification"
    );
    assert_eq!(
        provider.stream_calls() as u32,
        1 + MAX_THROTTLE_STREAM_EVENT_RETRIES,
        "retries must stop at 1 initial request + MAX_THROTTLE_STREAM_EVENT_RETRIES"
    );
    // Base schedule 1+2+4+8+16+30·5 = 181s; jitter is 0.8x–1.2x per step.
    assert!(
        waited >= std::time::Duration::from_secs(140),
        "the deep schedule must wait minutes (reaching the 30s cap), not seconds; waited {waited:?}"
    );
    assert!(
        waited <= std::time::Duration::from_secs(230),
        "total patience must stay bounded around ~3 minutes; waited {waited:?}"
    );
}

/// AC2c — negative control for the class split. A retryable **fault**
/// (`server_error` → `ProviderInternal`) keeps the shallow budget: exactly
/// 1 initial + MAX_STREAM_EVENT_RETRIES requests, then the session fails with
/// the original error, exactly as before the throttle deepening.
#[tokio::test]
async fn persistent_non_throttle_fault_keeps_the_shallow_retry_cap() {
    let provider = ScriptedStreamProvider::new(vec![], TurnScript::ErrorFirst("server_fault"));
    let mut fixture = Fixture::new().await;
    let result = fixture.run(&provider).await;

    let err = result.expect_err("a persistent provider fault must still fail the session");
    let rendered = format!("{err:#}");
    assert!(
        rendered.contains("provider stream event failed"),
        "the terminal context string must be unchanged; got {rendered}"
    );
    assert!(
        rendered.contains("An error occurred."),
        "the original upstream error must be preserved; got {rendered}"
    );
    // Absolute lower bound first: this is the assertion that dies if the retry
    // does nothing. (A purely `MAX_STREAM_EVENT_RETRIES`-relative check would
    // self-adjust to a no-op fix and stay green.)
    assert!(
        provider.stream_calls() > 1,
        "the transient fault must have been retried at least once; got {} request(s)",
        provider.stream_calls()
    );
    // Then the upper bound: bounded by exactly the SHALLOW cap — the deep
    // throttle budget must not leak onto fault classes.
    assert_eq!(
        provider.stream_calls() as u32,
        1 + MAX_STREAM_EVENT_RETRIES,
        "fault-class retries must stop at 1 initial request + MAX_STREAM_EVENT_RETRIES"
    );
}

/// AC2d — a server-supplied Retry-After floors the backoff. The first
/// attempt's backoff step is ~1s, so a wait of two minutes before the single
/// successful re-issue is only explicable by the floor being honored; the
/// tight upper bound proves the floor replaced (rather than stacked onto) the
/// schedule.
#[tokio::test]
async fn retry_after_floors_the_backoff_delay() {
    let provider = ScriptedStreamProvider::new(
        vec![TurnScript::ErrorFirst("floored_throttle")],
        TurnScript::Text("All done."),
    );
    let (result, waited) = drive_stream_directly(&provider).await;

    let state = result.expect("the floored throttle must be retried and succeed");
    assert_eq!(
        state.turn_text, "All done.",
        "the retried round's output must be observed by the stream consumer"
    );
    assert_eq!(
        provider.stream_calls(),
        2,
        "exactly one re-issue after the floored wait"
    );
    let floor = std::time::Duration::from_millis(RETRY_AFTER_FLOOR_MS);
    assert!(
        waited >= floor,
        "the wait must honor the server-supplied Retry-After floor of {floor:?}; waited {waited:?}"
    );
    assert!(
        waited <= floor + std::time::Duration::from_secs(5),
        "the wait must be the floor itself, not the floor plus extra schedule; waited {waited:?}"
    );
}

/// AC3 — negative control. A non-retryable class (`Authentication`) is not
/// retried: exactly one provider request, immediate failure, as today.
#[tokio::test]
async fn non_retryable_mid_stream_error_is_not_retried() {
    let provider = ScriptedStreamProvider::new(vec![], TurnScript::ErrorFirst("auth"));
    let mut fixture = Fixture::new().await;
    let result = fixture.run(&provider).await;

    let err = result.expect_err("an authentication failure must terminate the session");
    assert!(
        format!("{err:#}").contains("Incorrect API key provided."),
        "the original auth error must be preserved; got {err:#}"
    );
    assert_eq!(
        provider.stream_calls(),
        1,
        "a non-retryable provider error must never re-issue the request"
    );
}

/// AC4 — safety control. Once the round has emitted meaningful downstream
/// output, the identical transient error is NOT retried: re-issuing would
/// duplicate the streamed text (and, for tool calls, re-run side effects).
#[tokio::test]
async fn transient_error_after_emitted_output_is_not_retried() {
    let provider = ScriptedStreamProvider::new(
        vec![],
        TurnScript::TextThenError("Here is my partial answer. ", "overload"),
    );
    let mut fixture = Fixture::new().await;
    let result = fixture.run(&provider).await;

    let err = result.expect_err("an unsafe-to-retry round must fail as it does today");
    assert!(
        format!("{err:#}").contains(OVERLOAD_MESSAGE),
        "the original error must be preserved; got {err:#}"
    );
    assert_eq!(
        provider.stream_calls(),
        1,
        "a round that already streamed output must never be re-issued"
    );
}
