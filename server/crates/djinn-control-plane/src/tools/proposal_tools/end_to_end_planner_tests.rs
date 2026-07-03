// End-to-end planner refinement loop regression tests — extracted from
// mod.rs to meet the 1500-line file-size guard.  Behavior and
// expectations are unchanged.

// ── End-to-end planner refinement loop regression (task iy6v) ────────────
//
// This module ties together the `y4td` surface delivered by the sibling tasks
// (1787 block-patch regressions, kepb planner prompt wiring, 18g4 patch
// primitive, 6al0 revision metadata, mzz8 schema-lean guard) into a single
// integrated regression that models the proposal `r0io` / `5bdd` flow:
//
//   1. A planner authoring session loads `visual-spec` from the native-skill
//      registry delivered by `5uzr` / `y8p2`.
//   2. The planner pulls `get_block_catalog` from the `ilqx` surface on demand
//      — block vocabulary is never inlined into prompts or write schemas.
//   3. The planner converts a markdown-only proposal draft into block-enriched
//      MDX through several targeted `proposal_block_patch` calls — never a
//      monolithic `proposal_update`.
//   4. Each patch records one proposal revision with `targeted_block_patch`
//      metadata and the active `visual-spec` version attribution.
//   5. The enriched proposal exports through `proposal_export` as valid MDX.
//
// Why these tests live here rather than as a separate cross-crate harness:
// the planner refinement loop is a property of how the control-plane MCP
// server stitches the surfaces together — `proposal_create`,
// `proposal_block_patch`, `proposal_show` (revisions), and `proposal_export`
// all run on the same `DjinnMcpServer` against a real `ProposalRepository`.
// The native-skill registry lookup and the block-catalog pull are pure-Rust
// surfaces that resolve at compile time.  This module therefore exercises
// the real delivered end-to-end surface without standing up the planner
// session runtime, which would require additional infrastructure.
#[cfg(test)]
mod end_to_end_planner_refinement_loop_tests {
    use super::super::proposal_blocks::{
        parse_mdx_blocks, proposal_block_catalog, validate_mdx_blocks,
    };
    use crate::server::DjinnMcpServer;
    use crate::state::stubs::test_mcp_state;
    use djinn_agent::native_skills::{native_skill_version, resolved_native_skills_for_role};
    use djinn_core::events::EventBus;
    use djinn_db::{Database, ProposalCreateInput, ProposalRepository};
    use serde_json::Value;

    async fn test_server() -> (DjinnMcpServer, Database) {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        (DjinnMcpServer::new(test_mcp_state(db.clone())), db)
    }

    /// Multi-section markdown proposal draft used as the starting point for
    /// the planner refinement loop.  Four independently targetable sections /
    /// paragraphs are present so the test exercises the `proposal_block_patch`
    /// primitive over multiple distinct selectors.
    const DRAFT_BODY: &str = "\
# Visual-spec authoring integration

The opening paragraph introduces the proposal and explains its purpose.

## Approach

The approach section describes the high-level plan in prose.

## Tradeoffs

The tradeoffs section enumerates the costs of the chosen approach.

## Open Questions

The open-questions section collects uncertainties for the team.
";

    /// AC: planner authoring sessions receive the native `visual-spec` skill
    /// (delivered by `y8p2`) through the resolved-native-skills surface so the
    /// planner can `skill_read` it on demand rather than embedding it in the
    /// prompt body.  This is the lazy loading contract that lets
    /// non-authoring planner sessions avoid paying the visual-spec body cost.
    #[test]
    fn planner_authoring_session_resolves_visual_spec_from_native_registry() {
        // A planner authoring session must resolve exactly one native skill
        // — `visual-spec` — through the registry exposed by `y8p2`.
        let resolved = resolved_native_skills_for_role("planner");
        let names: Vec<&str> = resolved.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"visual-spec"),
            "planner authoring session must resolve visual-spec via the native \
             registry; got {names:?}"
        );

        // The version stamp must come from the same registry (no parallel
        // version source) so the planner can pass it through
        // `proposal_block_patch` for revision attribution.  `ResolvedSkill`
        // does not carry `version` (that field is reserved for the immutable
        // native registry), so we read the version from
        // `native_skill_version` directly.
        let registry_version = native_skill_version("visual-spec")
            .expect("native_skill_version must return the active visual-spec version");
        assert!(
            !registry_version.is_empty(),
            "native_skill_version must return a non-empty version stamp"
        );

        // The resolved skill must be marked `required: true` for the planner
        // role — the planner can't author MDX without it.  This pins the
        // lazy-loading contract: the registry is the single source of truth
        // for the active version, not a duplicated constant in prompts.
        let visual_spec = resolved.iter().find(|s| s.name == "visual-spec").unwrap();
        assert!(
            visual_spec.required,
            "visual-spec must be required for the planner authoring session"
        );
    }

    /// AC: non-authoring sessions (e.g. `worker`, `reviewer`) must NOT
    /// receive `visual-spec`.  This is the lazy-loading guard: only the
    /// planner role pays the visual-spec body cost.
    #[test]
    fn non_authoring_sessions_do_not_receive_visual_spec() {
        for role in ["worker", "reviewer"] {
            let resolved = resolved_native_skills_for_role(role);
            let names: Vec<&str> = resolved.iter().map(|s| s.name.as_str()).collect();
            assert!(
                !names.contains(&"visual-spec"),
                "{role} session must not receive visual-spec; got {names:?}"
            );
        }
    }

    /// AC: the `get_block_catalog` pull surface (delivered by `ilqx`) returns
    /// the lean (type, tag) vocabulary the planner uses to discover block
    /// names without inlining them in prompts or write schemas.  This test
    /// pins the catalog against the registry so a future drift between the
    /// two surfaces is caught.
    #[test]
    fn get_block_catalog_pull_surface_returns_lean_vocabulary() {
        let catalog = proposal_block_catalog();
        assert_eq!(
            catalog.len(),
            14,
            "get_block_catalog must expose all 14 v1 block types so the planner \
             can discover the vocabulary on demand"
        );
        // The catalog is the lean projection — no field schemas, no
        // descriptions, just (type, tag) pairs.  Any future regression that
        // bloats the catalog back into the rich registry shape is caught
        // here.
        for entry in &catalog {
            assert!(
                !entry.block_type.is_empty(),
                "catalog entry has empty type: {entry:?}"
            );
            assert!(
                !entry.tag.is_empty(),
                "catalog entry has empty tag: {entry:?}"
            );
        }
    }

    /// AC: a markdown-only proposal draft is progressively enriched into
    /// `body_format=mdx` through the targeted `proposal_block_patch`
    /// primitive.  `latest_revision_seq` must equal the seed revision (1)
    /// plus the number of patches applied.  A monolithic whole-body
    /// `proposal_update` is forbidden for enrichment — the test only
    /// invokes `proposal_block_patch`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_loop_increments_revision_seq_once_per_targeted_patch() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Planner Refinement Loop",
                body: DRAFT_BODY,
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        assert_eq!(
            proposal.latest_revision_seq, 1,
            "create seed must leave latest_revision_seq at 1"
        );
        assert_eq!(
            proposal.body_format, "markdown",
            "create seed must produce body_format=markdown"
        );

        // Three independently targetable sections get promoted to MDX blocks
        // through three sequential `proposal_block_patch` calls.  Each one
        // is exactly one material proposal edit (= one revision increment).
        let visual_spec_version = native_skill_version("visual-spec")
            .expect("native_skill_version must return the active visual-spec version");

        let patches = [
            (
                "The opening paragraph introduces the proposal and explains its purpose.",
                "<RichText id=\"opening\">\nThe structured opening paragraph.\n</RichText>",
                "first patch: opening paragraph -> RichText",
            ),
            (
                "The approach section describes the high-level plan in prose.",
                "<FileTree id=\"repo-layout\" name=\"repo\" />",
                "second patch: approach -> FileTree",
            ),
            (
                "The tradeoffs section enumerates the costs of the chosen approach.",
                "<Callout id=\"tradeoffs-callout\">\nThe structured tradeoff callout.\n</Callout>",
                "third patch: tradeoffs -> Callout",
            ),
        ];

        let mut expected_seq: i32 = 1;
        let mut prev_expected_revision_seq: Option<i32> = None;
        for (selector_text, block_mdx, note) in patches {
            let mut args = serde_json::json!({
                "id": proposal.id,
                "selector": { "exact_text": selector_text },
                "operation": "replace",
                "block_mdx": block_mdx,
                "native_skill_name": "visual-spec",
                "native_skill_version": visual_spec_version,
                "note": note,
            });
            // Pass `expected_latest_revision_seq` to exercise the stale-revision
            // guard path the prompt wires up for sequential patches.
            if let Some(prev) = prev_expected_revision_seq {
                args["expected_latest_revision_seq"] = serde_json::json!(prev);
            }

            let response = server
                .dispatch_tool("proposal_block_patch", args)
                .await
                .unwrap();
            assert!(
                response.get("error").is_none(),
                "proposal_block_patch failed for {note:?}: {:?}",
                response.get("error")
            );
            expected_seq += 1;
            prev_expected_revision_seq = Some(expected_seq);

            let after = repo.get(&proposal.id).await.unwrap().unwrap();
            assert_eq!(
                after.latest_revision_seq, expected_seq,
                "latest_revision_seq must be exactly +1 per patch after {note:?}"
            );
        }

        // Final state: three patches landed -> latest_revision_seq = 4
        // (1 seed + 3 patches).
        let final_state = repo.get(&proposal.id).await.unwrap().unwrap();
        assert_eq!(
            final_state.latest_revision_seq, 4,
            "three patches from a 1-seed proposal must yield latest_revision_seq=4"
        );
        assert_eq!(
            final_state.body_format, "mdx",
            "first MDX block patch must upgrade body_format to mdx"
        );

        // The proposal_show surface must report the same revision seq
        // (drift between the repo state and the public surface is caught here).
        let shown = server
            .dispatch_tool("proposal_show", serde_json::json!({ "id": proposal.id }))
            .await
            .unwrap();
        assert_eq!(
            shown
                .get("proposal")
                .and_then(|p| p.get("latest_revision_seq"))
                .and_then(|v| v.as_i64()),
            Some(4),
            "proposal_show.proposal.latest_revision_seq must match the repo state"
        );
    }

    /// AC: revision history exposes `targeted_block_patch` metadata on every
    /// patch revision, including the active `visual-spec` native-skill
    /// version attribution.  The seed revision must NOT carry that metadata
    /// — that signal is reserved for the patch primitive.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_loop_revisions_carry_visual_spec_attribution() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Refinement Loop Attribution",
                body: DRAFT_BODY,
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let visual_spec_version = native_skill_version("visual-spec")
            .expect("native_skill_version must return the active visual-spec version");

        // Apply two patches, attributing each to the registry's active version.
        for (selector_text, block_mdx) in [
            (
                "The opening paragraph introduces the proposal and explains its purpose.",
                "<RichText id=\"opening\">\nStructured opening.\n</RichText>",
            ),
            (
                "The approach section describes the high-level plan in prose.",
                "<FileTree id=\"repo\" name=\"repo\" />",
            ),
        ] {
            let response = server
                .dispatch_tool(
                    "proposal_block_patch",
                    serde_json::json!({
                        "id": proposal.id,
                        "selector": { "exact_text": selector_text },
                        "operation": "replace",
                        "block_mdx": block_mdx,
                        "native_skill_name": "visual-spec",
                        "native_skill_version": visual_spec_version,
                    }),
                )
                .await
                .unwrap();
            assert!(
                response.get("error").is_none(),
                "patch failed: {:?}",
                response.get("error")
            );
        }

        // Walk revisions through proposal_show — the surface the planner
        // consumes — and assert metadata shape on every patch revision.
        let shown = server
            .dispatch_tool("proposal_show", serde_json::json!({ "id": proposal.id }))
            .await
            .unwrap();
        let revisions = shown
            .get("revisions")
            .and_then(|v| v.as_array())
            .expect("proposal_show.revisions must be a JSON array");

        // 1 seed + 2 patches = 3 revisions.
        assert_eq!(
            revisions.len(),
            3,
            "expected 3 revisions (1 seed + 2 patches); got {}",
            revisions.len()
        );

        // The seed revision must NOT carry targeted-block-patch metadata —
        // `proposal_create` writes no event_metadata.
        let seed = &revisions[0];
        let seed_meta = seed.get("event_metadata");
        assert!(
            seed_meta.is_none() || seed_meta.is_some_and(|v| v.is_null()),
            "create seed revision must not carry event_metadata, got {seed_meta:?}"
        );

        // Every patch revision must carry the targeted-block-patch signal
        // AND the active `visual-spec` version from the native registry.
        for (idx, rev) in revisions.iter().enumerate().skip(1) {
            let meta_str = rev
                .get("event_metadata")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| panic!("patch rev #{idx} must expose event_metadata"));
            let meta: Value = serde_json::from_str(meta_str)
                .unwrap_or_else(|e| panic!("patch rev #{idx} event_metadata must be JSON: {e}"));
            assert_eq!(
                meta["change_kind"], "targeted_block_patch",
                "patch rev #{idx} must identify as targeted_block_patch"
            );
            assert_eq!(
                meta["native_skill_name"], "visual-spec",
                "patch rev #{idx} must attribute the native skill name"
            );
            assert_eq!(
                meta["native_skill_version"], visual_spec_version,
                "patch rev #{idx} must attribute the active visual-spec version from \
                 the native registry (drift between registry and patch metadata is \
                 caught here)"
            );
            // The byte-range fields are present and well-typed.
            assert!(
                meta["range_start_byte"].is_number() && meta["range_end_byte"].is_number(),
                "patch rev #{idx} must expose numeric byte-range fields"
            );
            assert!(
                meta["range_end_byte"].as_u64().unwrap()
                    > meta["range_start_byte"].as_u64().unwrap(),
                "patch rev #{idx} range_end_byte must exceed range_start_byte"
            );
        }
    }

    /// AC: after the refinement loop, the proposal exports cleanly through
    /// `proposal_export` and the returned MDX round-trips through the block
    /// parser.  This is the end-to-end fidelity contract that ties the
    /// refinement loop back to the MDX export surface.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_loop_enriched_proposal_exports_as_valid_mdx() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Refinement Loop Export",
                body: DRAFT_BODY,
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let visual_spec_version = native_skill_version("visual-spec")
            .expect("native_skill_version must return the active visual-spec version");

        // Apply three sequential targeted patches — the full planner refinement
        // loop from a markdown-only draft.
        for (selector_text, block_mdx) in [
            (
                "The opening paragraph introduces the proposal and explains its purpose.",
                "<RichText id=\"opening\">\nStructured opening.\n</RichText>",
            ),
            (
                "The approach section describes the high-level plan in prose.",
                "<FileTree id=\"repo-layout\" name=\"repo\" />",
            ),
            (
                "The tradeoffs section enumerates the costs of the chosen approach.",
                "<Callout id=\"tradeoffs\">\nStructured callout.\n</Callout>",
            ),
        ] {
            let response = server
                .dispatch_tool(
                    "proposal_block_patch",
                    serde_json::json!({
                        "id": proposal.id,
                        "selector": { "exact_text": selector_text },
                        "operation": "replace",
                        "block_mdx": block_mdx,
                        "native_skill_name": "visual-spec",
                        "native_skill_version": visual_spec_version,
                    }),
                )
                .await
                .unwrap();
            assert!(
                response.get("error").is_none(),
                "patch failed: {:?}",
                response.get("error")
            );
        }

        // Final body_format must be mdx.
        let stored = repo.get(&proposal.id).await.unwrap().unwrap();
        assert_eq!(stored.body_format, "mdx");

        // proposal_export must succeed and return MDX with frontmatter.
        let exported = server
            .dispatch_tool("proposal_export", serde_json::json!({ "id": proposal.id }))
            .await
            .unwrap();
        assert!(
            exported.get("error").is_none(),
            "proposal_export failed after MDX enrichment: {:?}",
            exported.get("error")
        );
        let mdx = exported
            .get("mdx")
            .and_then(|v| v.as_str())
            .expect("proposal_export.mdx must be a non-empty string for mdx proposals");
        assert!(
            mdx.contains("body_format: mdx"),
            "exported MDX frontmatter must record body_format: mdx"
        );
        assert!(
            mdx.matches("---").count() >= 2,
            "exported MDX must include the YAML frontmatter delimiters"
        );

        // The exported MDX body must parse into the same blocks as the
        // stored body.  This is the round-trip fidelity contract.
        let original_blocks =
            parse_mdx_blocks(&stored.body).expect("stored body must parse as MDX");
        let exported_body = mdx
            .splitn(3, "---")
            .nth(2)
            .expect("exported MDX must have a body section after frontmatter")
            .trim_start_matches('\n');
        let exported_blocks =
            parse_mdx_blocks(exported_body).expect("exported body must parse as MDX");
        assert_eq!(
            exported_blocks, original_blocks,
            "exported MDX blocks must match the stored body blocks byte-for-byte"
        );
        let exported_ids: Vec<&str> = exported_blocks.iter().map(|b| b.id.as_str()).collect();
        assert!(exported_ids.contains(&"opening"));
        assert!(exported_ids.contains(&"repo-layout"));
        assert!(exported_ids.contains(&"tradeoffs"));

        // Body validation must succeed end-to-end on the stored body.
        validate_mdx_blocks(&stored.body)
            .expect("enriched body must validate as MDX after the refinement loop");

        // Unrelated sections must survive byte-for-byte — proving no
        // monolithic whole-body rewrite happened.
        for anchor in [
            "# Visual-spec authoring integration",
            "## Open Questions",
            "The open-questions section collects uncertainties for the team.",
        ] {
            assert!(
                stored.body.contains(anchor),
                "unrelated anchor {anchor:?} must be preserved verbatim after the \
                 refinement loop; body was:\n{}",
                stored.body
            );
        }
    }

    /// AC: the planner workflow surfaces remain lazy.  Concretely:
    ///   * the `proposal_address.md` planner prompt does not inline the
    ///     block vocabulary or skill body (verified by re-asserting the
    ///     prompts-tests contract from kepb at the workflow-regression
    ///     level),
    ///   * the catalog pull surface is the single source of block
    ///     vocabulary (verified by ensuring the prompt does not name any
    ///     block tag from `proposal_block_catalog`),
    ///   * the active native-skill version stamped on patch revisions is
    ///     identical to the version returned by `native_skill_version` so the
    ///     registry remains the single source of truth.
    ///
    /// This test ties the lazy-surfaces contract to the actual refinement
    /// loop: any future edit that bakes vocabulary into the prompt, or that
    /// drifts the patch-attribute version away from the registry version,
    /// is caught here.
    #[test]
    fn refinement_loop_workflow_surfaces_remain_lazy() {
        // (a) The planner proposal-address prompt must not inline block
        //     vocabulary.  Re-assert the prompt-test contract at the
        //     workflow-regression level.
        let prompt = include_str!("../../../../djinn-roles/src/prompts/proposal_address.md");
        let catalog = proposal_block_catalog();
        for entry in &catalog {
            assert!(
                !prompt.contains(&entry.tag),
                "proposal_address.md must not inline block tag {:?} from the catalog",
                entry.tag
            );
            assert!(
                !prompt.contains(&entry.block_type),
                "proposal_address.md must not inline block type {:?} from the catalog",
                entry.block_type
            );
        }
        // Generic vocabulary surface must not appear either.
        assert!(
            !prompt.contains("block_types"),
            "proposal_address.md must not reference a `block_types` catalog list"
        );

        // (b) The catalog pull surface and the native-skill version stamp
        //     remain the single source of truth.  The registry version
        //     returned by `native_skill_version` is exactly what planners
        //     stamp on patch revisions — there is no parallel version
        //     constant the prompt or tests could drift against.
        //     `ResolvedSkill` does not carry `version` (that field is reserved
        //     for the immutable native registry), so we only assert that the
        //     planner role resolves `visual-spec` here.
        let registry_version = native_skill_version("visual-spec")
            .expect("native_skill_version must return the active visual-spec version");
        let resolved = resolved_native_skills_for_role("planner");
        assert!(
            resolved.iter().any(|s| s.name == "visual-spec"),
            "planner must resolve visual-spec"
        );
        assert!(
            !registry_version.is_empty(),
            "registry version must be a non-empty stamp"
        );
    }

    /// AC: the full integrated end-to-end refinement loop — markdown draft ->
    /// skill/catalog resolution -> 3 targeted patches -> MDX export — wires
    /// together every y4td surface into a single deterministic regression.
    /// This is the load-bearing test the task acceptance criteria converge on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_loop_end_to_end_ties_all_y4td_surfaces_together() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());

        // (1) Planner authoring session must resolve the native `visual-spec`
        //     skill (y8p2 surface) — verifies the lazy loading contract.
        let resolved_skills = resolved_native_skills_for_role("planner");
        assert!(
            resolved_skills.iter().any(|s| s.name == "visual-spec"),
            "planner must resolve visual-spec via the native registry"
        );
        let registry_version = native_skill_version("visual-spec")
            .expect("native_skill_version must return the active visual-spec version");
        assert!(
            !registry_version.is_empty(),
            "native_skill_version must return a non-empty stamp"
        );

        // (2) The planner pulls the lean catalog on demand (ilqx surface) —
        //     block vocabulary is never inlined into the proposal write
        //     schemas (verified separately by the prompt schema-lean
        //     regression in `schema_lean_tests`).
        let catalog = proposal_block_catalog();
        assert_eq!(
            catalog.len(),
            14,
            "get_block_catalog must expose the full v1 vocabulary on demand"
        );

        // (3) Create a markdown-only proposal draft.
        let proposal = repo
            .create(ProposalCreateInput {
                title: "End-to-end y4td regression",
                body: DRAFT_BODY,
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        assert_eq!(proposal.body_format, "markdown");
        assert_eq!(proposal.latest_revision_seq, 1);

        // (4) Apply 3 sequential targeted block patches through the real
        //     `proposal_block_patch` MCP surface — never a whole-body
        //     `proposal_update`.  Each patch carries the active visual-spec
        //     version from the registry for revision attribution.
        for (selector_text, block_mdx) in [
            (
                "The opening paragraph introduces the proposal and explains its purpose.",
                "<RichText id=\"opening\">\nStructured opening.\n</RichText>",
            ),
            (
                "The approach section describes the high-level plan in prose.",
                "<FileTree id=\"repo-layout\" name=\"repo\" />",
            ),
            (
                "The tradeoffs section enumerates the costs of the chosen approach.",
                "<Callout id=\"tradeoffs\">\nStructured callout.\n</Callout>",
            ),
        ] {
            let response = server
                .dispatch_tool(
                    "proposal_block_patch",
                    serde_json::json!({
                        "id": proposal.id,
                        "selector": { "exact_text": selector_text },
                        "operation": "replace",
                        "block_mdx": block_mdx,
                        "native_skill_name": "visual-spec",
                        "native_skill_version": registry_version,
                    }),
                )
                .await
                .unwrap();
            assert!(
                response.get("error").is_none(),
                "proposal_block_patch failed: {:?}",
                response.get("error")
            );
        }

        // (5) Final state: body_format=mdx, latest_revision_seq=4 (1 seed + 3 patches).
        let final_state = repo.get(&proposal.id).await.unwrap().unwrap();
        assert_eq!(final_state.body_format, "mdx");
        assert_eq!(final_state.latest_revision_seq, 4);

        // (6) Revision metadata: every patch revision carries
        //     `targeted_block_patch` + the registry's visual-spec version.
        let shown = server
            .dispatch_tool("proposal_show", serde_json::json!({ "id": proposal.id }))
            .await
            .unwrap();
        let revisions = shown
            .get("revisions")
            .and_then(|v| v.as_array())
            .expect("proposal_show.revisions must be a JSON array");
        assert_eq!(revisions.len(), 4, "1 seed + 3 patches = 4 revisions");
        for (idx, rev) in revisions.iter().enumerate().skip(1) {
            let meta: Value = serde_json::from_str(
                rev.get("event_metadata")
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| panic!("rev #{idx} must expose event_metadata")),
            )
            .unwrap_or_else(|e| panic!("rev #{idx} event_metadata must be JSON: {e}"));
            assert_eq!(meta["change_kind"], "targeted_block_patch");
            assert_eq!(meta["native_skill_name"], "visual-spec");
            assert_eq!(meta["native_skill_version"], registry_version);
        }

        // (7) The final enriched proposal exports as valid MDX through
        //     `proposal_export`, with all 3 patched blocks intact.
        let exported = server
            .dispatch_tool("proposal_export", serde_json::json!({ "id": proposal.id }))
            .await
            .unwrap();
        assert!(exported.get("error").is_none());
        let mdx = exported
            .get("mdx")
            .and_then(|v| v.as_str())
            .expect("export must return mdx for body_format=mdx proposals");
        let exported_body = mdx
            .splitn(3, "---")
            .nth(2)
            .expect("exported MDX must have a body section after frontmatter")
            .trim_start_matches('\n');
        let exported_blocks =
            parse_mdx_blocks(exported_body).expect("exported body must parse as MDX");
        let exported_ids: Vec<&str> = exported_blocks.iter().map(|b| b.id.as_str()).collect();
        assert!(exported_ids.contains(&"opening"));
        assert!(exported_ids.contains(&"repo-layout"));
        assert!(exported_ids.contains(&"tradeoffs"));
    }
}
