//! Doctor seed-check submodules.
//!
//! Each submodule groups a small set of `DoctorCheck` impls from a single
//! incident class. T1 owns the first two (sessions + slots); T2-T4 will
//! extend this file with their own `pub mod` lines, and T5 wires them into
//! the framework registry.

pub mod k8s;
pub mod sessions;
pub mod slots;
