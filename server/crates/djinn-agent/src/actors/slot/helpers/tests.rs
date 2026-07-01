// ─── b7pe cutover: code-context and reviewer-diff tests live in djinn-slot ───
//
// The eight tests that previously lived here exercised
// `code_context::{derive_task_scope_paths, format_knowledge_notes,
// is_role_auto_code_context_enabled, build_role_code_graph_context}` and
// `reviewer_diff::build_reviewer_diff_context` through `AgentContext` test
// fixtures.
//
// Since those helpers now delegate to the canonical `djinn_slot::helpers`
// implementations (see `code_context.rs` and `reviewer_diff.rs` shims in this
// directory), the behavioral test coverage is identical and is maintained
// canonically in:
//
//   `server/crates/djinn-slot/src/helpers/tests.rs`
//
// The agent-side shim delegation is compile-time verified: the `pub(crate) use`
// and `async fn` wrappers in `code_context.rs` / `reviewer_diff.rs` import and
// call the canonical functions, so any signature mismatch is caught by
// `cargo check -p djinn-agent`.
