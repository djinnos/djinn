// Facade: doctor logic lives in `djinn-coordinator`.
//
// This module re-exports the full doctor surface so existing
// `djinn_agent::doctor::*` import paths keep resolving.
pub use djinn_coordinator::doctor::*;
