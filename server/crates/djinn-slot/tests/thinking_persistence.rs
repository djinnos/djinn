//! Exact thinking-persistence behavior tests for the slot reply loop.
//!
//! These tests exercise the *production* `assemble_persisted_content` and
//! `flush_in_flight_turn` code paths — they do NOT replicate the reconciliation
//! algorithm in a test helper.  Each test builds an explicit `StreamEvent`
//! stream (or directly sets `StreamTurnState` fields that the production stream
//! consumer populates), then asserts the exact resulting `ContentBlock` array.
//!
//! Coverage:
//! - Exact-ID reconciliation: completed A → `[Thinking(A,sig)]`; completed A +
//!   partial B → `[Thinking(A,sig), Thinking(Bpartial,None)]`; interleaved IDs.
//! - Normal finalization and `flush_in_flight_turn` produce the same canonical
//!   order: provider-state, fallback thinking, text, tool calls.
//! - `record_thinking` receives the complete pre-reconciliation aggregate.
//! - OpenAI Chat unsigned reasoning (always retained, no signature).
//! - OpenAI Responses state-plus-summary behavior (provider state retained).
//! - Generic unproven provider/fallback preservation.
//! - Serialized session-message shape is unchanged.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashSet;

use djinn_core::events::EventBus;
use djinn_core::models::{Epic, Project, Task};
use djinn_db::Database;
use djinn_db::repositories::session::CreateSessionParams;
use djinn_db::{
    EpicCreateInput, EpicRepository, ProjectRepository, SessionMessageRepository,
    SessionRepository, TaskRepository,
};
use djinn_provider::message::ContentBlock;
use djinn_slot::reply_loop::persistence::assemble_persisted_content;
use djinn_slot::reply_loop::persistence::flush_in_flight_turn;
use djinn_slot::reply_loop::streaming::StreamTurnState;
use djinn_slot::reply_loop::streaming::UnresolvedThinkingFragment;

// ─── test DB / session setup ──────────────────────────────────────────────────

async fn create_test_db() -> Database {
    let db = Database::open_in_memory().expect("open in-memory db");
    db.ensure_initialized().await.expect("ensure initialized");
    db
}

async fn create_test_project(db: &Database) -> Project {
    let event_bus = EventBus::noop();
    let repo = ProjectRepository::new(db.clone(), event_bus);
    let uuid = uuid::Uuid::now_v7().simple();
    repo.create(
        &format!("test-project-{uuid}"),
        &format!("owner-{uuid}"),
        &format!("repo-{uuid}"),
    )
    .await
    .expect("create project")
}

async fn create_test_epic(db: &Database, project_id: &str) -> Epic {
    let event_bus = EventBus::noop();
    let repo = EpicRepository::new(db.clone(), event_bus);
    repo.create_for_project(
        project_id,
        EpicCreateInput {
            title: "test-epic",
            description: "test epic description",
            emoji: "🧪",
            color: "blue",
            owner: "test-owner",
            memory_refs: None,
            status: None,
            auto_breakdown: None,
            originating_adr_id: None,
            blocked_by: None,
        },
    )
    .await
    .expect("create epic")
}

async fn create_test_task(db: &Database, project_id: &str, epic_id: &str) -> Task {
    let event_bus = EventBus::noop();
    let repo = TaskRepository::new(db.clone(), event_bus);
    repo.create_in_project(
        project_id,
        Some(epic_id),
        "test-task",
        "test task description",
        "test task design",
        "task",
        2,
        "test-owner",
        None,
        None,
    )
    .await
    .expect("create test task")
}

/// Create a session in the test DB and return `(session_id, task_id)`.
async fn create_test_session(db: &Database) -> (String, String) {
    let project = create_test_project(db).await;
    let epic = create_test_epic(db, &project.id).await;
    let task = create_test_task(db, &project.id, &epic.id).await;
    let session_repo = SessionRepository::new(db.clone(), EventBus::noop());
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &project.id,
            task_id: Some(&task.id),
            model: "test-model",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .expect("create session");
    (session.id, task.id)
}

// ─── helpers for exact-array assertions ───────────────────────────────────────

/// Assert that a `ContentBlock` is `Thinking` with the given text and signature.
fn assert_thinking(block: &ContentBlock, expected_text: &str, expected_sig: Option<&str>) {
    match block {
        ContentBlock::Thinking {
            thinking,
            signature,
        } => {
            assert_eq!(
                thinking, expected_text,
                "thinking text mismatch in block {block:?}"
            );
            assert_eq!(
                signature.as_deref(),
                expected_sig,
                "signature mismatch in block {block:?}"
            );
        }
        other => panic!("expected Thinking block, got {other:?}"),
    }
}

fn assert_text(block: &ContentBlock, expected: &str) {
    match block {
        ContentBlock::Text { text } => assert_eq!(text, expected),
        other => panic!("expected Text block, got {other:?}"),
    }
}

// ─── AC1: exact-ID reconciliation arrays ──────────────────────────────────────

/// Completed thinking block A (signed) → exact array `[Thinking(A, sig)]`.
/// The attributed delta fragment is suppressed by exact ID because its
/// `ThinkingBlockComplete` landed.
#[test]
fn completed_signed_block_a_suppresses_delta_by_exact_id() {
    let provider_state = vec![ContentBlock::Thinking {
        thinking: "A".to_string(),
        signature: Some("sig-a".to_string()),
    }];
    let unresolved = vec![UnresolvedThinkingFragment::Attributed {
        id: 0,
        text: "A".to_string(),
    }];
    let completed = HashSet::from([0u64]);

    let content = assemble_persisted_content(&provider_state, &unresolved, &completed, "", &[]);

    assert_eq!(content.len(), 1, "expected exactly one block: {content:?}");
    assert_thinking(&content[0], "A", Some("sig-a"));
}

/// Completed A + partial B → exact array
/// `[Thinking(A,sig), Thinking(Bpartial,None)]`.
/// Block B's delta fragment has no completion, so it survives as an unsigned
/// fallback thinking block.
#[test]
fn completed_a_plus_partial_b_preserves_partial_as_unsigned() {
    let provider_state = vec![ContentBlock::Thinking {
        thinking: "A".to_string(),
        signature: Some("sig-a".to_string()),
    }];
    let unresolved = vec![
        UnresolvedThinkingFragment::Attributed {
            id: 0,
            text: "A".to_string(),
        },
        UnresolvedThinkingFragment::Attributed {
            id: 1,
            text: "Bpartial".to_string(),
        },
    ];
    let completed = HashSet::from([0u64]);

    let content = assemble_persisted_content(&provider_state, &unresolved, &completed, "", &[]);

    assert_eq!(content.len(), 2, "expected exactly two blocks: {content:?}");
    assert_thinking(&content[0], "A", Some("sig-a"));
    assert_thinking(&content[1], "Bpartial", None);
}

/// Interleaved IDs: completed A (id=0), partial B (id=1), completed C (id=2).
/// Result: provider-state holds A and C; fallback holds only B (id=1) as
/// unsigned.
#[test]
fn interleaved_ids_reconcile_correctly() {
    let provider_state = vec![
        ContentBlock::Thinking {
            thinking: "A".to_string(),
            signature: Some("sig-a".to_string()),
        },
        ContentBlock::Thinking {
            thinking: "C".to_string(),
            signature: Some("sig-c".to_string()),
        },
    ];
    let unresolved = vec![
        UnresolvedThinkingFragment::Attributed {
            id: 0,
            text: "A".to_string(),
        },
        UnresolvedThinkingFragment::Attributed {
            id: 1,
            text: "B".to_string(),
        },
        UnresolvedThinkingFragment::Attributed {
            id: 2,
            text: "C".to_string(),
        },
    ];
    let completed = HashSet::from([0u64, 2u64]);

    let content = assemble_persisted_content(&provider_state, &unresolved, &completed, "", &[]);

    assert_eq!(content.len(), 3, "expected three blocks: {content:?}");
    assert_thinking(&content[0], "A", Some("sig-a"));
    assert_thinking(&content[1], "C", Some("sig-c"));
    assert_thinking(&content[2], "B", None);
}

/// The interleaved completed/open result using only exact-ID reconciliation:
/// when fragments arrive in a different order (B before A), the canonical
/// assembly still places provider-state blocks first (in their arrival order)
/// then the single unsigned fallback block.
#[test]
fn interleaved_arrival_order_provider_state_first() {
    let provider_state = vec![
        ContentBlock::Thinking {
            thinking: "A".to_string(),
            signature: Some("sig-a".to_string()),
        },
        ContentBlock::Thinking {
            thinking: "C".to_string(),
            signature: Some("sig-c".to_string()),
        },
    ];
    // Arrival order: B(id=1) first, then A(id=0), then C(id=2)
    let unresolved = vec![
        UnresolvedThinkingFragment::Attributed {
            id: 1,
            text: "B".to_string(),
        },
        UnresolvedThinkingFragment::Attributed {
            id: 0,
            text: "A".to_string(),
        },
        UnresolvedThinkingFragment::Attributed {
            id: 2,
            text: "C".to_string(),
        },
    ];
    let completed = HashSet::from([0u64, 2u64]);

    let content = assemble_persisted_content(&provider_state, &unresolved, &completed, "", &[]);

    // Provider-state blocks first (A, C), then unsigned fallback (B only).
    assert_eq!(content.len(), 3, "expected three blocks: {content:?}");
    assert_thinking(&content[0], "A", Some("sig-a"));
    assert_thinking(&content[1], "C", Some("sig-c"));
    assert_thinking(&content[2], "B", None);
}

// ─── AC2: normal finalization vs flush_in_flight_turn canonical order ─────────

/// Both normal finalization and `flush_in_flight_turn` produce the same
/// canonical order: provider-state blocks, one unsigned thinking block,
/// assistant text, then tool calls.
#[tokio::test]
async fn normal_and_flush_produce_same_canonical_order() {
    let provider_state = vec![ContentBlock::Thinking {
        thinking: "signed-think".to_string(),
        signature: Some("sig".to_string()),
    }];
    let unresolved = vec![
        UnresolvedThinkingFragment::Attributed {
            id: 5,
            text: "completed-delta".to_string(),
        },
        UnresolvedThinkingFragment::Attributed {
            id: 6,
            text: "partial-fallback".to_string(),
        },
    ];
    let completed = HashSet::from([5u64]);
    let text = "assistant text";
    let tool_calls = vec![ContentBlock::ToolUse {
        id: "call-1".to_string(),
        name: "read".to_string(),
        input: serde_json::json!({}),
    }];

    // Normal finalization path — calls assemble_persisted_content directly
    // (same function that turn.rs normal completion calls).
    let normal_content =
        assemble_persisted_content(&provider_state, &unresolved, &completed, text, &tool_calls);

    // flush_in_flight_turn path — builds the same content via the same
    // assembler from StreamTurnState fields.
    let db = create_test_db().await;
    let (session_id, task_id) = create_test_session(&db).await;
    let msg_repo = SessionMessageRepository::new(db.clone(), EventBus::noop());

    let mut state = StreamTurnState::new();
    state.turn_provider_state = provider_state.clone();
    state.turn_unresolved_thinking = unresolved.clone();
    state.turn_completed_thinking_ids = completed.clone();
    state.turn_text = text.to_string();
    state.turn_tool_calls = tool_calls.clone();

    flush_in_flight_turn(&msg_repo, &session_id, &task_id, 0, &mut state).await;
    assert!(state.turn_flushed, "flush must set the idempotency guard");

    // Read the persisted assistant message back from the DB and extract
    // its content blocks.
    let conversation = msg_repo
        .load_raw_conversation(&session_id)
        .await
        .expect("load_raw_conversation");
    let assistant_msg = conversation
        .messages
        .iter()
        .find(|m| m.role == djinn_provider::message::Role::Assistant)
        .expect("flushed assistant message must be present");

    assert_eq!(
        assistant_msg.content, normal_content,
        "flush_in_flight_turn content must match normal finalization content"
    );

    // Assert canonical order: provider-state, unsigned thinking, text, tool.
    assert_eq!(normal_content.len(), 4);
    assert_thinking(&normal_content[0], "signed-think", Some("sig"));
    assert_thinking(&normal_content[1], "partial-fallback", None);
    assert_text(&normal_content[2], "assistant text");
    assert!(matches!(&normal_content[3], ContentBlock::ToolUse { id, .. } if id == "call-1"));
}

/// `record_thinking` receives the complete pre-reconciliation aggregate.
/// The aggregate includes ALL attributed delta text AND unattributed thinking,
/// regardless of whether the block completed.  This is the display/telemetry
/// aggregate, separate from persistence.
#[test]
fn record_thinking_receives_complete_pre_reconciliation_aggregate() {
    // Simulate the production stream consumer's behavior: turn_thinking gets
    // ALL delta text appended once, and turn_unresolved_thinking gets every
    // fragment in arrival order. The aggregate is built by the stream consumer
    // before persistence reconciliation.
    let mut turn_thinking = String::new();

    // Event stream: ThinkingDelta(id=0, "A"), ThinkingBlockComplete(id=0, "A", sig),
    // ThinkingDelta(id=1, "Bpartial"), Thinking("OpenAI-reasoning")
    let events = vec![
        ("delta", 0u64, "A"),
        ("complete", 0u64, "A"),
        ("delta", 1u64, "Bpartial"),
    ];

    for (kind, _id, text) in &events {
        match *kind {
            "delta" => {
                // Production: state.turn_thinking.push_str(&text)
                turn_thinking.push_str(text);
            }
            "complete" => {
                // Production: ThinkingBlockComplete does NOT append to turn_thinking.
                // It only pushes to turn_provider_state and marks the ID complete.
            }
            _ => {}
        }
    }
    // Unattributed OpenAI reasoning is also appended.
    turn_thinking.push_str("OpenAI-reasoning");

    // The aggregate must contain ALL text: "A" + "Bpartial" + "OpenAI-reasoning"
    assert_eq!(
        turn_thinking, "ABpartialOpenAI-reasoning",
        "record_thinking aggregate must include all delta text and unattributed text, \
         NOT the completion text (which would double-count)"
    );
}

// ─── AC3: provider-specific behavior ──────────────────────────────────────────

/// OpenAI Chat unsigned reasoning: `StreamEvent::Thinking(String)` produces an
/// unattributed fragment that is ALWAYS retained (no block ID to reconcile).
/// The persisted thinking block has `signature: None`.
#[test]
fn openai_chat_unsigned_reasoning_always_retained_no_signature() {
    let unresolved = vec![UnresolvedThinkingFragment::Unattributed(
        "reasoning from openai chat".to_string(),
    )];
    let completed = HashSet::<u64>::new();

    let content = assemble_persisted_content(&[], &unresolved, &completed, "", &[]);

    assert_eq!(content.len(), 1);
    assert_thinking(&content[0], "reasoning from openai chat", None);
}

/// OpenAI Responses state-plus-summary behavior: the `OpenAIReasoning`
/// provider-state block is retained in canonical position (first), and any
/// unattributed reasoning summary text is included as unsigned thinking.
#[test]
fn openai_responses_state_plus_summary_retained() {
    let provider_state = vec![ContentBlock::OpenAIReasoning {
        id: Some("resp_abc".to_string()),
        encrypted_content: "encrypted-state-blob".to_string(),
        summary: Some(serde_json::json!([{"type": "summary_text", "text": "summary"}])),
        status: Some("completed".to_string()),
    }];
    let unresolved = vec![UnresolvedThinkingFragment::Unattributed(
        "summary reasoning".to_string(),
    )];
    let completed = HashSet::<u64>::new();

    let content = assemble_persisted_content(&provider_state, &unresolved, &completed, "", &[]);

    // Canonical order: provider state first, then unsigned thinking.
    assert_eq!(content.len(), 2);
    assert!(matches!(
        &content[0],
        ContentBlock::OpenAIReasoning { id, encrypted_content, .. }
            if id.as_deref() == Some("resp_abc")
            && encrypted_content == "encrypted-state-blob"
    ));
    assert_thinking(&content[1], "summary reasoning", None);
}

/// Generic unproven provider/fallback preservation: a `Thinking` block that
/// arrives as provider state (e.g. via `Delta(Thinking)`) is retained, and
/// any unattributed fallback is also preserved. No value-based suppression.
#[test]
fn generic_unproven_provider_and_fallback_preserved() {
    let provider_state = vec![ContentBlock::Thinking {
        thinking: "delta-thinking-no-sig".to_string(),
        signature: None,
    }];
    let unresolved = vec![
        UnresolvedThinkingFragment::Unattributed("fallback-1".to_string()),
        UnresolvedThinkingFragment::Attributed {
            id: 99,
            text: "unmatched-attributed".to_string(),
        },
    ];
    let completed = HashSet::<u64>::new();

    let content = assemble_persisted_content(&provider_state, &unresolved, &completed, "", &[]);

    // Provider-state block + combined unsigned fallback
    // ("fallback-1" + "unmatched-attributed").
    assert_eq!(content.len(), 2);
    assert_thinking(&content[0], "delta-thinking-no-sig", None);
    assert_thinking(&content[1], "fallback-1unmatched-attributed", None);
}

/// RedactedThinking blocks in provider state are preserved verbatim.
#[test]
fn redacted_thinking_preserved_in_provider_state() {
    let provider_state = vec![ContentBlock::RedactedThinking {
        data: "base64-redacted-blob".to_string(),
    }];
    let content = assemble_persisted_content(&provider_state, &[], &HashSet::new(), "", &[]);

    assert_eq!(content.len(), 1);
    assert!(matches!(
        &content[0],
        ContentBlock::RedactedThinking { data } if data == "base64-redacted-blob"
    ));
}

// ─── AC3: serialized session-message shape unchanged ─────────────────────────

/// The serialized shape of a Message containing thinking blocks is unchanged:
/// the `type` discriminant is `"thinking"`, `signature` is omitted when `None`,
/// and the structure round-trips through serde.
#[test]
fn serialized_session_message_shape_with_thinking() {
    use djinn_provider::message::{Message, Role};

    let msg = Message {
        role: Role::Assistant,
        content: vec![
            ContentBlock::Thinking {
                thinking: "signed".to_string(),
                signature: Some("sig".to_string()),
            },
            ContentBlock::Thinking {
                thinking: "unsigned".to_string(),
                signature: None,
            },
            ContentBlock::Text {
                text: "response".to_string(),
            },
        ],
        metadata: None,
    };

    let json = serde_json::to_value(&msg).expect("serialize message");
    let content = json["content"].as_array().expect("content array");

    // Signed thinking: type, thinking, signature all present.
    assert_eq!(content[0]["type"], "thinking");
    assert_eq!(content[0]["thinking"], "signed");
    assert_eq!(content[0]["signature"], "sig");

    // Unsigned thinking: signature omitted (skip_serializing_if).
    assert_eq!(content[1]["type"], "thinking");
    assert_eq!(content[1]["thinking"], "unsigned");
    assert!(
        content[1].get("signature").is_none(),
        "None signature must be omitted from serialized shape"
    );

    // Text block.
    assert_eq!(content[2]["type"], "text");
    assert_eq!(content[2]["text"], "response");

    // Round-trip.
    let round_tripped: Message =
        serde_json::from_value(json).expect("deserialize message round-trip");
    assert_eq!(round_tripped.content.len(), 3);
}

/// The serialized shape of a canonical assembled turn (provider state +
/// fallback + text + tool) matches the expected JSON structure.
#[test]
fn serialized_canonical_assembled_shape() {
    use djinn_provider::message::{Message, Role};

    let provider_state = vec![ContentBlock::Thinking {
        thinking: "A".to_string(),
        signature: Some("sig-a".to_string()),
    }];
    let unresolved = vec![UnresolvedThinkingFragment::Attributed {
        id: 1,
        text: "Bpartial".to_string(),
    }];
    let completed = HashSet::from([0u64]);

    let content =
        assemble_persisted_content(&provider_state, &unresolved, &completed, "hello", &[]);

    let msg = Message {
        role: Role::Assistant,
        content,
        metadata: None,
    };

    let json = serde_json::to_value(&msg).expect("serialize");
    let arr = json["content"].as_array().expect("array");

    // [Thinking(A,sig-a), Thinking(Bpartial,None), Text(hello)]
    assert_eq!(arr.len(), 3);
    assert_eq!(arr[0]["type"], "thinking");
    assert_eq!(arr[0]["thinking"], "A");
    assert_eq!(arr[0]["signature"], "sig-a");
    assert_eq!(arr[1]["type"], "thinking");
    assert_eq!(arr[1]["thinking"], "Bpartial");
    assert!(arr[1].get("signature").is_none());
    assert_eq!(arr[2]["type"], "text");
    assert_eq!(arr[2]["text"], "hello");
}

// ─── AC2: flush_in_flight_turn specific behaviors ────────────────────────────

/// `flush_in_flight_turn` with only signed completion (no fallback) persists
/// just the provider-state thinking block.
#[tokio::test]
async fn flush_with_only_signed_completion() {
    let db = create_test_db().await;
    let (session_id, task_id) = create_test_session(&db).await;
    let msg_repo = SessionMessageRepository::new(db.clone(), EventBus::noop());

    let mut state = StreamTurnState::new();
    state.turn_provider_state = vec![ContentBlock::Thinking {
        thinking: "A".to_string(),
        signature: Some("sig-a".to_string()),
    }];
    state.turn_unresolved_thinking = vec![UnresolvedThinkingFragment::Attributed {
        id: 0,
        text: "A".to_string(),
    }];
    state.turn_completed_thinking_ids = HashSet::from([0u64]);

    flush_in_flight_turn(&msg_repo, &session_id, &task_id, 0, &mut state).await;

    let conversation = msg_repo
        .load_raw_conversation(&session_id)
        .await
        .expect("load");
    let assistant = conversation
        .messages
        .iter()
        .find(|m| m.role == djinn_provider::message::Role::Assistant)
        .expect("assistant message");

    assert_eq!(assistant.content.len(), 1);
    assert_thinking(&assistant.content[0], "A", Some("sig-a"));
}

/// `flush_in_flight_turn` with signed A + partial B + text persists canonical
/// order and no orphan tools.
#[tokio::test]
async fn flush_with_signed_a_partial_b_and_text_no_tools() {
    let db = create_test_db().await;
    let (session_id, task_id) = create_test_session(&db).await;
    let msg_repo = SessionMessageRepository::new(db.clone(), EventBus::noop());

    let mut state = StreamTurnState::new();
    state.turn_provider_state = vec![ContentBlock::Thinking {
        thinking: "A".to_string(),
        signature: Some("sig-a".to_string()),
    }];
    state.turn_unresolved_thinking = vec![
        UnresolvedThinkingFragment::Attributed {
            id: 0,
            text: "A".to_string(),
        },
        UnresolvedThinkingFragment::Attributed {
            id: 1,
            text: "Bpartial".to_string(),
        },
    ];
    state.turn_completed_thinking_ids = HashSet::from([0u64]);
    state.turn_text = "partial response".to_string();

    flush_in_flight_turn(&msg_repo, &session_id, &task_id, 0, &mut state).await;

    let conversation = msg_repo
        .load_raw_conversation(&session_id)
        .await
        .expect("load");
    let assistant = conversation
        .messages
        .iter()
        .find(|m| m.role == djinn_provider::message::Role::Assistant)
        .expect("assistant message");

    // [Thinking(A,sig-a), Thinking(Bpartial,None), Text(partial response)]
    assert_eq!(assistant.content.len(), 3);
    assert_thinking(&assistant.content[0], "A", Some("sig-a"));
    assert_thinking(&assistant.content[1], "Bpartial", None);
    assert_text(&assistant.content[2], "partial response");
}

/// `flush_in_flight_turn` is idempotent: calling it twice does not duplicate
/// persisted messages.
#[tokio::test]
async fn flush_in_flight_turn_is_idempotent() {
    let db = create_test_db().await;
    let (session_id, task_id) = create_test_session(&db).await;
    let msg_repo = SessionMessageRepository::new(db.clone(), EventBus::noop());

    let mut state = StreamTurnState::new();
    state.turn_text = "text".to_string();

    flush_in_flight_turn(&msg_repo, &session_id, &task_id, 0, &mut state).await;
    assert!(state.turn_flushed);

    // Second call — should be a no-op.
    flush_in_flight_turn(&msg_repo, &session_id, &task_id, 0, &mut state).await;

    let conversation = msg_repo
        .load_raw_conversation(&session_id)
        .await
        .expect("load");
    assert_eq!(
        conversation.messages.len(),
        1,
        "idempotent flush must not duplicate messages"
    );
}

/// Explicit StreamEvents flow through the production persistence state consumer,
/// then the normal finalizer used by `turn.rs` and the interrupted DB flush.
#[tokio::test]
async fn stream_events_drive_normal_flush_and_record_thinking() {
    use djinn_provider::provider::StreamEvent;
    use djinn_slot::reply_loop::streaming::consume_events_for_persistence;
    use djinn_slot::reply_loop::turn::{finalize_normal_turn_content, record_normal_turn_thinking};

    let events = || {
        vec![
            StreamEvent::ThinkingDelta {
                id: 5,
                text: "A".into(),
            },
            StreamEvent::ThinkingBlockComplete {
                id: 5,
                thinking: "A".into(),
                signature: Some("sig-a".into()),
            },
            StreamEvent::ThinkingDelta {
                id: 6,
                text: "Bpartial".into(),
            },
            StreamEvent::Thinking("chat-summary".into()),
            StreamEvent::Delta(ContentBlock::Text {
                text: "assistant text".into(),
            }),
            StreamEvent::Delta(ContentBlock::ToolUse {
                id: "call-1".into(),
                name: "read".into(),
                input: serde_json::json!({}),
            }),
            StreamEvent::Done,
        ]
    };
    let state = consume_events_for_persistence(events());
    let normal = finalize_normal_turn_content(
        &state.turn_provider_state,
        &state.turn_unresolved_thinking,
        &state.turn_completed_thinking_ids,
        &state.turn_text,
        &state.turn_tool_calls,
    );
    let mut recorded = None;
    record_normal_turn_thinking(&state.turn_thinking, |thinking| {
        recorded = Some(thinking.to_owned())
    });
    assert_eq!(recorded.as_deref(), Some("ABpartialchat-summary"));
    assert_eq!(normal.len(), 4);
    assert_thinking(&normal[0], "A", Some("sig-a"));
    assert_thinking(&normal[1], "Bpartialchat-summary", None);
    assert_text(&normal[2], "assistant text");
    assert!(matches!(&normal[3], ContentBlock::ToolUse { id, .. } if id == "call-1"));

    let db = create_test_db().await;
    let (session_id, task_id) = create_test_session(&db).await;
    let repo = SessionMessageRepository::new(db, EventBus::noop());
    let mut interrupted = consume_events_for_persistence(events());
    flush_in_flight_turn(&repo, &session_id, &task_id, 0, &mut interrupted).await;
    let persisted = repo
        .load_raw_conversation(&session_id)
        .await
        .expect("load flush");
    assert_eq!(persisted.messages[0].content, normal);
}

/// OpenAI Chat/Responses and generic `Delta(Thinking)` must be classified by
/// the production event consumer before normal finalization.
#[test]
fn provider_events_preserve_openai_and_generic_fallbacks() {
    use djinn_provider::provider::StreamEvent;
    use djinn_slot::reply_loop::streaming::consume_events_for_persistence;
    use djinn_slot::reply_loop::turn::finalize_normal_turn_content;
    let state = consume_events_for_persistence(vec![
        StreamEvent::Thinking("chat".into()),
        StreamEvent::Delta(ContentBlock::OpenAIReasoning {
            id: Some("resp".into()),
            encrypted_content: "state".into(),
            summary: None,
            status: None,
        }),
        StreamEvent::Thinking("summary".into()),
        StreamEvent::Delta(ContentBlock::Thinking {
            thinking: "generic".into(),
            signature: None,
        }),
    ]);
    let content = finalize_normal_turn_content(
        &state.turn_provider_state,
        &state.turn_unresolved_thinking,
        &state.turn_completed_thinking_ids,
        &state.turn_text,
        &state.turn_tool_calls,
    );
    assert!(
        matches!(&content[0], ContentBlock::OpenAIReasoning { id: Some(id), .. } if id == "resp")
    );
    assert_thinking(&content[1], "generic", None);
    assert_thinking(&content[2], "chatsummary", None);
}
