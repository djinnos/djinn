//! Bridge coverage for registry-converted operations.
//!
//! The full operation registry records bridge method identities for every
//! supported `code_graph` operation, while runtime dispatch is still only
//! registry-routed for the vxmw vertical slice.  These tests verify the
//! stronger forwarding-stub contract for the methods that are actually
//! converted today; follow-up migration tasks can extend the converted set
//! as they route more operations through the registry.

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
