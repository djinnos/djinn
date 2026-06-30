//! Bridge coverage for registry-covered operations.
//!
//! The full operation registry records bridge method identities for every
//! supported `code_graph` operation. These tests keep the server adapter honest:
//! each registry-covered identity must have a same-named inherent method on the
//! server-owned [`RepoGraphBridge`] and the `RepoGraphOps` impl in `mod.rs` must
//! explicitly forward to that inherent method. This catches accidentally falling
//! back to control-plane trait defaults for non-basic families such as route,
//! flow, snapshot, status, and coupling operations.

use std::collections::BTreeSet;

use djinn_control_plane::tools::graph_tools::operation_registry::{
    CODE_GRAPH_REGISTRY, KNOWN_BRIDGE_METHODS,
};

use crate::mcp_bridge::graph_ops::RepoGraphBridge;

/// Expected `RepoGraphOps` trait methods that are forwarded by the
/// `RepoGraphBridge` adapter for every converted registry entry.
///
/// This list is intentionally manual — it mirrors `KNOWN_BRIDGE_METHODS`
/// in `operation_registry.rs` and is validated by the tests below.
const EXPECTED_BRIDGE_FORWARDINGS: &[&str] = &[
    "neighbors",
    "ranked",
    "implementations",
    "impact",
    "search",
    "query_subgraph",
    "route_map",
    "shape_check",
    "api_impact",
    "flow",
    "cycles",
    "orphans",
    "path",
    "edges",
    "describe",
    "context",
    "status",
    "workspaces",
    "symbols_at",
    "diff_touches",
    "detect_changes",
    "api_surface",
    "boundary_check",
    "hotspots",
    "complexity",
    "refactor_candidates",
    "metrics_at",
    "dead_symbols",
    "deprecated_callers",
    "touches_hot_path",
    "coupling",
    "coupling_hotspots",
    "coupling_hubs",
    "churn",
    "snapshot",
    "crate_graph",
];

const SERVER_GRAPH_OPS_MOD_RS: &str = include_str!("../mod.rs");

fn assert_inherent_bridge_method_exists(method: &str) {
    fn method_item<T>(_method: T) {}

    match method {
        "api_impact" => method_item(RepoGraphBridge::api_impact),
        "api_surface" => method_item(RepoGraphBridge::api_surface),
        "boundary_check" => method_item(RepoGraphBridge::boundary_check),
        "churn" => method_item(RepoGraphBridge::churn),
        "complexity" => method_item(RepoGraphBridge::complexity),
        "context" => method_item(RepoGraphBridge::context),
        "coupling" => method_item(RepoGraphBridge::coupling),
        "coupling_hotspots" => method_item(RepoGraphBridge::coupling_hotspots),
        "coupling_hubs" => method_item(RepoGraphBridge::coupling_hubs),
        "crate_graph" => method_item(RepoGraphBridge::crate_graph),
        "cycles" => method_item(RepoGraphBridge::cycles),
        "dead_symbols" => method_item(RepoGraphBridge::dead_symbols),
        "deprecated_callers" => method_item(RepoGraphBridge::deprecated_callers),
        "describe" => method_item(RepoGraphBridge::describe),
        "detect_changes" => method_item(RepoGraphBridge::detect_changes),
        "diff_touches" => method_item(RepoGraphBridge::diff_touches),
        "edges" => method_item(RepoGraphBridge::edges),
        "flow" => method_item(RepoGraphBridge::flow),
        "hotspots" => method_item(RepoGraphBridge::hotspots),
        "impact" => method_item(RepoGraphBridge::impact),
        "implementations" => method_item(RepoGraphBridge::implementations),
        "metrics_at" => method_item(RepoGraphBridge::metrics_at),
        "neighbors" => method_item(RepoGraphBridge::neighbors),
        "orphans" => method_item(RepoGraphBridge::orphans),
        "path" => method_item(RepoGraphBridge::path),
        "query_subgraph" => method_item(RepoGraphBridge::query_subgraph),
        "ranked" => method_item(RepoGraphBridge::ranked),
        "refactor_candidates" => method_item(RepoGraphBridge::refactor_candidates),
        "route_map" => method_item(RepoGraphBridge::route_map),
        "search" => method_item(RepoGraphBridge::search),
        "shape_check" => method_item(RepoGraphBridge::shape_check),
        "snapshot" => method_item(RepoGraphBridge::snapshot),
        "status" => method_item(RepoGraphBridge::status),
        "symbols_at" => method_item(RepoGraphBridge::symbols_at),
        "touches_hot_path" => method_item(RepoGraphBridge::touches_hot_path),
        "workspaces" => method_item(RepoGraphBridge::workspaces),
        other => panic!(
            "registry bridge_method '{other}' is not covered by the server \
             inherent-method existence check"
        ),
    }
}

#[test]
fn converted_registry_ops_have_bridge_forwardings() {
    // Every registry-routed bridge_method must be forwarded by RepoGraphBridge.
    for method in KNOWN_BRIDGE_METHODS {
        assert!(
            EXPECTED_BRIDGE_FORWARDINGS.contains(method),
            "registry-routed bridge_method '{method}' is not in EXPECTED_BRIDGE_FORWARDINGS — \
             add a forwarding stub in RepoGraphBridge",
        );
    }

    // Every expected forwarding must have a corresponding registry entry.
    for method in EXPECTED_BRIDGE_FORWARDINGS {
        assert!(
            CODE_GRAPH_REGISTRY
                .iter()
                .any(|e| e.bridge_method == *method),
            "EXPECTED_BRIDGE_FORWARDINGS has '{method}' not in any registry entry",
        );
    }

    // The two lists must be in sync with each other.
    assert_eq!(
        EXPECTED_BRIDGE_FORWARDINGS.len(),
        KNOWN_BRIDGE_METHODS.len(),
        "EXPECTED_BRIDGE_FORWARDINGS and KNOWN_BRIDGE_METHODS have different lengths"
    );
    for method in KNOWN_BRIDGE_METHODS {
        assert!(
            EXPECTED_BRIDGE_FORWARDINGS.contains(method),
            "KNOWN_BRIDGE_METHODS has '{method}' not in EXPECTED_BRIDGE_FORWARDINGS"
        );
    }
}

#[test]
fn registry_bridge_methods_have_inherent_server_methods() {
    let registry_methods = CODE_GRAPH_REGISTRY
        .iter()
        .map(|entry| entry.bridge_method)
        .collect::<BTreeSet<_>>();

    for method in registry_methods {
        assert_inherent_bridge_method_exists(method);
    }
}

#[test]
fn registry_bridge_methods_are_forwarded_by_repo_graph_ops_impl() {
    let registry_methods = CODE_GRAPH_REGISTRY
        .iter()
        .map(|entry| entry.bridge_method)
        .collect::<BTreeSet<_>>();

    for method in registry_methods {
        let forwarding_call = format!("RepoGraphBridge::{method}(");
        assert!(
            SERVER_GRAPH_OPS_MOD_RS.contains(&forwarding_call),
            "registry bridge_method '{method}' is not explicitly forwarded by \
             impl RepoGraphOps for RepoGraphBridge via `{forwarding_call}...`"
        );
    }
}

#[test]
fn forwarding_coverage_includes_non_basic_registry_families() {
    for method in [
        "route_map",
        "api_impact",
        "flow",
        "snapshot",
        "status",
        "coupling_hubs",
    ] {
        assert!(
            CODE_GRAPH_REGISTRY
                .iter()
                .any(|entry| entry.bridge_method == method),
            "non-basic bridge_method '{method}' is not covered by the registry"
        );
        assert_inherent_bridge_method_exists(method);
        let forwarding_call = format!("RepoGraphBridge::{method}(");
        assert!(
            SERVER_GRAPH_OPS_MOD_RS.contains(&forwarding_call),
            "non-basic bridge_method '{method}' is not forwarded by the server adapter"
        );
    }
}
