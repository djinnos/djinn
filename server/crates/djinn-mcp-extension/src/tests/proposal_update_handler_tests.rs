//! Handler-level tests for the in-pod `proposal_update` write path
//! (`handlers::task_epic::call_proposal_update`).
//!
//! These mirror the server-side tool tests in
//! `djinn-control-plane/src/tools/proposal_tools/create_tests.rs`
//! (`mdx_upgrade_and_validation_tests`): the in-pod agent path must resolve
//! the persisted `body_format` and validate MDX blocks byte-identically to the
//! server-side `proposal_update` — auto-upgrading a markdown body that carries
//! block tags to `mdx`, rejecting unknown tags and empty children-based
//! blocks, and accepting valid children / content-attribute forms.

use std::path::{Path, PathBuf};

use djinn_control_plane::McpState;
use djinn_control_plane::state::stubs::test_mcp_state;
use djinn_core::events::EventBus;
use djinn_db::{Database, ProposalCreateInput, ProposalRepository};

use crate::context::ExtensionContext;
use crate::handlers::task_epic::{call_proposal_block_patch, call_proposal_update};

/// Minimal [`ExtensionContext`] stub over an in-memory database. Only `db()`
/// and `event_bus()` are exercised by the proposal handlers; the remaining
/// capabilities return inert defaults.
struct StubCtx {
    db: Database,
}

#[async_trait::async_trait]
impl ExtensionContext for StubCtx {
    fn db(&self) -> Database {
        self.db.clone()
    }
    fn event_bus(&self) -> EventBus {
        EventBus::noop()
    }
    fn mcp_state(&self) -> McpState {
        test_mcp_state(self.db.clone())
    }
    fn lsp(&self) -> djinn_lsp::LspManager {
        djinn_lsp::LspManager::new()
    }
    fn working_root_for(&self, fallback: &Path) -> PathBuf {
        fallback.to_path_buf()
    }
    fn default_project_id(&self) -> Option<&str> {
        None
    }
}

async fn test_ctx() -> StubCtx {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    StubCtx { db }
}

/// Seed a plain-markdown proposal directly through the repository.
async fn seed_markdown_proposal(ctx: &StubCtx) -> djinn_core::models::proposal::Proposal {
    let repo = ProposalRepository::new(ctx.db(), ctx.event_bus());
    repo.create(ProposalCreateInput {
        title: "Plain",
        body: "plain markdown body",
        acceptance_criteria: Some("[]"),
        status: None,
        body_format: Some("markdown"),
    })
    .await
    .unwrap()
}

fn args(v: serde_json::Value) -> Option<serde_json::Map<String, serde_json::Value>> {
    Some(v.as_object().cloned().expect("args must be an object"))
}

/// A valid block-bearing body (children-form callout + trailing question-form,
/// satisfying the mdx question-form placement gate).
const VALID_BLOCK_BODY: &str = "# Proposal\n\n<Callout id=\"note\" tone=\"info\">\nImportant context.\n</Callout>\n\n<QuestionForm id=\"q\" title=\"Open Questions\">\nAny concerns?\n</QuestionForm>\n";

/// Omitted body_format + a body carrying known block tags → the stored
/// body_format is upgraded to "mdx" (previously stored as markdown with all
/// block validation skipped).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_omitted_format_with_blocks_stores_mdx() {
    let ctx = test_ctx().await;
    let existing = seed_markdown_proposal(&ctx).await;

    let result = call_proposal_update(
        &ctx,
        &args(serde_json::json!({ "id": existing.id, "body": VALID_BLOCK_BODY })),
    )
    .await
    .expect("update should succeed");
    assert_eq!(result["ok"], serde_json::json!(true));

    let repo = ProposalRepository::new(ctx.db(), ctx.event_bus());
    let stored = repo.get(&existing.id).await.unwrap().unwrap();
    assert_eq!(
        stored.body_format, "mdx",
        "a markdown body carrying block tags must be stored as mdx"
    );
    assert_eq!(stored.body, VALID_BLOCK_BODY);
}

/// An unknown block tag in a markdown-declared body is rejected (previously
/// passed silently because markdown bodies skipped validation).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_unknown_block_tag_rejected() {
    let ctx = test_ctx().await;
    let existing = seed_markdown_proposal(&ctx).await;

    let err = call_proposal_update(
        &ctx,
        &args(serde_json::json!({
            "id": existing.id,
            "body": "# P\n\n<TotallyUnknown id=\"z\" />\n",
            "body_format": "markdown",
        })),
    )
    .await
    .expect_err("unknown block tag must be rejected");
    assert!(err.contains("TotallyUnknown"), "error was: {err}");

    // Nothing was persisted.
    let repo = ProposalRepository::new(ctx.db(), ctx.event_bus());
    let stored = repo.get(&existing.id).await.unwrap().unwrap();
    assert_eq!(stored.body, "plain markdown body");
    assert_eq!(stored.body_format, "markdown");
}

/// The exact production failure: a children-based block written in the
/// self-closing attribute form (`<Decisions id="x" decisions={[…]} />`) is
/// rejected with an actionable error directing the author to `###`-heading
/// children markdown.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_self_closing_decisions_attr_form_rejected() {
    let ctx = test_ctx().await;
    let existing = seed_markdown_proposal(&ctx).await;

    let body = "# P\n\n<Decisions id=\"choice\" decisions={[{\"decision\":\"JWT\"}]} />\n\n<QuestionForm id=\"q\" title=\"Q\">\nq?\n</QuestionForm>\n";
    let err = call_proposal_update(
        &ctx,
        &args(serde_json::json!({ "id": existing.id, "body": body })),
    )
    .await
    .expect_err("self-closing Decisions must be rejected");
    assert!(err.contains("Decisions block"), "error was: {err}");
    assert!(err.contains("`choice`"), "error must name the id: {err}");
    assert!(
        err.contains("###"),
        "error must direct the author to `###` heading children: {err}"
    );
}

/// Self-closing file-tree and checklist blocks (children-based, no attribute
/// alternative) are likewise rejected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_self_closing_children_blocks_rejected() {
    let ctx = test_ctx().await;
    let existing = seed_markdown_proposal(&ctx).await;

    for (id, tag_body) in [
        ("layout", "<FileTree id=\"layout\" root=\"src\" />"),
        ("acc", "<Checklist id=\"acc\" />"),
    ] {
        let body = format!(
            "# P\n\n{tag_body}\n\n<QuestionForm id=\"q\" title=\"Q\">\nq?\n</QuestionForm>\n"
        );
        let result = call_proposal_update(
            &ctx,
            &args(serde_json::json!({ "id": existing.id, "body": body })),
        )
        .await;
        let err = match result {
            Err(e) => e,
            Ok(v) => panic!("expected rejection for empty block `{id}`, got {v}"),
        };
        assert!(
            err.contains(&format!("`{id}`")),
            "error must name the block id `{id}`: {err}"
        );
        assert!(err.contains("children"), "error was: {err}");
    }
}

/// A valid children-form Decisions block plus an annotated-code attribute form
/// are accepted and stored as mdx.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_valid_children_and_attr_forms_accepted() {
    let ctx = test_ctx().await;
    let existing = seed_markdown_proposal(&ctx).await;

    let body = "# P\n\n<Decisions id=\"d\">\n### Use JWT for stateless auth\nStatus: accepted\n\nWe scale horizontally.\n</Decisions>\n\n<AnnotatedCode id=\"code\" language=\"rust\" code={`fn main() {}`} />\n\n<QuestionForm id=\"q\" title=\"Q\">\nq?\n</QuestionForm>\n";
    let result = call_proposal_update(
        &ctx,
        &args(serde_json::json!({ "id": existing.id, "body": body })),
    )
    .await
    .expect("valid children + attribute forms must be accepted");
    assert_eq!(result["ok"], serde_json::json!(true));

    let repo = ProposalRepository::new(ctx.db(), ctx.event_bus());
    let stored = repo.get(&existing.id).await.unwrap().unwrap();
    assert_eq!(stored.body_format, "mdx");
}

/// A plain markdown body without block tags stays markdown — no upgrade, no
/// block validation applied.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_plain_markdown_stays_markdown() {
    let ctx = test_ctx().await;
    let existing = seed_markdown_proposal(&ctx).await;

    let result = call_proposal_update(
        &ctx,
        &args(serde_json::json!({ "id": existing.id, "body": "# Revised\n\nStill prose." })),
    )
    .await
    .expect("plain markdown update should succeed");
    assert_eq!(result["ok"], serde_json::json!(true));

    let repo = ProposalRepository::new(ctx.db(), ctx.event_bus());
    let stored = repo.get(&existing.id).await.unwrap().unwrap();
    assert_eq!(stored.body_format, "markdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn block_patch_stale_revision_is_rejected_before_selector_resolution() {
    let ctx = test_ctx().await;
    let existing = seed_markdown_proposal(&ctx).await;
    let err = call_proposal_block_patch(
        &ctx,
        &args(serde_json::json!({
            "id": existing.id,
            "expected_latest_revision_seq": 99,
            "selector": { "exact_text": "not present" },
            "operation": "replace",
            "block_mdx": "replacement",
        })),
    )
    .await
    .expect_err("stale guard must run before selection");
    assert!(err.contains("stale revision"), "error was: {err}");
    let repo = ProposalRepository::new(ctx.db(), ctx.event_bus());
    let stored = repo.get(&existing.id).await.unwrap().unwrap();
    assert_eq!(stored.latest_revision_seq, 1);
    assert_eq!(repo.revisions(&existing.id).await.unwrap().len(), 1);
}
