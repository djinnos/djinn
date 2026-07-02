Conflict resolution plan:
1. Both `create.rs` and `mod.rs` are in merge-conflict state (status UU).
2. Per task xpj0: `proposal_add_target` and `proposal_remove_target` belong in `create.rs` alongside the existing CRUD tools (create/import/export/show/list/update/block-patch/delete already there from py7d/ca70).
3. `ProposalTargetParams` must move to `create.rs`; `mod.rs` should only retain feedback/signoff/lifecycle/refinement params and the `proposal_remaining_tool_router` (now only feedback/signoff/lifecycle/build tools).
4. Shared helpers stay in `mod.rs` as `pub(super)` and are re-imported by `create.rs`.
5. Verify re-exports in `mod.rs` include `ProposalTargetParams`.
6. Check cargo check passes after resolution.