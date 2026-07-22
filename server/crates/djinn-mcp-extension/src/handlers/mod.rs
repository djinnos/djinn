//! Handler groups for the extension dispatch.
//!
//! Each submodule corresponds to a logical tool group dispatched from
//! [`crate::dispatch`].

pub(crate) mod code_intel;
#[allow(unused_imports)]
pub(crate) mod jit_pitfalls;
pub(crate) mod memory_agent;
mod proposal_authoring;
mod proposal_read;
pub(crate) mod task_admin;
pub(crate) mod task_epic;
