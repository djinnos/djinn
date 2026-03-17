// TaskRepository and all query types live in djinn-db.
// Re-export everything so existing import paths continue to work.
pub use djinn_db::repositories::task::*;

// Server-layer merge/git transition helpers (depend on AgentContext, GitActor, etc.)
// These cannot move to djinn-db due to server-level dependencies.
pub mod transitions;

#[cfg(test)]
mod tests;
