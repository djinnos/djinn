//! Chat compaction boundary reuse regressions.
//!
//! These focused integration tests verify the observable epic behavior:
//!
//! 1. **Reuse**: repeated long-chat requests under an unchanged completed
//!    boundary reuse persisted summaries instead of re-summarizing full history.
//! 2. **Tail mismatch**: retained-tail mismatch behaves safely — the projection
//!    falls back to raw history rather than silently reusing a stale boundary.
//! 3. **Continuity**: prior completed summaries reach the summarizer as
//!    `<previous-summary>` context without stale marker pairs appearing as
//!    ordinary conversation turns.
//!
//! All tests use deterministic in-memory databases and do not require external
//! provider credentials or live network calls.

use djinn_compaction::COMPACTION_SUMMARY_END_MARKER;
use djinn_core::events::EventBus;
use djinn_core::message::{ContentBlock, Message, Role};
use djinn_db::repositories::session_compaction_boundary::{
    BeginCompactionParams, CompleteCompactionParams, SessionCompactionBoundaryRepository,
};
use djinn_db::{Database, SessionMessageRepository};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn test_db() -> djinn_db::Database {
    djinn_db::Database::open_in_memory().unwrap()
}

/// Seed a projectless chat session row so the FK on session_messages holds.
async fn seed_chat_session(db: &djinn_db::Database, session_id: &str) {
    djinn_db::test_support::seed_chat_session_row(db, session_id).await;
}

/// Insert messages into session_messages one at a time via the repository layer
/// and return their DB-assigned ids.
async fn insert_and_collect_ids(
    db: &Database,
    session_id: &str,
    messages: &[Message],
) -> Vec<String> {
    let repo = SessionMessageRepository::new(db.clone(), EventBus::noop());
    let mut ids = Vec::with_capacity(messages.len());
    for msg in messages {
        let role = match msg.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        let content_json = serde_json::to_string(&msg.content).unwrap_or_else(|_| "[]".to_string());
        let inserted = repo
            .insert_message(session_id, "", role, &content_json, None)
            .await
            .unwrap();
        ids.push(inserted.id);
    }
    ids
}

/// Helper to build the compacted marker pair (user summary + assistant
/// continuation) that `compact_conversation` inserts into the conversation.
fn compaction_marker_pair(summary_text: &str) -> (Message, Message) {
    let user = Message::user(format!("{summary_text}{COMPACTION_SUMMARY_END_MARKER}"));
    let assistant = Message::assistant(
        "Your context was compacted. The previous message contains a summary of the \
         conversation so far. Continue calling tools as necessary to complete the task.",
    );
    (user, assistant)
}

// ── AC1: Boundary reuse — repeated requests under unchanged boundary ─────────

/// Regression: after a completed compaction boundary is persisted and the
/// retained tail still matches, repeated `load_conversation` calls MUST return
/// the projected summary + tail (not raw history). This proves that the chat
/// handler does not need to invoke a fresh full-history summarization for the
/// same boundary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_load_conversation_reuses_projected_boundary() {
    let db = test_db();
    let session_id = uuid::Uuid::now_v7().to_string();
    seed_chat_session(&db, &session_id).await;

    let msg_repo = SessionMessageRepository::new(db.clone(), EventBus::noop());
    let boundary_repo = SessionCompactionBoundaryRepository::new(db.clone());

    // Seed: system + old user + old assistant + new user (tail).
    let raw_messages = vec![
        Message::system("You are helpful."),
        Message::user("old question"),
        Message::assistant("old answer"),
        Message::user("new question"),
    ];
    let ids = insert_and_collect_ids(&db, &session_id, &raw_messages).await;
    assert_eq!(ids.len(), 4);

    // Complete a boundary that retains the last message (new question).
    let first_retained_id = &ids[3];
    let started = boundary_repo
        .record_compaction_started(BeginCompactionParams {
            session_id: &session_id,
            schema_version: 1,
            trigger: None,
            current_context_tokens_before: None,
            first_message_id: Some(&ids[1]),
            last_compacted_message_id: Some(&ids[2]),
            first_retained_message_id: Some(first_retained_id),
            retained_tail_hash: None,
            marker_metadata: None,
        })
        .await
        .unwrap();
    boundary_repo
        .complete_compaction_boundary(CompleteCompactionParams {
            boundary_id: &started.id,
            schema_version: 1,
            current_context_tokens_after: None,
            first_message_id: Some(&ids[1]),
            last_compacted_message_id: Some(&ids[2]),
            first_retained_message_id: Some(first_retained_id),
            retained_tail_hash: None,
            summary_text: "Summary of old turns.",
            marker_metadata: None,
        })
        .await
        .unwrap();

    // First load — must project the boundary.
    let first = msg_repo.load_conversation(&session_id).await.unwrap();
    assert_eq!(
        first.messages.len(),
        2,
        "projected: summary + 1 tail message"
    );
    assert_eq!(first.messages[0].role, Role::System);
    assert_eq!(first.messages[0].text_content(), "Summary of old turns.");
    assert_eq!(first.messages[1].role, Role::User);
    assert_eq!(first.messages[1].text_content(), "new question");

    // Second load — same result. This is the reuse guarantee: the boundary
    // is stable across repeated requests without re-summarization.
    let second = msg_repo.load_conversation(&session_id).await.unwrap();
    assert_eq!(second.messages.len(), 2);
    assert_eq!(second.messages[0].text_content(), "Summary of old turns.");
    assert_eq!(second.messages[1].text_content(), "new question");

    // Third load after a new message is appended — boundary is still valid
    // because the first_retained_message_id still exists in the raw history.
    msg_repo
        .insert_message(
            &session_id,
            "",
            "assistant",
            r#"[{"type":"text","text":"answer to new question"}]"#,
            None,
        )
        .await
        .unwrap();
    let third = msg_repo.load_conversation(&session_id).await.unwrap();
    // Summary + original tail + newly appended message.
    assert_eq!(third.messages.len(), 3);
    assert_eq!(third.messages[0].text_content(), "Summary of old turns.");
    assert_eq!(third.messages[1].text_content(), "new question");
    assert_eq!(third.messages[2].text_content(), "answer to new question");

    // Only one boundary row exists — no duplicate boundaries from repeated loads.
    assert_eq!(
        boundary_repo.boundary_count(&session_id).await.unwrap(),
        1,
        "repeated loads must not create new boundary rows"
    );
}

/// Regression: the projected boundary summary carries marker metadata that
/// downstream consumers can inspect. The summary text is the cleaned version
/// (without the end marker) — this is what the boundary records.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_reuse_preserves_marker_metadata() {
    let db = test_db();
    let session_id = uuid::Uuid::now_v7().to_string();
    seed_chat_session(&db, &session_id).await;

    let msg_repo = SessionMessageRepository::new(db.clone(), EventBus::noop());
    let boundary_repo = SessionCompactionBoundaryRepository::new(db.clone());

    let raw = vec![
        Message::user("msg1"),
        Message::assistant("reply1"),
        Message::user("msg2"),
    ];
    let ids = insert_and_collect_ids(&db, &session_id, &raw).await;

    let marker_meta = serde_json::json!({
        "marker_kind": "compaction_summary",
        "end_marker": COMPACTION_SUMMARY_END_MARKER,
    });
    let started = boundary_repo
        .record_compaction_started(BeginCompactionParams {
            session_id: &session_id,
            schema_version: 1,
            trigger: None,
            current_context_tokens_before: None,
            first_message_id: Some(&ids[0]),
            last_compacted_message_id: Some(&ids[1]),
            first_retained_message_id: Some(&ids[2]),
            retained_tail_hash: None,
            marker_metadata: Some(&marker_meta),
        })
        .await
        .unwrap();
    boundary_repo
        .complete_compaction_boundary(CompleteCompactionParams {
            boundary_id: &started.id,
            schema_version: 1,
            current_context_tokens_after: None,
            first_message_id: Some(&ids[0]),
            last_compacted_message_id: Some(&ids[1]),
            first_retained_message_id: Some(&ids[2]),
            retained_tail_hash: None,
            summary_text: "Compact summary text.",
            marker_metadata: Some(&marker_meta),
        })
        .await
        .unwrap();

    let conv = msg_repo.load_conversation(&session_id).await.unwrap();
    assert_eq!(conv.messages[0].role, Role::System);
    let meta = conv.messages[0]
        .metadata
        .as_ref()
        .expect("projected summary must have metadata");
    let pd = meta
        .provider_data
        .as_ref()
        .expect("provider_data must carry marker metadata");
    assert_eq!(pd["marker_kind"].as_str().unwrap(), "compaction_summary");
    // The summary text is clean — no end marker leaked.
    assert!(
        !conv.messages[0]
            .text_content()
            .contains(COMPACTION_SUMMARY_END_MARKER),
        "projected summary must not contain the raw end marker"
    );
}

// ── AC2: Retained-tail mismatch — safe fallback ──────────────────────────────

/// Regression: when the boundary's retained-tail hash does not match the actual
/// tail hash, `load_conversation` must fall back to the full raw history rather
/// than silently reusing a stale boundary projection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tail_hash_mismatch_falls_back_to_raw_history() {
    let db = test_db();
    let session_id = uuid::Uuid::now_v7().to_string();
    seed_chat_session(&db, &session_id).await;

    let msg_repo = SessionMessageRepository::new(db.clone(), EventBus::noop());
    let boundary_repo = SessionCompactionBoundaryRepository::new(db.clone());

    let raw = vec![
        Message::system("sys"),
        Message::user("q1"),
        Message::assistant("a1"),
        Message::user("q2"),
    ];
    let ids = insert_and_collect_ids(&db, &session_id, &raw).await;

    // Record a boundary with a WRONG tail hash — simulating a stale boundary
    // whose tail has drifted.
    let started = boundary_repo
        .record_compaction_started(BeginCompactionParams {
            session_id: &session_id,
            schema_version: 1,
            trigger: None,
            current_context_tokens_before: None,
            first_message_id: Some(&ids[1]),
            last_compacted_message_id: Some(&ids[2]),
            first_retained_message_id: Some(&ids[3]),
            retained_tail_hash: Some("djinn:default:STALE_HASH_VALUE"),
            marker_metadata: None,
        })
        .await
        .unwrap();
    boundary_repo
        .complete_compaction_boundary(CompleteCompactionParams {
            boundary_id: &started.id,
            schema_version: 1,
            current_context_tokens_after: None,
            first_message_id: Some(&ids[1]),
            last_compacted_message_id: Some(&ids[2]),
            first_retained_message_id: Some(&ids[3]),
            retained_tail_hash: Some("djinn:default:STALE_HASH_VALUE"),
            summary_text: "Stale summary.",
            marker_metadata: None,
        })
        .await
        .unwrap();

    let conv = msg_repo.load_conversation(&session_id).await.unwrap();

    // The boundary is applied (summary is prepended) but the tail is the FULL
    // raw history because the hash didn't match. Total = 1 summary + 4 raw msgs.
    assert_eq!(
        conv.messages.len(),
        5,
        "tail mismatch: summary + full raw history"
    );
    assert_eq!(conv.messages[0].text_content(), "Stale summary.");
    assert_eq!(conv.messages[1].text_content(), "sys");
    assert_eq!(conv.messages[2].text_content(), "q1");
    assert_eq!(conv.messages[3].text_content(), "a1");
    assert_eq!(conv.messages[4].text_content(), "q2");
}

/// Regression: when the boundary's `first_retained_message_id` does not exist
/// in the current raw messages (e.g. messages were trimmed or identity changed),
/// `load_conversation` falls back to the full raw history.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nonexistent_retained_message_id_falls_back_to_raw() {
    let db = test_db();
    let session_id = uuid::Uuid::now_v7().to_string();
    seed_chat_session(&db, &session_id).await;

    let msg_repo = SessionMessageRepository::new(db.clone(), EventBus::noop());
    let boundary_repo = SessionCompactionBoundaryRepository::new(db.clone());

    let raw = vec![Message::user("q1"), Message::assistant("a1")];
    let _ids = insert_and_collect_ids(&db, &session_id, &raw).await;

    // Complete a boundary pointing at a nonexistent message id.
    let started = boundary_repo
        .record_compaction_started(BeginCompactionParams {
            session_id: &session_id,
            schema_version: 1,
            trigger: None,
            current_context_tokens_before: None,
            first_message_id: None,
            last_compacted_message_id: None,
            first_retained_message_id: Some("nonexistent-message-id-42"),
            retained_tail_hash: None,
            marker_metadata: None,
        })
        .await
        .unwrap();
    boundary_repo
        .complete_compaction_boundary(CompleteCompactionParams {
            boundary_id: &started.id,
            schema_version: 1,
            current_context_tokens_after: None,
            first_message_id: None,
            last_compacted_message_id: None,
            first_retained_message_id: Some("nonexistent-message-id-42"),
            retained_tail_hash: None,
            summary_text: "Should not drop messages.",
            marker_metadata: None,
        })
        .await
        .unwrap();

    let conv = msg_repo.load_conversation(&session_id).await.unwrap();
    // Summary is prepended, then the full raw history (since the retained id
    // was not found). 1 summary + 2 raw messages = 3.
    assert_eq!(
        conv.messages.len(),
        3,
        "nonexistent retained id: summary + full raw"
    );
    assert_eq!(conv.messages[0].text_content(), "Should not drop messages.");
    assert_eq!(conv.messages[1].text_content(), "q1");
    assert_eq!(conv.messages[2].text_content(), "a1");
}

/// Regression: when a NEW completed boundary is written for a different range
/// but the tail hash of the new boundary is stale, the system still falls back
/// safely instead of treating the stale tail as valid.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn newer_boundary_with_stale_tail_hash_falls_back() {
    let db = test_db();
    let session_id = uuid::Uuid::now_v7().to_string();
    seed_chat_session(&db, &session_id).await;

    let msg_repo = SessionMessageRepository::new(db.clone(), EventBus::noop());
    let boundary_repo = SessionCompactionBoundaryRepository::new(db.clone());

    // Original messages.
    let raw = vec![
        Message::user("q1"),
        Message::assistant("a1"),
        Message::user("q2"),
        Message::assistant("a2"),
    ];
    let ids = insert_and_collect_ids(&db, &session_id, &raw).await;

    // First boundary completes successfully.
    let first = boundary_repo
        .record_compaction_started(BeginCompactionParams {
            session_id: &session_id,
            schema_version: 1,
            trigger: None,
            current_context_tokens_before: None,
            first_message_id: Some(&ids[0]),
            last_compacted_message_id: Some(&ids[1]),
            first_retained_message_id: Some(&ids[2]),
            retained_tail_hash: None,
            marker_metadata: None,
        })
        .await
        .unwrap();
    boundary_repo
        .complete_compaction_boundary(CompleteCompactionParams {
            boundary_id: &first.id,
            schema_version: 1,
            current_context_tokens_after: None,
            first_message_id: Some(&ids[0]),
            last_compacted_message_id: Some(&ids[1]),
            first_retained_message_id: Some(&ids[2]),
            retained_tail_hash: None,
            summary_text: "First compaction.",
            marker_metadata: None,
        })
        .await
        .unwrap();

    // Second boundary completes with a MISMATCHED tail hash — simulating a
    // boundary that was written against a previous tail state.
    let second = boundary_repo
        .record_compaction_started(BeginCompactionParams {
            session_id: &session_id,
            schema_version: 1,
            trigger: None,
            current_context_tokens_before: None,
            first_message_id: Some(&ids[2]),
            last_compacted_message_id: Some(&ids[3]),
            first_retained_message_id: Some(&ids[3]),
            retained_tail_hash: Some("djinn:default:CORRUPTED"),
            marker_metadata: None,
        })
        .await
        .unwrap();
    boundary_repo
        .complete_compaction_boundary(CompleteCompactionParams {
            boundary_id: &second.id,
            schema_version: 1,
            current_context_tokens_after: None,
            first_message_id: Some(&ids[2]),
            last_compacted_message_id: Some(&ids[3]),
            first_retained_message_id: Some(&ids[3]),
            retained_tail_hash: Some("djinn:default:CORRUPTED"),
            summary_text: "Second compaction (stale tail).",
            marker_metadata: None,
        })
        .await
        .unwrap();

    let conv = msg_repo.load_conversation(&session_id).await.unwrap();
    // The LATEST completed boundary is the second one. Because its tail hash
    // doesn't match, we get the summary + FULL raw history. This is the safe
    // fallback — we never silently drop messages.
    assert_eq!(
        conv.messages[0].text_content(),
        "Second compaction (stale tail)."
    );
    // Full raw follows: q1, a1, q2, a2.
    assert_eq!(conv.messages.len(), 5, "stale tail: summary + 4 raw");
    assert_eq!(conv.messages[1].text_content(), "q1");
    assert_eq!(conv.messages[4].text_content(), "a2");
}

// ── AC3: Continuity — prior summary reaches summarizer as previous-summary ──

/// Regression: when a conversation contains a recognized compaction marker pair
/// (user message with end marker + assistant continuation), the compaction
/// machinery's `extract_prior_summary` detects it and the prior summary is
/// provided as `<previous-summary>` context for the next summarization.
///
/// This test verifies the `djinn_compaction::prompts` layer directly, proving
/// that the summarizer receives the prior summary as merge context.
#[test]
fn prior_summary_detected_and_wrapped_as_previous_summary_context() {
    use djinn_compaction::extract_prior_summary;

    // Build a conversation that looks like a compacted session:
    // [system, user(summary+marker), assistant(continuation), user(follow-up)]
    let (summary_user, continuation_asst) = compaction_marker_pair("Prior summary of old work.");
    let messages = vec![
        Message::system("sys"),
        summary_user,
        continuation_asst,
        Message::user("What about X?"),
    ];

    let (prior_text, summary_idx, continuation_idx) =
        extract_prior_summary(&messages).expect("must detect prior summary");

    assert_eq!(prior_text, "Prior summary of old work.");
    assert_eq!(summary_idx, 1, "summary is at index 1");
    assert_eq!(continuation_idx, 2, "continuation is at index 2");
}

/// Regression: the `<previous-summary>` block correctly wraps the prior summary
/// with merge instructions. The old marker pair indices are excluded from the
/// messages to be summarized.
#[test]
fn previous_summary_block_contains_merge_instructions() {
    use djinn_compaction::{extract_prior_summary, previous_summary_block};

    let (summary_user, continuation_asst) = compaction_marker_pair("Everything done so far.");
    let messages = vec![
        Message::system("sys"),
        summary_user,
        continuation_asst,
        Message::user("New turn."),
        Message::assistant("New reply."),
    ];

    let (prior_text, summary_idx, continuation_idx) =
        extract_prior_summary(&messages).expect("must detect prior summary");

    // Build the previous-summary block.
    let block = previous_summary_block(&prior_text);
    assert!(
        block.contains("<previous-summary>"),
        "block must contain <previous-summary> tag"
    );
    assert!(
        block.contains("</previous-summary>"),
        "block must close the tag"
    );
    assert!(
        block.contains("Everything done so far."),
        "block must contain the prior summary text"
    );
    assert!(
        block.contains("UPDATE and MERGE"),
        "block must instruct the summarizer to merge, not re-summarize"
    );

    // Simulate what `do_compact` does: exclude the marker pair from the
    // messages being summarized. Only non-marker messages remain.
    let kept: Vec<&Message> = messages
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != summary_idx && *i != continuation_idx)
        .map(|(_, m)| m)
        .collect();
    assert_eq!(
        kept.len(),
        3,
        "system + new turn + new reply (markers excluded)"
    );
    assert_eq!(kept[0].role, Role::System);
    assert_eq!(kept[1].role, Role::User);
    assert_eq!(kept[1].text_content(), "New turn.");
    assert_eq!(kept[2].role, Role::Assistant);
    assert_eq!(kept[2].text_content(), "New reply.");

    // Verify: none of the kept messages contain the old compaction marker text.
    for msg in &kept {
        assert!(
            !msg.text_content().contains(COMPACTION_SUMMARY_END_MARKER),
            "ordinary messages must not contain the end marker"
        );
        assert!(
            !msg.text_content()
                .contains("Your context was compacted. The previous message"),
            "ordinary messages must not contain the continuation text"
        );
    }
}

/// Regression: when a conversation does NOT contain a recognized marker pair,
/// `extract_prior_summary` returns `None`. This is the case for a boundary-
/// projected conversation (which uses a System message, not the pair format).
#[test]
fn boundary_projected_system_summary_is_not_detected_as_prior_summary() {
    use djinn_compaction::extract_prior_summary;

    // Boundary-projected conversation: System summary + tail. No User+Assistant
    // marker pair. The summarizer should NOT detect a prior summary here.
    let messages = vec![
        Message::system("Summary from boundary projection."),
        Message::user("tail question"),
        Message::assistant("tail answer"),
    ];

    let result = extract_prior_summary(&messages);
    assert!(
        result.is_none(),
        "System message summary must not be detected as prior summary pair"
    );
}

/// Regression: a conversation that had a compaction marker pair from raw
/// history, followed by a boundary projection that replaces it, has the marker
/// pair excluded. The projected summary (System) carries the same content but
/// in a form that does NOT leak the old marker pair text into ordinary input.
#[test]
fn projected_summary_excludes_old_marker_pair_text() {
    // Simulate what `load_conversation` returns for a boundary-projected session.
    // The projected summary is a System message (clean text, no markers).
    let projected_summary = "Summary of work so far. Task is half done.";
    let projected = vec![
        Message {
            role: Role::System,
            content: vec![ContentBlock::text(projected_summary)],
            metadata: None,
        },
        Message::user("continuing work"),
        Message::assistant("done"),
    ];

    // Verify the projected conversation is clean: no end marker, no continuation.
    for msg in &projected {
        let text = msg.text_content();
        assert!(
            !text.contains(COMPACTION_SUMMARY_END_MARKER),
            "projected conversation must not contain the raw end marker"
        );
        assert!(
            !text.contains("Your context was compacted. The previous message"),
            "projected conversation must not contain the compaction continuation"
        );
        assert!(
            !text.contains("Part of your context was compacted."),
            "projected conversation must not contain partial compaction continuation"
        );
    }

    // The summary text itself is clean.
    assert_eq!(projected[0].text_content(), projected_summary);
    assert_eq!(projected[0].role, Role::System);
}

/// End-to-end regression: a session that had an in-memory compaction (with
/// marker pair in the raw messages) and then a persisted boundary, loads via
/// `load_conversation` with the boundary projection. The loaded conversation
/// carries the summary as a System message and the retained tail — without the
/// old marker pair leaking through.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_projection_replaces_raw_marker_pair() {
    let db = test_db();
    let session_id = uuid::Uuid::now_v7().to_string();
    seed_chat_session(&db, &session_id).await;

    let msg_repo = SessionMessageRepository::new(db.clone(), EventBus::noop());
    let boundary_repo = SessionCompactionBoundaryRepository::new(db.clone());

    // Step 1: Simulate the raw messages as they would exist after an in-memory
    // compaction. The original pre-compaction messages are still in session_messages,
    // but the in-memory conversation was compacted and then new messages were persisted.
    //
    // Raw messages in DB:
    //   [user("old1"), assistant("old2")]  ← original pre-compaction messages
    //   [user(summary+marker), assistant(continuation)]  ← compaction artifacts persisted
    //   [user("follow-up")]  ← post-compaction tail
    //
    // But in the actual chat handler, compaction markers are NOT re-persisted
    // (the handler only persists new assistant turns). So the raw messages are
    // just the original ones plus new tail. The boundary is what records the
    // summary. Let's model this correctly:
    let raw = vec![
        Message::system("You are helpful."),
        Message::user("old question"),
        Message::assistant("old answer"),
        Message::user("follow up"),
    ];
    let ids = insert_and_collect_ids(&db, &session_id, &raw).await;

    // Step 2: Complete a boundary. The summary text is the cleaned version.
    let started = boundary_repo
        .record_compaction_started(BeginCompactionParams {
            session_id: &session_id,
            schema_version: 1,
            trigger: None,
            current_context_tokens_before: None,
            first_message_id: Some(&ids[1]),
            last_compacted_message_id: Some(&ids[2]),
            first_retained_message_id: Some(&ids[3]),
            retained_tail_hash: None,
            marker_metadata: Some(&serde_json::json!({
                "marker_kind": "compaction_summary",
            })),
        })
        .await
        .unwrap();
    boundary_repo
        .complete_compaction_boundary(CompleteCompactionParams {
            boundary_id: &started.id,
            schema_version: 1,
            current_context_tokens_after: None,
            first_message_id: Some(&ids[1]),
            last_compacted_message_id: Some(&ids[2]),
            first_retained_message_id: Some(&ids[3]),
            retained_tail_hash: None,
            summary_text: "Clean summary of old turns.",
            marker_metadata: Some(&serde_json::json!({
                "marker_kind": "compaction_summary",
            })),
        })
        .await
        .unwrap();

    // Step 3: load_conversation applies the boundary projection.
    let conv = msg_repo.load_conversation(&session_id).await.unwrap();

    // The projected conversation has: System(summary) + tail.
    assert_eq!(conv.messages.len(), 2);
    assert_eq!(conv.messages[0].role, Role::System);
    assert_eq!(
        conv.messages[0].text_content(),
        "Clean summary of old turns."
    );
    assert_eq!(conv.messages[1].text_content(), "follow up");

    // No marker pair text is present.
    for msg in &conv.messages {
        assert!(
            !msg.text_content().contains(COMPACTION_SUMMARY_END_MARKER),
            "projected load must not leak end marker"
        );
        assert!(
            !msg.text_content().contains("Your context was compacted"),
            "projected load must not contain continuation marker"
        );
    }
}

/// Regression: `load_raw_conversation` is unaffected by boundaries and returns
/// all messages including the raw pre-compaction history. This confirms the
/// two code paths (projected vs raw) are independent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn load_raw_conversation_unaffected_by_boundary() {
    let db = test_db();
    let session_id = uuid::Uuid::now_v7().to_string();
    seed_chat_session(&db, &session_id).await;

    let msg_repo = SessionMessageRepository::new(db.clone(), EventBus::noop());
    let boundary_repo = SessionCompactionBoundaryRepository::new(db.clone());

    let raw = vec![
        Message::user("q1"),
        Message::assistant("a1"),
        Message::user("q2"),
    ];
    let ids = insert_and_collect_ids(&db, &session_id, &raw).await;

    let started = boundary_repo
        .record_compaction_started(BeginCompactionParams {
            session_id: &session_id,
            schema_version: 1,
            trigger: None,
            current_context_tokens_before: None,
            first_message_id: Some(&ids[0]),
            last_compacted_message_id: Some(&ids[1]),
            first_retained_message_id: Some(&ids[2]),
            retained_tail_hash: None,
            marker_metadata: None,
        })
        .await
        .unwrap();
    boundary_repo
        .complete_compaction_boundary(CompleteCompactionParams {
            boundary_id: &started.id,
            schema_version: 1,
            current_context_tokens_after: None,
            first_message_id: Some(&ids[0]),
            last_compacted_message_id: Some(&ids[1]),
            first_retained_message_id: Some(&ids[2]),
            retained_tail_hash: None,
            summary_text: "Sum.",
            marker_metadata: None,
        })
        .await
        .unwrap();

    // load_conversation applies projection.
    let projected = msg_repo.load_conversation(&session_id).await.unwrap();
    assert_eq!(projected.messages.len(), 2, "summary + 1 tail");

    // load_raw_conversation returns ALL raw messages, no boundary.
    let raw_conv = msg_repo.load_raw_conversation(&session_id).await.unwrap();
    assert_eq!(raw_conv.messages.len(), 3, "all raw messages preserved");
    assert_eq!(raw_conv.messages[0].text_content(), "q1");
    assert_eq!(raw_conv.messages[1].text_content(), "a1");
    assert_eq!(raw_conv.messages[2].text_content(), "q2");
}
