//! Chat context regression tests.
//!
//! The prior iteration covered repo-map reinforcement / companion-note
//! wiring — that subsystem has been retired alongside the aider-style
//! repo map. If we reintroduce per-request stable context here, new
//! tests should cover composition via
//! `build_project_context_block` rather than cache fetch paths.
