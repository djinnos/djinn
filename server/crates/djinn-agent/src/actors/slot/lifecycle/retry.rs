//! Agent facade for canonical slot lifecycle retry helpers.

#[allow(unused_imports)]
pub(super) use djinn_slot::lifecycle::retry::{
    is_database_locked, retry_task_transition_on_locked,
};
