//! Stable durable-authority labels for reclamation reporting.
//!
//! These labels identify the ledger a reconciliation summary mutated. They are
//! deliberately distinct: resize lifecycle reconciliation changes
//! `build_pod_permits`, while lease reclamation and release change
//! `build_leases`.

/// Durable authority for resize lifecycle reconciliation.
pub const RESIZE_LIFECYCLE_LEDGER: &str = "build_pod_permits";

/// Durable authority for lease reclamation and release.
pub const LEASE_RECLAMATION_LEDGER: &str = "build_leases";

#[cfg(test)]
mod tests {
    use super::{LEASE_RECLAMATION_LEDGER, RESIZE_LIFECYCLE_LEDGER};

    #[test]
    fn resize_lifecycle_ledger_spelling_is_stable() {
        assert_eq!(RESIZE_LIFECYCLE_LEDGER, "build_pod_permits");
    }

    #[test]
    fn lease_reclamation_ledger_spelling_is_stable() {
        assert_eq!(LEASE_RECLAMATION_LEDGER, "build_leases");
    }

    #[test]
    fn lifecycle_and_lease_ledgers_are_distinct() {
        assert_ne!(RESIZE_LIFECYCLE_LEDGER, LEASE_RECLAMATION_LEDGER);
    }

    #[test]
    fn stable_ledger_identifier_contract_is_exact_and_distinct() {
        assert_eq!(RESIZE_LIFECYCLE_LEDGER, "build_pod_permits");
        assert_eq!(LEASE_RECLAMATION_LEDGER, "build_leases");
        assert_ne!(RESIZE_LIFECYCLE_LEDGER, LEASE_RECLAMATION_LEDGER);
    }
}
