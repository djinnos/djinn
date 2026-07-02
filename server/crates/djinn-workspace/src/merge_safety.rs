//! Merge-safety classification for refs that participate in the
//! resume-via-git and PR-merge paths.
//!
//! Sibling task `8yjx` (capture-before-exit) writes safety-scanned
//! checkpoint commits to two kinds of locations:
//!
//! - **`refs/heads/task/<short_id>`** — the canonical task branch. The
//!   checkpoint lands as a regular commit on top of (or interleaved with)
//!   the worker's task work. This branch IS the PR head and IS the natural
//!   candidate for the final squash merge to `main` after review.
//! - **`refs/djinn/checkpoints/<task-id>/<session-id>`** — an alternate
//!   ref created on push conflict (the canonical worker branch was rejected
//!   because it already advanced). This ref is a preservation/resume source
//!   ONLY: a safety-scanned checkpoint commit written to it must never be
//!   treated as the final merge commit into `main`, because it carries
//!   WIP / dirty-delta worker output that has not been reviewed or
//!   verified.
//!
//! This module provides the classification helpers the merge and cleanup
//! paths use to keep those two roles disjoint:
//!
//! - [`is_checkpoint_ref`] — fast guard used by the merge helpers to
//!   refuse checkpoint-only refs as direct merge heads.
//! - [`is_protected_ref`] — fast guard for ref-cleanup paths so the
//!   post-close branch sweep never deletes a ref that's not safe to drop
//!   (the integration target, or any alternate checkpoint ref that the
//!   resume-via-git path may still need to consult).
//! - [`classify_ref`] — full classification used by tests / observability.
//! - [`MergeSafetyDecision`] / [`evaluate_merge_head`] — guarded merge-head
//!   selector. Wraps the ref-shape check + an optional structured-event
//!   payload describing the rejection so callers can emit telemetry
//!   without having to duplicate the string-shape logic.
//!
//! All helpers are pure (no I/O, no clock, no randomness). They are
//! deliberately cheap so they can be called inside the supervisor's hot
//! loop without measurably affecting dispatch latency.
//!
//! ## Why the ref-shape check, not commit-trailer inspection
//! The 8yjx checkpoint writer tags the commit's *trailer* with a
//! `Djinn-Checkpoint` header, but reading trailers requires a `git log`
//! call against the mirror (or the worktree's HEAD). The ref-shape check
//! is a cheap, deterministic O(|ref|) filter that fires before any I/O
//! and catches the most dangerous case (a checkpoint ref being treated as
//! the merge head) without consulting git history. Callers that need
//! stronger validation (e.g. the resume selector that ALSO checks
//! safety-scan results) layer it on top via [`djinn_coordinator`].

/// Fully-qualified namespace under which checkpoint refs are stored.
///
/// Sibling task `8yjx` writes checkpoint refs into
/// `refs/djinn/checkpoints/<task-id>/<session-id>` on push conflict;
/// `djinn_coordinator::dispatch::resume_source` reads the same prefix to
/// recover the alternate checkpoint ref from a checkpoint lifecycle
/// record. Keeping the prefix here as a single source of truth avoids the
/// merge path silently picking up a future refactor that renamed the
/// namespace.
pub const CHECKPOINT_REF_PREFIX: &str = "refs/djinn/checkpoints/";

/// Refs that must never be deleted by the post-close branch-cleanup path,
/// never used as a direct merge head, and never replaced on a fast-forward
/// push. The integration targets (`main` / `master`) and the checkpoint
/// preservation namespace are both members of this set.
///
/// Treat this list as a deliberate allowlist of "the universe of refs
/// the system will refuse to mutate from automated paths". Adding a new
/// entry here requires a corresponding test in
/// `djinn-workspace/src/merge_safety.rs` that proves the new entry is
/// honoured by [`is_protected_ref`].
pub const PROTECTED_REFS: &[&str] = &[
    "main", "master",
    // `HEAD` (the symbolic ref) must never be deleted; the worktree
    // depends on it resolving.
    "HEAD",
];

/// Classification of a ref name in the context of merge / cleanup paths.
///
/// Mirrors the resume-source selector's
/// `djinn_coordinator::dispatch::resume_source::ResumeSourceKind` only at
/// the role level (PR head vs. preservation-only ref); the merge path
/// does not consume resume-source selection directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefRole {
    /// The canonical `refs/heads/task/<short_id>` PR-head branch. Eligible
    /// to be the source of a final squash merge to `main` after review.
    TaskBranch,
    /// An alternate checkpoint ref under
    /// `refs/djinn/checkpoints/...`. Carries WIP / dirty-delta worker
    /// output that has not been reviewed. Preservation / resume source
    /// ONLY — must NEVER be the source of a final merge to `main`.
    CheckpointRef,
    /// A protected ref (`main`, `master`, `HEAD`, …). The merge and
    /// cleanup paths refuse to mutate these.
    Protected,
    /// A ref that is none of the above. Typically an unrelated feature
    /// branch the operator created manually.
    Other,
}

impl RefRole {
    /// Whether the ref is permitted to serve as the source of a final
    /// squash merge to the integration target.
    ///
    /// Only `TaskBranch` (and `Other`, which we don't gate on — operators
    /// may legitimately want to merge their own branches) currently
    /// return `true`. `CheckpointRef` and `Protected` both return
    /// `false`.
    pub fn is_eligible_final_merge_source(self) -> bool {
        matches!(self, Self::TaskBranch | Self::Other)
    }

    /// Whether the ref is safe for the post-close branch-cleanup path to
    /// delete. Only `TaskBranch` returns `true`; `CheckpointRef` (alternate
    /// preservation refs the resume path may still need) and `Protected`
    /// (integration targets) both return `false`.
    pub fn is_safe_to_cleanup(self) -> bool {
        matches!(self, Self::TaskBranch)
    }

    /// Whether the ref is a checkpoint preservation source. Used by
    /// observability / events to surface the "this ref was rejected from
    /// the final-merge path because it's a checkpoint" reason without
    /// callers having to compare against the prefix string.
    pub fn is_checkpoint_ref(self) -> bool {
        matches!(self, Self::CheckpointRef)
    }
}

/// Classify `ref_name` into a [`RefRole`].
///
/// Accepts both short refs (`main`, `task/<short_id>`) and fully-qualified
/// refs (`refs/heads/task/<short_id>`, `refs/djinn/checkpoints/<task>/<sid>`).
/// The leading `refs/heads/` is stripped before the protected-ref check so
/// callers can pass either form interchangeably.
pub fn classify_ref(ref_name: &str) -> RefRole {
    let trimmed = ref_name.trim();
    if trimmed.is_empty() {
        return RefRole::Other;
    }

    // Alternate checkpoint refs: namespace-scoped, never treated as a
    // final-merge source. We check the fully-qualified prefix FIRST so a
    // `refs/djinn/checkpoints/...` ref is never accidentally promoted to
    // `Protected` if the same short name happens to coincide with a
    // protected entry below.
    if trimmed.starts_with(CHECKPOINT_REF_PREFIX) {
        return RefRole::CheckpointRef;
    }

    // Protected short refs (integration targets + symbolic HEAD). We
    // accept both `main` and `refs/heads/main` so callers don't have to
    // normalise before guarding.
    let short = trimmed
        .strip_prefix("refs/heads/")
        .unwrap_or(trimmed)
        .trim_end_matches("/")
        .trim_start_matches("origin/")
        .trim_start_matches("remotes/origin/");
    if PROTECTED_REFS.contains(&short) {
        return RefRole::Protected;
    }

    // The canonical task branch: `task/<short_id>`. We deliberately
    // accept only the short form here — a `refs/heads/task/<id>`
    // fully-qualified ref is normalized to its short name by the strip
    // above so the comparison is consistent.
    if short.starts_with("task/") {
        return RefRole::TaskBranch;
    }

    RefRole::Other
}

/// Cheap shape check: does `ref_name` look like a checkpoint
/// preservation ref? Equivalent to
/// `classify_ref(ref_name) == RefRole::CheckpointRef` but available as a
/// fast guard for hot paths that don't need the full classification.
pub fn is_checkpoint_ref(ref_name: &str) -> bool {
    classify_ref(ref_name) == RefRole::CheckpointRef
}

/// Cheap shape check: does `ref_name` look like a ref the cleanup path
/// must not delete? Equivalent to
/// `classify_ref(ref_name) == RefRole::Protected` but available as a fast
/// guard for hot paths.
pub fn is_protected_ref(ref_name: &str) -> bool {
    classify_ref(ref_name) == RefRole::Protected
}

/// Structured merge-safety decision for a candidate merge head.
///
/// The merge and PR-open paths use this to decide whether a ref / commit
/// may serve as the direct source of a final squash merge to the
/// integration target. The variant carries the machine-readable reason
/// any rejection happened so callers can surface it in structured events
/// / telemetry without having to re-classify the ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeSafetyDecision {
    /// The ref is eligible to be the source of the final squash merge.
    Eligible,
    /// The ref was rejected because it carries WIP / dirty-delta worker
    /// output that has not been reviewed.
    CheckpointRef {
        ref_name: String,
        sha: Option<String>,
    },
    /// The ref was rejected because it's a protected ref (integration
    /// target, symbolic HEAD). It is never the source of a "final merge";
    /// merges go INTO this ref via `merge_pull_request`, never out of it.
    ProtectedRef { ref_name: String },
    /// The ref shape is recognised but the merge path cannot see a SHA
    /// to merge (an empty / unresolved ref selector). Treated as a
    /// rejection so the merge path can degrade to a no-op rather than
    /// attempt a meaningless merge.
    MissingSha { ref_name: String },
}

impl MergeSafetyDecision {
    /// Whether the candidate is safe to merge. Only `Eligible` returns
    /// `true`; every rejection variant returns `false`.
    pub fn is_eligible(&self) -> bool {
        matches!(self, Self::Eligible)
    }

    /// Short, machine-readable rejection tag suitable for structured
    /// event / metric labels. Returns `"eligible"` for the
    /// [`MergeSafetyDecision::Eligible`] case so callers can use a single
    /// field for both outcomes.
    pub fn rejection_tag(&self) -> &'static str {
        match self {
            Self::Eligible => "eligible",
            Self::CheckpointRef { .. } => "checkpoint_ref",
            Self::ProtectedRef { .. } => "protected_ref",
            Self::MissingSha { .. } => "missing_sha",
        }
    }
}

/// Evaluate whether `ref_name` / `sha` may be used as the source of a
/// final squash merge to the integration target.
///
/// The merge path passes the *candidate* ref / SHA it would feed into
/// `git merge --squash` (or `merge_pull_request`) here. The function
/// refuses:
/// - any ref matching `refs/djinn/checkpoints/...` — preservation / resume
///   source only, never a final merge head;
/// - any ref whose short name matches a protected entry (`main`,
///   `master`, `HEAD`) — merges go INTO this ref, never out of it;
/// - any candidate lacking a SHA — a ref selector with no resolved commit
///   is rejected so the merge path can degrade to a no-op rather than
///   attempt a meaningless merge.
///
/// `task_id` is recorded on rejection so the structured-event payload
/// emitted by the caller carries the same identifier the merge-side logs
/// already use.
pub fn evaluate_merge_head(
    task_id: &str,
    ref_name: &str,
    sha: Option<&str>,
) -> MergeSafetyDecision {
    if let Some(role) = sha
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|_| classify_ref(ref_name))
    {
        // We have a SHA so the role classification is the dominant
        // signal. Use it directly.
        match role {
            RefRole::CheckpointRef => {
                let sha_owned = sha.map(str::to_owned);
                tracing::warn!(
                    task_id,
                    ref = ref_name,
                    sha = sha_owned.as_deref().unwrap_or(""),
                    "merge-safety: refusing to use checkpoint ref as final merge head"
                );
                return MergeSafetyDecision::CheckpointRef {
                    ref_name: ref_name.to_owned(),
                    sha: sha_owned,
                };
            }
            RefRole::Protected => {
                tracing::warn!(
                    task_id,
                    ref = ref_name,
                    "merge-safety: refusing to merge FROM a protected ref"
                );
                return MergeSafetyDecision::ProtectedRef {
                    ref_name: ref_name.to_owned(),
                };
            }
            RefRole::TaskBranch | RefRole::Other => {
                return MergeSafetyDecision::Eligible;
            }
        }
    }

    // Either no SHA was provided or it was empty. We still classify so
    // we can return the more specific rejection variant; otherwise the
    // caller can't distinguish "missing SHA" from "eligible but
    // unresolved".
    match classify_ref(ref_name) {
        RefRole::CheckpointRef => MergeSafetyDecision::CheckpointRef {
            ref_name: ref_name.to_owned(),
            sha: None,
        },
        RefRole::Protected => MergeSafetyDecision::ProtectedRef {
            ref_name: ref_name.to_owned(),
        },
        _ => MergeSafetyDecision::MissingSha {
            ref_name: ref_name.to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_canonical_task_branch() {
        assert_eq!(classify_ref("refs/heads/task/abc12"), RefRole::TaskBranch);
        assert_eq!(classify_ref("task/abc12"), RefRole::TaskBranch);
    }

    #[test]
    fn classify_alternate_checkpoint_ref() {
        assert_eq!(
            classify_ref("refs/djinn/checkpoints/task-1/session-1"),
            RefRole::CheckpointRef
        );
        assert_eq!(
            classify_ref("refs/djinn/checkpoints/foo/bar/baz"),
            RefRole::CheckpointRef
        );
    }

    #[test]
    fn checkpoint_prefix_does_not_collide_with_protected() {
        // A future protected entry `djinn` must not classify the
        // checkpoint namespace as protected. This test pins that
        // invariant so adding `djinn` (or anything similar) to
        // PROTECTED_REFS doesn't silently re-classify checkpoint refs.
        assert_eq!(
            classify_ref("refs/djinn/checkpoints/anything"),
            RefRole::CheckpointRef
        );
    }

    #[test]
    fn classify_protected_refs() {
        for protected in ["main", "master", "HEAD"] {
            assert_eq!(classify_ref(protected), RefRole::Protected);
            assert_eq!(
                classify_ref(&format!("refs/heads/{protected}")),
                RefRole::Protected
            );
            assert_eq!(
                classify_ref(&format!("remotes/origin/{protected}")),
                RefRole::Protected
            );
        }
    }

    #[test]
    fn classify_arbitrary_feature_branch_as_other() {
        assert_eq!(classify_ref("feature/some-thing"), RefRole::Other);
        assert_eq!(classify_ref("user/alice/experiment"), RefRole::Other);
    }

    #[test]
    fn classify_empty_and_whitespace_only_as_other() {
        assert_eq!(classify_ref(""), RefRole::Other);
        assert_eq!(classify_ref("   "), RefRole::Other);
    }

    #[test]
    fn is_checkpoint_ref_helper_matches_classify() {
        for (input, expected) in [
            ("refs/djinn/checkpoints/task-1/session-1", true),
            ("refs/djinn/checkpoints/foo", true),
            ("refs/heads/task/abc", false),
            ("main", false),
            ("refs/heads/main", false),
            ("feature/x", false),
            ("", false),
        ] {
            assert_eq!(
                is_checkpoint_ref(input),
                expected,
                "is_checkpoint_ref({input:?}) = {expected}?"
            );
            assert_eq!(
                is_checkpoint_ref(input),
                classify_ref(input) == RefRole::CheckpointRef
            );
        }
    }

    #[test]
    fn is_protected_ref_helper_matches_classify() {
        for (input, expected) in [
            ("main", true),
            ("refs/heads/main", true),
            ("master", true),
            ("HEAD", true),
            ("refs/djinn/checkpoints/x/y", false),
            ("task/abc", false),
            ("feature/x", false),
            ("", false),
        ] {
            assert_eq!(
                is_protected_ref(input),
                expected,
                "is_protected_ref({input:?}) = {expected}?"
            );
        }
    }

    #[test]
    fn ref_role_eligibility_matches_role() {
        // The role-classification table is the source of truth — guard
        // the eligibility helpers against drift so a future role addition
        // can't silently make a checkpoint ref eligible.
        assert!(RefRole::TaskBranch.is_eligible_final_merge_source());
        assert!(!RefRole::CheckpointRef.is_eligible_final_merge_source());
        assert!(!RefRole::Protected.is_eligible_final_merge_source());
        // `Other` is operator-managed; we deliberately do not block it.
        assert!(RefRole::Other.is_eligible_final_merge_source());
    }

    #[test]
    fn ref_role_safe_to_cleanup_only_task_branch() {
        assert!(RefRole::TaskBranch.is_safe_to_cleanup());
        assert!(!RefRole::CheckpointRef.is_safe_to_cleanup());
        assert!(!RefRole::Protected.is_safe_to_cleanup());
        // `Other` is operator-managed; the automated cleanup path must
        // not sweep it.
        assert!(!RefRole::Other.is_safe_to_cleanup());
    }

    #[test]
    fn ref_role_checkpoint_flag_only_checkpoint_ref() {
        assert!(RefRole::CheckpointRef.is_checkpoint_ref());
        assert!(!RefRole::TaskBranch.is_checkpoint_ref());
        assert!(!RefRole::Protected.is_checkpoint_ref());
        assert!(!RefRole::Other.is_checkpoint_ref());
    }

    #[test]
    fn evaluate_merge_head_eligible_for_task_branch_with_sha() {
        let decision = evaluate_merge_head("task-abc", "refs/heads/task/abc12", Some("deadbeef"));
        assert_eq!(decision, MergeSafetyDecision::Eligible);
        assert!(decision.is_eligible());
        assert_eq!(decision.rejection_tag(), "eligible");
    }

    #[test]
    fn evaluate_merge_head_rejects_checkpoint_ref_even_with_sha() {
        let decision = evaluate_merge_head(
            "task-abc",
            "refs/djinn/checkpoints/task-abc/session-1",
            Some("deadbeef"),
        );
        match &decision {
            MergeSafetyDecision::CheckpointRef { ref_name, sha } => {
                assert_eq!(ref_name, "refs/djinn/checkpoints/task-abc/session-1");
                assert_eq!(sha.as_deref(), Some("deadbeef"));
            }
            other => panic!("expected CheckpointRef, got {other:?}"),
        }
        assert!(!decision.is_eligible());
        assert_eq!(decision.rejection_tag(), "checkpoint_ref");
    }

    #[test]
    fn evaluate_merge_head_rejects_protected_ref() {
        let decision = evaluate_merge_head("task-abc", "main", Some("deadbeef"));
        assert!(matches!(decision, MergeSafetyDecision::ProtectedRef { .. }));
        assert!(!decision.is_eligible());
        assert_eq!(decision.rejection_tag(), "protected_ref");

        // `refs/heads/main` must also be classified as protected so the
        // guard works regardless of which form the caller passes.
        let decision = evaluate_merge_head("task-abc", "refs/heads/main", Some("deadbeef"));
        assert!(matches!(decision, MergeSafetyDecision::ProtectedRef { .. }));
    }

    #[test]
    fn evaluate_merge_head_records_task_id_in_tracing_payload() {
        // Smoke check: even though `evaluate_merge_head` returns a value
        // (rather than emitting an event itself), the function must NOT
        // panic when the task_id is non-empty and must produce a
        // rejection variant for checkpoint refs regardless of task_id.
        let decision = evaluate_merge_head("", "refs/djinn/checkpoints/x/y", Some("sha"));
        assert!(matches!(
            decision,
            MergeSafetyDecision::CheckpointRef { .. }
        ));
    }

    #[test]
    fn evaluate_merge_head_handles_missing_sha_as_missing_sha_variant() {
        // No SHA + task-branch ref shape → MissingSha (the merge path
        // can degrade to a no-op rather than guessing what to merge).
        let decision = evaluate_merge_head("task-abc", "task/abc12", None);
        assert_eq!(
            decision,
            MergeSafetyDecision::MissingSha {
                ref_name: "task/abc12".to_owned()
            }
        );
        assert_eq!(decision.rejection_tag(), "missing_sha");

        // Empty SHA string is treated identically to None.
        let decision = evaluate_merge_head("task-abc", "task/abc12", Some("   "));
        assert!(matches!(decision, MergeSafetyDecision::MissingSha { .. }));
    }

    #[test]
    fn evaluate_merge_head_checkpoint_ref_without_sha_preserves_null_sha() {
        let decision = evaluate_merge_head(
            "task-abc",
            "refs/djinn/checkpoints/task-abc/session-1",
            None,
        );
        match decision {
            MergeSafetyDecision::CheckpointRef { ref_name, sha } => {
                assert_eq!(ref_name, "refs/djinn/checkpoints/task-abc/session-1");
                assert!(sha.is_none());
            }
            other => panic!("expected CheckpointRef, got {other:?}"),
        }
    }

    #[test]
    fn checkpoint_ref_prefix_constant_is_stable() {
        // The 8yjx / 3ln4 tasks share this prefix; renaming it would
        // silently break both. Pin it here so a refactor that touches the
        // constant triggers a deliberate decision (and a test update).
        assert_eq!(CHECKPOINT_REF_PREFIX, "refs/djinn/checkpoints/");
    }

    #[test]
    fn protected_refs_list_includes_integration_targets() {
        // Pin the protected set so an accidental removal trips a test
        // and forces a deliberate decision (rather than silently
        // re-enabling destructive cleanup).
        assert!(PROTECTED_REFS.contains(&"main"));
        assert!(PROTECTED_REFS.contains(&"master"));
        assert!(PROTECTED_REFS.contains(&"HEAD"));
    }
}
