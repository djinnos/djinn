//! Tests for the `refinement_tools` concern.
//!
//! The test bodies live in `tests/` as real modules. They used to be `.inc`
//! fragments pulled in with the textual include macro, which made them
//! invisible to the code graph: `rust-analyzer scip` emits no document for an
//! included file, so nothing defined inside one could be found by an agent,
//! and the `*.rs`-filtered CI guards skipped them too. The macro is now
//! banned outright — see `scripts/check-include-macro.sh`.
//!
//! Imports and fixtures shared by more than one child module are declared
//! here — the children reach them through `use super::*`, exactly as the
//! single flat module did before the split, without widening any real
//! visibility.

#![allow(unused_imports)]

use super::*;
use crate::server::DjinnMcpServer;
use crate::state::stubs::test_mcp_state;
use crate::tools::proposal_ops::EvidenceLifecycleState;
use crate::tools::refinement_helpers::{
    build_refinement_status, check_needs_evidence_cap, validate_demand_evidence,
};
use djinn_core::events::EventBus;
use djinn_db::{Database, ProposalCreateInput, ProposalRepository};
use std::sync::Arc;

mod tests_part1;
mod tests_part10;
mod tests_part2;
mod tests_part3;
mod tests_part4;
mod tests_part5;
mod tests_part6;
mod tests_part7;
mod tests_part8;
mod tests_part9;

// Fixtures shared across the child modules. Re-exporting them here — rather
// than having each child import from each sibling — keeps the shared surface
// in one greppable place, and `use super::*` in a child picks them up.
use tests_part1::test_server;
use tests_part4::{insert_epic, insert_project, insert_task, setup_structured_claim};
use tests_part5::{
    admit_refinement_run, create_judge_task, create_test_user, link_proposal_to_project,
    mutation_snapshot, setup_demand_test, valid_demand_params,
};
