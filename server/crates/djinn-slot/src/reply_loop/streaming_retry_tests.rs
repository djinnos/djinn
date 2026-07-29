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
//! Every test here asserts a **session-level side effect** (did the session
//! survive? how many provider requests were actually issued?), never that a
//! predicate returns `true`.

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

use super::streaming::MAX_STREAM_EVENT_RETRIES;
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

/// AC2 — retries are bounded, and the ORIGINAL error survives to the caller
/// with the same `provider stream event failed: ...` context the coordinator's
/// classification already relies on.
#[tokio::test]
async fn persistent_mid_stream_overload_fails_after_the_retry_cap() {
    let provider = ScriptedStreamProvider::new(vec![], TurnScript::ErrorFirst("overload"));
    let mut fixture = Fixture::new().await;
    let result = fixture.run(&provider).await;

    let err = result.expect_err("a persistently overloaded provider must still fail the session");
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
    // Absolute lower bound first: this is the assertion that dies if the retry
    // does nothing. (A purely `MAX_STREAM_EVENT_RETRIES`-relative check would
    // self-adjust to a no-op fix and stay green.)
    assert!(
        provider.stream_calls() > 1,
        "the transient error must have been retried at least once; got {} request(s)",
        provider.stream_calls()
    );
    // Then the upper bound: bounded, and bounded by exactly the documented cap.
    assert_eq!(
        provider.stream_calls(),
        3,
        "retries must stop at 1 initial request + MAX_STREAM_EVENT_RETRIES ({MAX_STREAM_EVENT_RETRIES})"
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
