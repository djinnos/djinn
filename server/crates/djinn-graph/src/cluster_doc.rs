//! PR F4 — auto-generate one `cluster_doc` note per [`Community`].
//!
//! After [`crate::canonical_graph::ensure_canonical_graph`] installs a
//! freshly-built graph, the warm pipeline calls
//! [`spawn_generate_for_all`] which, when the
//! `DJINN_CLUSTER_DOCS` feature flag is on, writes one note per
//! detected community into Dolt via [`djinn_db::NoteRepository`].
//!
//! Each note has:
//! - `note_type = "cluster_doc"` (folder maps to `reference/clusters` —
//!   see `djinn_db::repositories::note::file_helpers::folder_for_type`).
//! - `title    = "{label} (cluster)"`.
//! - `body`    is a deterministic structural summary of the community
//!   (member count, label, top-K member names, intra/outgoing edge
//!   counts, keywords). The plan calls for an LLM-rendered prose doc
//!   here — this PR ships the placeholder summarizer; the slot for the
//!   LLM rendered template lives at
//!   `djinn-agent/src/prompts/cluster-doc.md` and will be wired in once
//!   the agent runtime is reachable from the warm context. The
//!   placeholder text is descriptive enough that `memory_search` can
//!   still index and surface it.
//! - `tags`    is `community.keywords` JSON-encoded.
//!
//! Idempotence: each community has a stable `id` (sha256-of-members);
//! we form a permalink (`reference/clusters/community-{id}`) and skip
//! the write if a note with that permalink already exists.
//!
//! Two-pass synthesis (per the plan's PR F4 spec): the leaf-then-parent
//! rendering is a no-op here because the F3 modularity-based detector
//! produces a *flat* partition — every community is treated as a leaf.
//! When/if a hierarchical detector replaces it, parent clusters render
//! in a second pass from the children's already-generated docs.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use djinn_core::events::EventBus;
use djinn_db::repositories::note::NoteRepository;
use djinn_db::Database;
use petgraph::graph::NodeIndex;
use petgraph::visit::EdgeRef as PetgraphEdgeRef;

use crate::communities::Community;
use crate::repo_graph::RepoDependencyGraph;

/// `note_type` value used for cluster docs. Matches the `folder_for_type`
/// arm in `djinn-db`.
pub const CLUSTER_DOC_NOTE_TYPE: &str = "cluster_doc";

/// `title` suffix appended to the community's label so cluster docs
/// stand out in `memory_list` / `memory_search` results.
const CLUSTER_DOC_TITLE_SUFFIX: &str = " (cluster)";

/// Cap on the number of member display names listed verbatim in the
/// placeholder body. Long communities are summarized with an "and N
/// more" tail.
const TOP_MEMBERS_LIMIT: usize = 12;

/// Cap on the number of outgoing-edge target labels listed verbatim.
const TOP_OUTGOING_LIMIT: usize = 8;

// ─── Feature flag ─────────────────────────────────────────────────────────────

/// Read the `DJINN_CLUSTER_DOCS` flag. Default `false` — generating
/// cluster docs costs LLM tokens (once the prompt template is wired up)
/// so opt-in per project.
///
/// Recognized "on" values (case-insensitive): `1`, `true`, `yes`, `on`.
/// Unset or any other value means off.
pub fn cluster_docs_enabled() -> bool {
    match std::env::var("DJINN_CLUSTER_DOCS") {
        Err(_) => false,
        Ok(v) => {
            let lower = v.trim().to_ascii_lowercase();
            matches!(lower.as_str(), "1" | "true" | "yes" | "on")
        }
    }
}

// ─── Permalink ────────────────────────────────────────────────────────────────

/// Stable permalink for the cluster doc of `community`. Lives in the
/// `reference/clusters` folder per `folder_for_type("cluster_doc")`. The
/// id half is the community's first-16-hex-chars sha digest, which is
/// stable across rebuilds as long as membership is stable.
pub fn cluster_doc_permalink(community: &Community) -> String {
    format!("reference/clusters/community-{}", community.id)
}

// ─── Body builder ─────────────────────────────────────────────────────────────

/// Build a placeholder body for the cluster doc. Deterministic — the
/// same `(graph, community)` always produces the same string so the
/// idempotence check (skip on existing permalink) doesn't churn.
///
/// Sections:
/// 1. one-line tldr (label + member count + cohesion)
/// 2. keywords list
/// 3. top members (display names, capped by [`TOP_MEMBERS_LIMIT`])
/// 4. intra/outgoing edge counts
/// 5. top outgoing call targets, capped by [`TOP_OUTGOING_LIMIT`]
///
/// The format intentionally tracks the slots in
/// `djinn-agent/src/prompts/cluster-doc.md` so the same body shape
/// works as the eventual LLM prompt's "structural facts" block.
pub fn build_placeholder_body(graph: &RepoDependencyGraph, community: &Community) -> String {
    let pg = graph.graph();

    // Resolve member display names + collect file-path roots for the
    // "files touched" tail.
    let mut member_names: Vec<String> = Vec::with_capacity(community.member_ids.len());
    let mut file_roots: BTreeSet<String> = BTreeSet::new();
    for &member_pos in &community.member_ids {
        let idx = NodeIndex::new(member_pos);
        if idx.index() >= pg.node_count() {
            // Defensive: drop members whose ids fall outside the live
            // graph (can happen when the artifact loaded predates a
            // build). The caller will see a degraded-but-still-valid
            // doc rather than a panic.
            continue;
        }
        let node = &pg[idx];
        member_names.push(node.display_name.clone());
        if let Some(path) = &node.file_path
            && let Some(seg) = first_path_segment(path)
        {
            file_roots.insert(seg);
        }
    }

    // Edge categorization.
    let member_set: BTreeSet<usize> = community.member_ids.iter().copied().collect();
    let mut intra_edges: usize = 0;
    let mut outgoing_targets: BTreeMap<String, usize> = BTreeMap::new();
    let mut outgoing_count: usize = 0;
    for &member_pos in &community.member_ids {
        let idx = NodeIndex::new(member_pos);
        if idx.index() >= pg.node_count() {
            continue;
        }
        for edge_ref in pg.edges(idx) {
            let target = edge_ref.target();
            if member_set.contains(&target.index()) {
                intra_edges += 1;
            } else {
                outgoing_count += 1;
                let name = pg[target].display_name.clone();
                *outgoing_targets.entry(name).or_default() += 1;
            }
        }
    }

    // Most-frequent outgoing targets (descending by count, ties by
    // name).
    let mut top_outgoing: Vec<(String, usize)> = outgoing_targets.into_iter().collect();
    top_outgoing.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    top_outgoing.truncate(TOP_OUTGOING_LIMIT);

    let cohesion_pct = (community.cohesion * 100.0).round() as i64;

    // Render. Markdown headings track the prompt template slots so the
    // same body works as the LLM prompt's structural-facts block once
    // wired up.
    let mut out = String::new();
    out.push_str(&format!(
        "**Cluster `{}`** — {} symbols, cohesion ≈ {}%.\n\n",
        community.label, community.symbol_count, cohesion_pct
    ));

    if !community.keywords.is_empty() {
        out.push_str("## Keywords\n\n");
        out.push_str(
            &community
                .keywords
                .iter()
                .map(|k| format!("`{k}`"))
                .collect::<Vec<_>>()
                .join(", "),
        );
        out.push_str("\n\n");
    }

    if !member_names.is_empty() {
        out.push_str("## Members (top symbols)\n\n");
        let shown = member_names.len().min(TOP_MEMBERS_LIMIT);
        for name in member_names.iter().take(shown) {
            out.push_str(&format!("- `{name}`\n"));
        }
        if member_names.len() > shown {
            out.push_str(&format!(
                "- _…and {} more_\n",
                member_names.len() - shown
            ));
        }
        out.push('\n');
    }

    if !file_roots.is_empty() {
        out.push_str("## Files touched\n\n");
        for root in &file_roots {
            out.push_str(&format!("- `{root}/`\n"));
        }
        out.push('\n');
    }

    out.push_str("## Connectivity\n\n");
    out.push_str(&format!(
        "- intra-community edges: **{intra_edges}**\n",
    ));
    out.push_str(&format!(
        "- outgoing edges (to other clusters / singletons): **{outgoing_count}**\n",
    ));
    out.push('\n');

    if !top_outgoing.is_empty() {
        out.push_str("## Top outgoing call targets\n\n");
        for (name, count) in &top_outgoing {
            out.push_str(&format!("- `{name}` ({count})\n"));
        }
        out.push('\n');
    }

    out.push_str(
        "_Auto-generated by `cluster_doc::generate_for_all` after community detection. \
         Re-rendered on each warm cycle whose membership changes the community id._\n",
    );

    out
}

fn first_path_segment(path: &Path) -> Option<String> {
    path.components().find_map(|c| match c {
        std::path::Component::Normal(s) => s.to_str().map(str::to_string),
        _ => None,
    })
}

// ─── Persistence ──────────────────────────────────────────────────────────────

/// Encode `keywords` as a JSON array string suitable for the `notes.tags`
/// column. Returns `"[]"` for empty.
fn encode_tags(keywords: &[String]) -> String {
    serde_json::to_string(keywords).unwrap_or_else(|_| "[]".to_string())
}

/// Persist one cluster-doc note per community in `graph.communities()`,
/// idempotent on the community's stable permalink.
///
/// Returns the number of new notes written (i.e. excludes communities
/// that already had a doc). Errors per community are logged and
/// swallowed so one bad write doesn't sink the rest of the pass.
pub async fn generate_for_all(
    db: Database,
    event_bus: EventBus,
    project_id: &str,
    graph: Arc<RepoDependencyGraph>,
) -> usize {
    let note_repo = NoteRepository::new(db, event_bus);
    let mut written: usize = 0;

    for community in graph.communities() {
        let permalink = cluster_doc_permalink(community);

        // Idempotent: skip if a doc for this community already exists.
        match note_repo.get_by_permalink(project_id, &permalink).await {
            Ok(Some(_existing)) => {
                tracing::debug!(
                    project_id = %project_id,
                    community_id = %community.id,
                    permalink = %permalink,
                    "cluster_doc: skipping — note already exists"
                );
                continue;
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    project_id = %project_id,
                    community_id = %community.id,
                    error = %e,
                    "cluster_doc: get_by_permalink failed; skipping"
                );
                continue;
            }
        }

        let title = format!("{}{}", community.label, CLUSTER_DOC_TITLE_SUFFIX);
        let body = build_placeholder_body(graph.as_ref(), community);
        let tags = encode_tags(&community.keywords);

        match note_repo
            .create_db_note_with_permalink(
                project_id,
                &permalink,
                &title,
                &body,
                CLUSTER_DOC_NOTE_TYPE,
                &tags,
            )
            .await
        {
            Ok(note) => {
                tracing::info!(
                    project_id = %project_id,
                    community_id = %community.id,
                    note_id = %note.id,
                    permalink = %note.permalink,
                    symbol_count = community.symbol_count,
                    "cluster_doc: wrote note"
                );
                written += 1;
            }
            Err(e) => {
                tracing::warn!(
                    project_id = %project_id,
                    community_id = %community.id,
                    permalink = %permalink,
                    error = %e,
                    "cluster_doc: create_db_note_with_permalink failed"
                );
            }
        }
    }

    written
}

/// Fire-and-forget kickoff. Returns immediately; the actual write loop
/// runs on a detached `tokio::spawn`. Skips when:
///
/// * `DJINN_CLUSTER_DOCS` is unset / off — default rollout state.
/// * `graph.communities()` is empty (no communities, no work).
///
/// This is what `ensure_canonical_graph` calls at the end of the warm
/// pipeline.
pub fn spawn_generate_for_all(
    db: Database,
    event_bus: EventBus,
    project_id: String,
    graph: Arc<RepoDependencyGraph>,
) {
    if !cluster_docs_enabled() {
        return;
    }
    if graph.communities().is_empty() {
        return;
    }
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        let community_count = graph.communities().len();
        let written = generate_for_all(db, event_bus, &project_id, graph).await;
        tracing::info!(
            project_id = %project_id,
            community_count,
            written,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "cluster_doc: generate_for_all finished"
        );
    });
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::*;
    use crate::repo_graph::{
        REPO_GRAPH_ARTIFACT_VERSION, RepoDependencyGraph, RepoGraphArtifact,
        RepoGraphArtifactEdge, RepoGraphEdgeKind, RepoGraphNode, RepoGraphNodeKind,
        RepoNodeKey,
    };

    /// Build a tiny two-cluster graph (mirrors the one in `communities.rs`
    /// tests) so we have a real `RepoDependencyGraph` to feed the
    /// placeholder builder.
    fn two_cluster_graph() -> RepoDependencyGraph {
        let mk_symbol_node = |name: &str, file: &str| RepoGraphNode {
            id: RepoNodeKey::Symbol(format!("symbol:{name}")),
            kind: RepoGraphNodeKind::Symbol,
            display_name: name.to_string(),
            language: Some("rust".to_string()),
            file_path: Some(PathBuf::from(file)),
            symbol: Some(format!("symbol:{name}")),
            symbol_kind: None,
            is_external: false,
            visibility: None,
            signature: None,
            documentation: vec![],
            signature_parts: None,
            is_test: false,
        };

        let nodes = vec![
            mk_symbol_node("auth_login", "src/auth/login.rs"),
            mk_symbol_node("auth_session", "src/auth/session.rs"),
            mk_symbol_node("auth_token", "src/auth/token.rs"),
            mk_symbol_node("billing_charge", "src/billing/charge.rs"),
            mk_symbol_node("billing_invoice", "src/billing/invoice.rs"),
            mk_symbol_node("billing_refund", "src/billing/refund.rs"),
        ];

        let edge = |s, t, w| RepoGraphArtifactEdge {
            source: s,
            target: t,
            kind: RepoGraphEdgeKind::SymbolReference,
            weight: w,
            evidence_count: 1,
            confidence: 0.9,
            reason: None,
            step: None,
        };
        let edges = vec![
            // auth: tight triangle
            edge(0, 1, 5.0),
            edge(1, 0, 5.0),
            edge(1, 2, 5.0),
            edge(2, 1, 5.0),
            edge(0, 2, 5.0),
            edge(2, 0, 5.0),
            // billing: tight triangle
            edge(3, 4, 5.0),
            edge(4, 3, 5.0),
            edge(4, 5, 5.0),
            edge(5, 4, 5.0),
            edge(3, 5, 5.0),
            edge(5, 3, 5.0),
            // Thin bridge
            edge(2, 3, 0.5),
            edge(3, 2, 0.5),
        ];

        let artifact = RepoGraphArtifact {
            version: REPO_GRAPH_ARTIFACT_VERSION,
            nodes,
            edges,
            symbol_ranges: BTreeMap::new(),
            communities: Vec::new(),
            processes: Vec::new(),
        };
        RepoDependencyGraph::from_artifact(&artifact)
    }

    #[test]
    fn placeholder_body_non_empty_for_synthetic_community() {
        // `from_artifact` re-runs detection in the build path because
        // we go through `RepoDependencyGraph::build` only when an
        // artifact carries no communities — but our two-cluster
        // fixture goes via `from_artifact(...)` which preserves the
        // (empty) sidecar. Re-derive communities from the live graph
        // for this unit test.
        let graph = two_cluster_graph();
        let communities = crate::communities::detect_communities(&graph);
        assert!(
            !communities.is_empty(),
            "fixture should yield at least one community"
        );

        let community = &communities[0];
        let body = build_placeholder_body(&graph, community);
        assert!(!body.is_empty(), "placeholder body should not be empty");
        assert!(
            body.contains(&community.label),
            "body should mention the cluster label"
        );
        assert!(
            body.contains("Members"),
            "body should include a members section"
        );
        assert!(
            body.contains("Connectivity"),
            "body should include a connectivity section"
        );
        // Placeholder marker must be present so consumers can detect
        // un-LLM-rendered cluster docs.
        assert!(
            body.contains("Auto-generated by `cluster_doc::generate_for_all`"),
            "body should carry the auto-generation marker, got:\n{body}"
        );
    }

    #[test]
    fn permalink_is_stable_for_a_given_community() {
        let graph = two_cluster_graph();
        let communities = crate::communities::detect_communities(&graph);
        let community = &communities[0];
        let p1 = cluster_doc_permalink(community);
        let p2 = cluster_doc_permalink(community);
        assert_eq!(p1, p2);
        assert!(p1.starts_with("reference/clusters/community-"));
    }

    #[test]
    fn cluster_docs_enabled_default_is_off() {
        // SAFETY: tests run with no DJINN_CLUSTER_DOCS set by default.
        // Explicit unset just in case the harness leaks one.
        // SAFETY: setting / unsetting an env var is safe in single-
        // threaded unit-test bodies; std::env::set_var is marked unsafe
        // in newer toolchains because mutating the global env races
        // with concurrent getenv readers. This test does not spawn
        // threads, so the access is serialized.
        unsafe { std::env::remove_var("DJINN_CLUSTER_DOCS") };
        assert!(!cluster_docs_enabled());

        unsafe { std::env::set_var("DJINN_CLUSTER_DOCS", "1") };
        assert!(cluster_docs_enabled());

        unsafe { std::env::set_var("DJINN_CLUSTER_DOCS", "0") };
        assert!(!cluster_docs_enabled());
        unsafe { std::env::remove_var("DJINN_CLUSTER_DOCS") };
    }

    #[test]
    fn encode_tags_round_trips_keywords() {
        let kws = vec!["auth".to_string(), "session".to_string()];
        let s = encode_tags(&kws);
        assert_eq!(s, r#"["auth","session"]"#);
        assert_eq!(encode_tags(&[]), "[]");
    }

    /// Integration: feed `generate_for_all` a synthetic graph against
    /// an in-memory Dolt and verify exactly one note per community
    /// gets written, with the right type / title / tags / body, and
    /// that a second call is a no-op (idempotent).
    #[tokio::test]
    async fn generate_for_all_writes_one_note_per_community_idempotently() {
        use crate::test_helpers::create_test_db;
        use djinn_core::events::EventBus;
        use djinn_db::ProjectRepository;
        use djinn_db::repositories::note::NoteRepository;
        use std::sync::Arc;

        let db = create_test_db();
        let event_bus = EventBus::noop();
        let project = ProjectRepository::new(db.clone(), event_bus.clone())
            .create("test-cluster-doc", "test", "test-cluster-doc")
            .await
            .expect("create project");

        // Build a graph, run community detection, then re-build a
        // graph from an artifact whose `communities` field is
        // populated — `from_artifact` rehydrates the sidecar verbatim,
        // which is the same shape `ensure_canonical_graph` will see.
        let bare_graph = two_cluster_graph();
        let communities = crate::communities::detect_communities(&bare_graph);
        assert!(
            !communities.is_empty(),
            "fixture must yield non-empty communities for this test"
        );
        let expected = communities.len();

        let mut artifact = bare_graph.to_artifact();
        artifact.communities = communities;
        let graph = RepoDependencyGraph::from_artifact(&artifact);

        let arc = Arc::new(graph);
        let written = generate_for_all(db.clone(), event_bus.clone(), &project.id, arc.clone())
            .await;
        assert_eq!(written, expected, "first pass should write all communities");

        // Verify each community has a note.
        let note_repo = NoteRepository::new(db.clone(), event_bus.clone());
        for community in arc.communities() {
            let permalink = cluster_doc_permalink(community);
            let note = note_repo
                .get_by_permalink(&project.id, &permalink)
                .await
                .expect("get_by_permalink")
                .expect("note must exist");
            assert_eq!(note.note_type, CLUSTER_DOC_NOTE_TYPE);
            assert!(note.title.ends_with(CLUSTER_DOC_TITLE_SUFFIX));
            assert!(note.title.starts_with(&community.label));
            assert!(!note.content.is_empty());
            // Keywords landed in tags as a JSON array.
            assert!(
                note.tags.starts_with('['),
                "tags should be a JSON array, got {:?}",
                note.tags
            );
        }

        // Second pass is a no-op (idempotent on permalink).
        let written_again =
            generate_for_all(db.clone(), event_bus.clone(), &project.id, arc.clone()).await;
        assert_eq!(
            written_again, 0,
            "second pass should skip — notes already exist"
        );
    }
}
