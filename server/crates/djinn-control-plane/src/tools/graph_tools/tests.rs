#![allow(unused_imports)]

use super::handler_impact_check::{
    CrateIndex, ImpactAggregator, check_impact_check_staleness,
    derive_safe_slice_and_recommendation,
};
use crate::bridge::*;
use crate::tools::graph_exclusions::GraphExclusions;

include!("tests_part1.inc");
include!("tests_part2.inc");
include!("tests_part3.inc");
include!("tests_df6s_pagination.inc");
include!("tests_crate_graph.inc");
include!("tests_registry_dispatch.inc");
include!("tests_coverage.inc");
