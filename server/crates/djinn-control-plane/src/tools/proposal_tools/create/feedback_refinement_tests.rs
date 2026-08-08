//! Canonical persisted feedback-refinement generations projected by proposal_show.

use crate::server::DjinnMcpServer;
use crate::state::stubs::test_mcp_state;
use djinn_core::events::EventBus;
use djinn_db::test_support::queue_feedback_refinement_generation_for_test;
use djinn_db::{
    Database, FeedbackRefinementDisposition, FeedbackRefinementDispositionInput,
    ProposalCreateInput, ProposalDebateTrailCreateInput, ProposalFeedbackCreateInput,
    ProposalRepository, ProposalUpdateInput,
};
use serde_json::{Value, json};

async fn test_server() -> (DjinnMcpServer, Database) {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    (DjinnMcpServer::new(test_mcp_state(db.clone())), db)
}

/// Make generated repository timestamps stable while retaining an exact JSON
/// assertion for every non-time lifecycle field.
fn normalize_timestamps(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                if matches!(key.as_str(), "created_at" | "accepted_at" | "withdrawn_at") {
                    *child = Value::String("<timestamp>".into());
                } else {
                    normalize_timestamps(child);
                }
            }
        }
        Value::Array(values) => values.iter_mut().for_each(normalize_timestamps),
        _ => {}
    }
}

/// `proposal_show` is a thin projection of durable injection/source/debate
/// records. This exercises queued, materialized, both dispositions, withdrawal,
/// mixed severity ordered rows, and later generations of the same root.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proposal_show_projects_persisted_feedback_refinement_generations() {
    let (server, db) = test_server().await;
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal = repo
        .create(ProposalCreateInput {
            title: "Feedback lifecycle projection",
            body: "body",
            acceptance_criteria: Some("[]"),
            status: None,
            body_format: None,
        })
        .await
        .unwrap();

    let author = "feedback-author".to_owned();
    let root = djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(author.clone()), async {
            repo.add_feedback_with_severity(
                ProposalFeedbackCreateInput {
                    proposal_id: &proposal.id,
                    parent_id: None,
                    author_kind: "user",
                    author_model: None,
                    body: "root blocking feedback",
                },
                "blocking",
            )
            .await
            .unwrap()
        })
        .await;
    let advisory = djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(author.clone()), async {
            repo.add_feedback_with_severity(
                ProposalFeedbackCreateInput {
                    proposal_id: &proposal.id,
                    parent_id: Some(&root.id),
                    author_kind: "user",
                    author_model: None,
                    body: "advisory context",
                },
                "advisory",
            )
            .await
            .unwrap()
        })
        .await;

    let first = repo
        .capture_feedback_refinement_boundary(&proposal.id)
        .await
        .unwrap()
        .captures
        .pop()
        .unwrap();
    repo.update(
        &proposal.id,
        ProposalUpdateInput {
            title: &proposal.title,
            body: "body revised to address feedback",
            acceptance_criteria: &proposal.acceptance_criteria,
            status: &proposal.status,
            superseded_by: proposal.superseded_by.as_deref(),
            body_format: Some(&proposal.body_format),
            event_metadata: None,
        },
    )
    .await
    .unwrap();
    djinn_core::auth_context::SESSION_USER_ID
        .scope(Some("judge-fixed".into()), async {
            repo.dispose_feedback_refinement_generation(FeedbackRefinementDispositionInput {
                proposal_id: proposal.id.clone(),
                injection_id: first.injection.id.clone(),
                root_feedback_id: first.injection.root_feedback_id.clone(),
                generation: first.injection.generation,
                debate_entry_id: first.debate_entry.id.clone(),
                disposition: FeedbackRefinementDisposition::FixedRevision { revision_seq: 2 },
            })
            .await
            .unwrap();
        })
        .await;

    let second_reply = repo
        .add_feedback(ProposalFeedbackCreateInput {
            proposal_id: &proposal.id,
            parent_id: Some(&root.id),
            author_kind: "agent",
            author_model: Some("review-model"),
            body: "second generation blocking feedback",
        })
        .await
        .unwrap();
    let second = repo
        .capture_feedback_refinement_boundary(&proposal.id)
        .await
        .unwrap()
        .captures
        .pop()
        .unwrap();
    djinn_core::auth_context::SESSION_USER_ID
        .scope(Some("judge-wont-fix".into()), async {
            repo.dispose_feedback_refinement_generation(FeedbackRefinementDispositionInput {
                proposal_id: proposal.id.clone(),
                injection_id: second.injection.id.clone(),
                root_feedback_id: second.injection.root_feedback_id.clone(),
                generation: second.injection.generation,
                debate_entry_id: second.debate_entry.id.clone(),
                disposition: FeedbackRefinementDisposition::WontFix {
                    reason: "outside the agreed scope".into(),
                },
            })
            .await
            .unwrap();
        })
        .await;

    let third_reply = repo
        .add_feedback(ProposalFeedbackCreateInput {
            proposal_id: &proposal.id,
            parent_id: Some(&root.id),
            author_kind: "agent",
            author_model: Some("review-model"),
            body: "withdrawn generation feedback",
        })
        .await
        .unwrap();
    let third = repo
        .capture_feedback_refinement_boundary(&proposal.id)
        .await
        .unwrap()
        .captures
        .pop()
        .unwrap();
    repo.withdraw_feedback_with_refinement_derivation(&third_reply.id, "feedback-author")
        .await
        .unwrap();

    let queued_reply = repo
        .add_feedback(ProposalFeedbackCreateInput {
            proposal_id: &proposal.id,
            parent_id: Some(&root.id),
            author_kind: "agent",
            author_model: Some("review-model"),
            body: "queued generation feedback",
        })
        .await
        .unwrap();
    let queued = repo
        .capture_feedback_refinement_boundary(&proposal.id)
        .await
        .unwrap()
        .captures
        .pop()
        .unwrap();
    queue_feedback_refinement_generation_for_test(&db, &queued.injection.id).await;

    let injected = repo
        .capture_feedback_refinement_boundary(&proposal.id)
        .await
        .unwrap()
        .captures
        .pop()
        .unwrap();

    let queued_root = repo
        .add_feedback(ProposalFeedbackCreateInput {
            proposal_id: &proposal.id,
            parent_id: None,
            author_kind: "agent",
            author_model: Some("review-model"),
            body: "separately queued feedback",
        })
        .await
        .unwrap();
    let separately_queued = repo
        .capture_feedback_refinement_boundary(&proposal.id)
        .await
        .unwrap()
        .captures
        .pop()
        .unwrap();
    queue_feedback_refinement_generation_for_test(&db, &separately_queued.injection.id).await;

    let ordinary = repo
        .add_debate_trail_entry(ProposalDebateTrailCreateInput {
            proposal_id: &proposal.id,
            kind: "objection",
            body: "ordinary adversary objection",
            blocking: true,
            agent_role: "adversary",
            author_kind: "agent",
            author_model: Some("adversary-model"),
            source_task_id: None,
            against_revision_seq: 1,
            round: 9,
            body_metadata: None,
        })
        .await
        .unwrap();

    let response = server
        .dispatch_tool(
            "proposal_show",
            json!({
                "id": proposal.id,
                "fields": ["feedback_refinements", "debate"],
            }),
        )
        .await
        .unwrap();
    let mut refinements = response["feedback_refinements"].clone();
    normalize_timestamps(&mut refinements);
    assert_eq!(
        refinements,
        json!([
            {
                "root_feedback_id": root.id, "generation": 1, "state": "accepted",
                "debate_entry_id": first.debate_entry.id, "round": 1,
                "source_rows": [
                    {"source_feedback_id": root.id, "source_ordinal": 1, "author_kind": "user", "author_user_id": author, "body": "root blocking feedback", "severity": "blocking", "created_at": "<timestamp>"},
                    {"source_feedback_id": advisory.id, "source_ordinal": 2, "source_parent_id": root.id, "author_kind": "user", "author_user_id": "feedback-author", "body": "advisory context", "severity": "advisory", "created_at": "<timestamp>"}
                ],
                "accepted_disposition": "fixed_revision", "accepted_revision_seq": 2,
                "accepted_at": "<timestamp>", "accepted_by_user_id": "judge-fixed"
            },
            {
                "root_feedback_id": root.id, "generation": 2, "state": "wont_fix",
                "debate_entry_id": second.debate_entry.id, "round": 2,
                "source_rows": [{"source_feedback_id": second_reply.id, "source_ordinal": 1, "source_parent_id": root.id, "author_kind": "agent", "author_model": "review-model", "body": "second generation blocking feedback", "severity": "blocking", "created_at": "<timestamp>"}],
                "accepted_disposition": "wont_fix", "accepted_reason": "outside the agreed scope",
                "accepted_at": "<timestamp>", "accepted_by_user_id": "judge-wont-fix"
            },
            {
                "root_feedback_id": root.id, "generation": 3, "state": "withdrawn_by_author",
                "debate_entry_id": third.debate_entry.id, "round": 3,
                "source_rows": [{"source_feedback_id": third_reply.id, "source_ordinal": 1, "source_parent_id": root.id, "author_kind": "agent", "author_model": "review-model", "body": "withdrawn generation feedback", "severity": "blocking", "created_at": "<timestamp>"}],
                "withdrawn_at": "<timestamp>", "withdrawn_by_user_id": "feedback-author"
            },
            {
                "root_feedback_id": root.id, "generation": 4, "state": "injected",
                "debate_entry_id": injected.debate_entry.id, "round": 4,
                "source_rows": [{"source_feedback_id": queued_reply.id, "source_ordinal": 1, "source_parent_id": root.id, "author_kind": "agent", "author_model": "review-model", "body": "queued generation feedback", "severity": "blocking", "created_at": "<timestamp>"}]
            },
            {
                "root_feedback_id": queued_root.id, "generation": 1, "state": "queued", "round": 5,
                "source_rows": [{"source_feedback_id": queued_root.id, "source_ordinal": 1, "author_kind": "agent", "author_model": "review-model", "body": "separately queued feedback", "severity": "blocking", "created_at": "<timestamp>"}]
            }
        ])
    );

    let ordinary_row = response["debate_trail"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == ordinary.id)
        .unwrap();
    assert_eq!(ordinary_row["kind"], "objection");
    assert_eq!(ordinary_row["agent_role"], "adversary");
    assert!(ordinary_row.get("source_feedback_id").is_none());
    assert!(ordinary_row.get("source_rows").is_none());
    assert!(ordinary_row.get("disposition_state").is_none());
}
