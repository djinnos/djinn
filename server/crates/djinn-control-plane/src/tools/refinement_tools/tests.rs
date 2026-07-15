#![allow(unused_imports)]

use super::*;
use crate::server::DjinnMcpServer;
use crate::state::stubs::test_mcp_state;
use crate::tools::proposal_ops::EvidenceLifecycleState;
use crate::tools::refinement_helpers::{
    build_refinement_status, check_needs_evidence_cap, validate_demand_evidence,
};
use djinn_core::events::EventBus;
use djinn_db::{Database, EffectiveCreatorProvenance, ProposalCreateInput, ProposalRepository};
use std::sync::Arc;

include!("tests_part1.inc");
include!("tests_part2.inc");
include!("tests_part3.inc");
include!("tests_part4.inc");
include!("tests_part5.inc");
include!("tests_part6.inc");
include!("tests_part7.inc");
include!("tests_part8.inc");
