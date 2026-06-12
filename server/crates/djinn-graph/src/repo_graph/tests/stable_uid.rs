use super::*;
use petgraph::stable_graph::StableGraph;

fn node_for_key(id: RepoNodeKey) -> RepoGraphNode {
    let kind = match &id {
        RepoNodeKey::File(_) => RepoGraphNodeKind::File,
        RepoNodeKey::Symbol(_) => RepoGraphNodeKind::Symbol,
        RepoNodeKey::Process(_) => RepoGraphNodeKind::Process,
        RepoNodeKey::Table(_) => RepoGraphNodeKind::Table,
        RepoNodeKey::Route(_) => RepoGraphNodeKind::Route,
        RepoNodeKey::Tool(_) => RepoGraphNodeKind::Tool,
    };

    RepoGraphNode {
        id,
        kind,
        display_name: "test-node".to_string(),
        language: None,
        file_path: None,
        symbol: None,
        symbol_kind: None,
        is_external: false,
        visibility: None,
        signature: None,
        documentation: Vec::new(),
        signature_parts: None,
        is_test: false,
        complexity: None,
        workspace: None,
        route_framework: None,
        route_handler_symbol: None,
    }
}

fn stable_uid_cases() -> Vec<(RepoNodeKey, &'static str)> {
    vec![
        (
            RepoNodeKey::File(PathBuf::from("src/lib.rs")),
            "file:src/lib.rs",
        ),
        (
            RepoNodeKey::Symbol("scip-rust pkg src/lib.rs `run`().".to_string()),
            "symbol:scip-rust pkg src/lib.rs `run`().",
        ),
        (
            RepoNodeKey::Process("process-abc123".to_string()),
            "process:process-abc123",
        ),
        (
            RepoNodeKey::Table("public.users".to_string()),
            "table:public.users",
        ),
        (
            RepoNodeKey::Route("GET /api/agents (axum)".to_string()),
            "route:GET /api/agents (axum)",
        ),
        (
            RepoNodeKey::Tool("agents.list".to_string()),
            "tool:agents.list",
        ),
    ]
}

#[test]
fn stable_uid_covers_all_repo_node_key_variants() {
    for (key, expected) in stable_uid_cases() {
        assert_eq!(key.stable_uid(), expected);

        let node = node_for_key(key);
        assert_eq!(node.stable_uid(), expected);
        assert_eq!(stable_node_uid(&node), expected);
    }
}

#[test]
fn stable_uid_is_deterministic_across_calls_and_node_indices() {
    for (key, expected) in stable_uid_cases() {
        let first_call = key.stable_uid();
        let second_call = key.stable_uid();
        assert_eq!(first_call, second_call);

        let first_build_node = node_for_key(key.clone());
        let rebuilt_node = node_for_key(key.clone());
        assert_eq!(first_build_node.stable_uid(), rebuilt_node.stable_uid());

        let mut graph_a: StableGraph<RepoGraphNode, ()> = StableGraph::default();
        let first_index = graph_a.add_node(first_build_node);

        let mut graph_b: StableGraph<RepoGraphNode, ()> = StableGraph::default();
        graph_b.add_node(node_for_key(RepoNodeKey::File(PathBuf::from(
            "placeholder.rs",
        ))));
        let second_index = graph_b.add_node(rebuilt_node);

        assert_ne!(first_index, second_index);
        assert_eq!(graph_a[first_index].stable_uid(), expected);
        assert_eq!(graph_b[second_index].stable_uid(), expected);
    }
}
