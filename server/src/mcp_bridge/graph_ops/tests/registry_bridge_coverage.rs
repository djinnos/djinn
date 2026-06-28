//! Bridge coverage for registry-converted operations.
//!
//! The `RepoGraphOps` trait impl in `mod.rs` forwards every trait method,
//! so any missing forwarding is a compile error.  The tests below verify
//! the stronger contract: every `bridge_method` referenced by the
//! operation registry has a corresponding forwarding stub in the
//! `RepoGraphBridge` adapter (not just a trait default).
//!
//! When a new operation is registered in `operation_registry.rs`, both
//! its `RepoGraphOps::method` and its `RepoGraphBridge::method` must
//! exist.  The tests catch forward additions that forgot either side.

/// Expected `RepoGraphOps` trait methods that are forwarded by the
/// `RepoGraphBridge` adapter for every converted registry entry.
///
/// This list is intentionally manual — it mirrors `KNOWN_BRIDGE_METHODS`
/// in `operation_registry.rs` and is validated by the tests below.
const EXPECTED_BRIDGE_FORWARDINGS: &[&str] =
    &["neighbors", "impact", "context", "coupling_hotspots"];

#[test]
fn converted_registry_ops_have_bridge_forwardings() {
    // Import the operation registry from djinn-control-plane.
    use djinn_control_plane::tools::graph_tools::operation_registry::{
        CODE_GRAPH_REGISTRY, KNOWN_BRIDGE_METHODS,
    };

    // Every registered bridge_method must be in EXPECTED_BRIDGE_FORWARDINGS.
    for entry in CODE_GRAPH_REGISTRY {
        assert!(
            EXPECTED_BRIDGE_FORWARDINGS.contains(&entry.bridge_method),
            "registry entry '{}' has bridge_method '{}' not in EXPECTED_BRIDGE_FORWARDINGS — \
             add a forwarding stub in RepoGraphBridge",
            entry.name,
            entry.bridge_method,
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
