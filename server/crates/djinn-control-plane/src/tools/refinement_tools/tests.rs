#![allow(unused_imports)]

use super::*;
use crate::server::DjinnMcpServer;
use crate::state::stubs::test_mcp_state;
use djinn_core::events::EventBus;
use djinn_db::{Database, ProposalCreateInput};
use std::sync::Arc;

include!("tests_part1.inc");
include!("tests_part2.inc");
