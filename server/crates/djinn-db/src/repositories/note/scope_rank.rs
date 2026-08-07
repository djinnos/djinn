//! Deterministic ranked-scope signal for knowledge injection (proposal `5205`).
//!
//! This module is intentionally free of database access so the whole ranking
//! contract — component comparability, minimum distance, best-pair
//! aggregation, candidate universe, sort-then-truncate, and note-ID ties — is
//! exercised by ordinary unit tests rather than by a live Postgres fixture.
//! The repository method that owns the SQL universe delegates every scoring
//! decision here.

/// A scope candidate: a note ID and the note's stored `scope_paths`.
#[derive(Debug, Clone)]
pub struct ScopeCandidate {
    pub note_id: String,
    pub scope_paths: Vec<String>,
}

/// Split a path into non-empty components after separator normalization.
///
/// Comparison is component-based: raw string prefixes never match, so `src/a`
/// is not an ancestor of `src/ab`.
fn components(path: &str) -> Vec<&str> {
    path.split('/').filter(|part| !part.is_empty()).collect()
}

/// Component distance between a task path and a note scope path.
///
/// * `Some(0)` when the two paths are equal.
/// * `Some(n)` when either is a component ancestor of the other, where `n` is
///   the absolute difference in component count.
/// * `None` when the pair is not comparable.
pub fn component_distance(left: &str, right: &str) -> Option<usize> {
    let left = components(left);
    let right = components(right);
    if left.is_empty() || right.is_empty() {
        return None;
    }
    let shared = left.len().min(right.len());
    if left[..shared] != right[..shared] {
        return None;
    }
    Some(left.len().abs_diff(right.len()))
}

/// The scope score for a note: `1 / (1 + min_distance)` over every comparable
/// task-path/note-path pair, or `None` when no pair is comparable.
///
/// Best-pair aggregation: additional weaker paths never dilute the best match.
pub fn best_pair_score(task_paths: &[String], note_scope_paths: &[String]) -> Option<f64> {
    let mut best: Option<usize> = None;
    for task_path in task_paths {
        for note_path in note_scope_paths {
            if let Some(distance) = component_distance(task_path, note_path) {
                best = Some(match best {
                    Some(current) => current.min(distance),
                    None => distance,
                });
            }
        }
    }
    best.map(|distance| 1.0 / (1.0 + distance as f64))
}

/// Rank the scope-signal candidate universe.
///
/// The candidate universe is every supplied note whose normalized
/// `scope_paths` contains at least one path component-prefix-comparable with a
/// validated task path. A global note (no scope path) and an unrelated scoped
/// note are absent from this list; they may still enter fusion through another
/// signal.
///
/// Sorting is by score descending then note ID ascending. Truncation to
/// `window` happens **after** sorting, so a high-scoring note can never be cut
/// by input order. No recency field participates.
pub fn rank_scope_candidates(
    task_paths: &[String],
    candidates: &[ScopeCandidate],
    window: usize,
) -> Vec<(String, f64)> {
    if task_paths.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(String, f64)> = candidates
        .iter()
        .filter_map(|candidate| {
            best_pair_score(task_paths, &candidate.scope_paths)
                .map(|score| (candidate.note_id.clone(), score))
        })
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    scored.truncate(window);
    scored
}

#[cfg(test)]
mod tests {
    use super::{ScopeCandidate, best_pair_score, component_distance, rank_scope_candidates};

    fn candidate(note_id: &str, scope_paths: &[&str]) -> ScopeCandidate {
        ScopeCandidate {
            note_id: note_id.to_owned(),
            scope_paths: scope_paths.iter().map(|p| (*p).to_owned()).collect(),
        }
    }

    fn paths(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_owned()).collect()
    }

    fn ids(ranked: &[(String, f64)]) -> Vec<&str> {
        ranked.iter().map(|(id, _)| id.as_str()).collect()
    }

    #[test]
    fn equal_paths_have_distance_zero() {
        assert_eq!(component_distance("src/a/b", "src/a/b"), Some(0));
    }

    #[test]
    fn ancestor_distance_is_absolute_component_difference() {
        assert_eq!(component_distance("src/a/b/c", "src/a"), Some(2));
        // Symmetric: the note may be *below* the touched directory.
        assert_eq!(component_distance("src/a", "src/a/b/c"), Some(2));
    }

    #[test]
    fn false_component_prefix_is_not_comparable() {
        // The whole point of component comparability: `src/a` is a raw string
        // prefix of `src/ab` but is not its ancestor.
        assert_eq!(component_distance("src/a", "src/ab"), None);
        assert_eq!(component_distance("src/ab", "src/a"), None);
        assert_eq!(
            best_pair_score(&paths(&["src/a"]), &paths(&["src/ab"])),
            None
        );
    }

    #[test]
    fn unrelated_paths_are_not_comparable() {
        assert_eq!(component_distance("server/crates/db", "ui/src"), None);
    }

    #[test]
    fn repeated_separators_do_not_change_comparability() {
        assert_eq!(component_distance("src//a///b", "src/a/b"), Some(0));
    }

    #[test]
    fn best_pair_wins_over_additional_weaker_paths() {
        // The note carries one exact match and one very coarse match. The
        // exact match must set the score; the coarse one must not dilute it.
        let score = best_pair_score(
            &paths(&["server/crates/djinn-db/src/lib.rs"]),
            &paths(&["server/crates/djinn-db/src/lib.rs", "server"]),
        );
        assert_eq!(score, Some(1.0));
    }

    #[test]
    fn multi_task_path_best_pair_wins() {
        let score = best_pair_score(
            &paths(&["ui/src", "server/crates/djinn-db/src/lib.rs"]),
            &paths(&["server/crates/djinn-db/src/lib.rs"]),
        );
        assert_eq!(score, Some(1.0));
    }

    #[test]
    fn score_ladder_exact_then_nearest_then_coarse() {
        assert_eq!(
            best_pair_score(&paths(&["a/b/c/d"]), &paths(&["a/b/c/d"])),
            Some(1.0)
        );
        assert_eq!(
            best_pair_score(&paths(&["a/b/c/d"]), &paths(&["a/b/c"])),
            Some(0.5)
        );
        assert_eq!(
            best_pair_score(&paths(&["a/b/c/d"]), &paths(&["a/b"])),
            Some(1.0 / 3.0)
        );
    }

    /// The complete ordering fixture AC3 enumerates: exact file, nearest
    /// ancestor, coarse ancestor, directory descendant, unrelated scoped,
    /// global, multi-path, component-boundary false prefix, and ties.
    #[test]
    fn ranked_universe_orders_exact_then_ancestors_and_excludes_non_members() {
        let task_paths = paths(&["server/crates/djinn-db/src/lib.rs", "ui/components"]);
        let candidates = vec![
            candidate("n-exact", &["server/crates/djinn-db/src/lib.rs"]),
            candidate("n-nearest", &["server/crates/djinn-db/src"]),
            candidate("n-coarse", &["server/crates"]),
            candidate("n-descendant", &["ui/components/button/index.tsx"]),
            candidate("n-unrelated", &["website/docs"]),
            candidate("n-global", &[]),
            candidate(
                "n-multi",
                &["website/docs", "server/crates/djinn-db/src/lib.rs"],
            ),
            // `server/crates/djinn-d` is a raw string prefix of
            // `server/crates/djinn-db` but not a component ancestor.
            candidate("n-false-prefix", &["server/crates/djinn-d"]),
        ];

        let ranked = rank_scope_candidates(&task_paths, &candidates, 50);

        assert_eq!(
            ids(&ranked),
            vec![
                // score 1.0 (distance 0), tied, ordered by note ID ascending
                "n-exact",
                "n-multi",
                // score 0.5 — nearest ancestor, one component away
                "n-nearest",
                // score 1/3 — descendant two components below `ui/components`
                "n-descendant",
                // score 0.25 — coarse ancestor, three components away
                "n-coarse",
            ],
            "global, unrelated, and false-prefix notes are not in this signal"
        );
        assert_eq!(ranked[0].1, 1.0);
        assert_eq!(ranked[1].1, 1.0);
        assert_eq!(ranked[2].1, 0.5);
        assert_eq!(ranked[3].1, 1.0 / 3.0);
        assert_eq!(ranked[4].1, 0.25);
    }

    #[test]
    fn ties_are_broken_by_note_id_ascending_regardless_of_input_order() {
        let task_paths = paths(&["src/app"]);
        let forward = vec![
            candidate("bbb", &["src/app"]),
            candidate("aaa", &["src/app"]),
            candidate("ccc", &["src/app"]),
        ];
        let reversed: Vec<_> = forward.iter().rev().cloned().collect();
        assert_eq!(
            ids(&rank_scope_candidates(&task_paths, &forward, 50)),
            vec!["aaa", "bbb", "ccc"]
        );
        assert_eq!(
            ids(&rank_scope_candidates(&task_paths, &reversed, 50)),
            vec!["aaa", "bbb", "ccc"]
        );
    }

    #[test]
    fn truncation_happens_after_sorting() {
        // The single best note is supplied last. A truncate-then-sort
        // implementation would drop it; sort-then-truncate must keep it.
        let task_paths = paths(&["a/b/c"]);
        let mut candidates: Vec<ScopeCandidate> = (0..60)
            .map(|index| candidate(&format!("coarse-{index:03}"), &["a"]))
            .collect();
        candidates.push(candidate("zzz-exact", &["a/b/c"]));

        let ranked = rank_scope_candidates(&task_paths, &candidates, 50);
        assert_eq!(ranked.len(), 50);
        assert_eq!(ranked[0].0, "zzz-exact");
        assert_eq!(ranked[0].1, 1.0);
    }

    #[test]
    fn empty_task_paths_produce_an_empty_signal() {
        let candidates = vec![candidate("n-any", &["src/app"])];
        assert!(rank_scope_candidates(&[], &candidates, 50).is_empty());
    }

    #[test]
    fn global_notes_are_never_members_of_the_scope_signal() {
        let candidates = vec![candidate("n-global", &[])];
        assert!(rank_scope_candidates(&paths(&["src/app"]), &candidates, 50).is_empty());
    }
}
