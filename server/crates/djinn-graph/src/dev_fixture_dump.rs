//! Dev-only, `#[ignore]`d test that builds the full repo dependency graph
//! locally (real SCIP indexers, no DB, no server) and dumps an **uncapped**
//! snapshot-shaped JSON — every node, every edge — for the galaxy Storybook
//! fixture (proposal lmkv). The MCP snapshot path caps nodes and drawable
//! edges for wire-payload sanity; look-and-feel review of the galaxy needs
//! the whole graph, which only this offline path can produce today.
//!
//! Run (from the repo root; takes minutes — rust-analyzer indexes the
//! workspace for real):
//!
//! ```text
//! DJINN_DUMP_ROOT=$PWD \
//! DJINN_DUMP_OUT=$PWD/ui/src/components/galaxy/__fixtures__/djinn-code-graph.snapshot.json \
//! cargo test -p djinn-graph --lib dump_full_galaxy_snapshot -- --ignored --nocapture
//! ```
//!
//! The output file is gitignored (see `__fixtures__/.gitignore`).

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "dev tool: runs real SCIP indexers on DJINN_DUMP_ROOT and writes DJINN_DUMP_OUT"]
    async fn dump_full_galaxy_snapshot() {
        let root = PathBuf::from(
            std::env::var("DJINN_DUMP_ROOT").expect("set DJINN_DUMP_ROOT to the repo to index"),
        );
        let out = PathBuf::from(
            std::env::var("DJINN_DUMP_OUT").expect("set DJINN_DUMP_OUT to the fixture path"),
        );
        let scip_out = tempfile::tempdir().expect("tempdir for scip artifacts");

        let run = crate::scip_indexer::run_indexers_already_locked(
            &root,
            scip_out.path(),
            None,
            None,
            None,
            None,
        )
        .await
        .expect("run_indexers");
        eprintln!("workspace statuses: {:?}", run.workspace_statuses);
        assert!(
            !run.artifacts.is_empty(),
            "no SCIP artifacts produced — are indexers installed on PATH?"
        );

        let parsed =
            crate::scip_parser::parse_scip_artifacts_with_cache_reuse(&run.artifacts, false)
                .expect("parse_scip_artifacts");
        let crate_map = crate::canonical_graph::derive_crate_map(&root);
        let options = crate::repo_graph::RepoGraphBuildOptions::from_env();
        let graph =
            crate::repo_graph::RepoDependencyGraph::try_build_with_source_options_and_crate_map(
                &parsed,
                Some(&root),
                options,
                Some(&crate_map),
            )
            .expect("build graph");

        let g = graph.graph();
        let mut nodes = Vec::with_capacity(g.node_count());
        for idx in g.node_indices() {
            let node = graph.node(idx);
            nodes.push(serde_json::json!({
                "id": node.id.stable_uid(),
                "kind": node.kind,
                "label": node.display_name,
                "file_path": node.file_path.as_ref().map(|p| p.display().to_string()),
                "workspace": node.workspace,
                "pagerank": 0.0,
                "cognitive": node.complexity.as_ref().map(|c| c.cognitive),
                "is_test": node.is_test,
            }));
        }

        use petgraph::visit::EdgeRef;
        let mut edges = Vec::with_capacity(g.edge_count());
        for edge_ref in g.edge_references() {
            let weight = edge_ref.weight();
            edges.push(serde_json::json!({
                "from": graph.node(edge_ref.source()).id.stable_uid(),
                "to": graph.node(edge_ref.target()).id.stable_uid(),
                "kind": format!("{:?}", weight.kind),
                "confidence": weight.confidence,
            }));
        }

        let payload = serde_json::json!({
            "project_id": "local-dump",
            "git_head": "local",
            "generated_at": "",
            "truncated": false,
            "total_nodes": nodes.len(),
            "total_edges": edges.len(),
            "node_cap": nodes.len(),
            "nodes": nodes,
            "edges": edges,
        });

        std::fs::write(&out, serde_json::to_string(&payload).expect("serialize"))
            .expect("write fixture");
        eprintln!(
            "wrote {} nodes / {} edges to {}",
            payload["total_nodes"], payload["total_edges"], out.display()
        );
    }
}
