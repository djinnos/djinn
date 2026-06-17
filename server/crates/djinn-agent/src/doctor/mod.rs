//! `djinn-agent`-side doctor seed checks.
//!
//! Checks that live in `djinn-agent` (rather than `djinn-core`) because they
//! consume the `pub(crate)` live-mover API exposed under
//! [`crate::supervisor_impl`] and re-exported through
//! [`crate::supervisor_impl`] (the crate-internal module root). A
//! `DoctorCheck` impl in `djinn-core` would either need a public-API bridge
//! (more plumbing) or would have to duplicate the evidence model (rejected by
//! the `pitfalls/coupling-non-pr-diagnostics-to-pr-open-disposition-code`
//! guardrail).
//!
//! The `DoctorCheck` trait itself is defined in [`djinn_core::doctor`]; this
//! module mirrors the framework's shape and is registered into the framework's
//! registry by T5.
//!
//! `dead_code` is allowed at the module level because T4 delivers the check
//! implementation and bridge; T5 wires them into the registry. Until T5
//! lands, the non-test consumers do not yet exist.

#![allow(dead_code)]

pub mod leader_tick;
pub mod live_mover;
