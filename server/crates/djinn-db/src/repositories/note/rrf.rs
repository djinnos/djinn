use std::collections::HashMap;

/// Fixed candidate window used by [`RankingProfile::KnowledgeInjectionV1`].
///
/// Proposal `5205`: lexical, embedding, temporal, graph, task-affinity, and
/// validated-scope retrieval each request and retain at most this many
/// *eligible* notes before fusion, and the fused list returned to packing is
/// truncated to the same bound. `top_k` affects [`injection_rrf_k`] and packing
/// but never this window.
pub const KNOWLEDGE_INJECTION_CANDIDATE_WINDOW: usize = 50;

/// Ranking configuration selected at the search/fusion boundary.
///
/// `Default` is behaviour-identical to the pre-`5205` fusion: the caller's `k`
/// is used verbatim and the fused score is multiplied by the raw confidence.
/// Every caller other than knowledge injection stays on `Default`; changing its
/// semantics is forbidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RankingProfile {
    /// Pre-existing behaviour. Confidence multiplies the fused score directly.
    #[default]
    Default,
    /// Injection-only profile (proposal `5205`). List-size-aware `k`, bounded
    /// confidence influence, fixed 50-note windows, note-ID tie-break.
    KnowledgeInjectionV1,
}

impl RankingProfile {
    /// Stable identifier recorded in retrieval traces.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::KnowledgeInjectionV1 => "knowledge_injection_v1",
        }
    }

    /// The multiplier applied to a candidate's fused score for `confidence`.
    ///
    /// `Default` keeps the historical raw multiplication (an annihilating
    /// multiplier at low confidence). `KnowledgeInjectionV1` maps confidence
    /// onto `[0.75, 1.25]` so confidence remains a modest prior and a uniform
    /// 1.0 corpus becomes a constant factor that cannot reorder anything.
    pub fn confidence_multiplier(self, confidence: f64) -> f64 {
        match self {
            Self::Default => confidence,
            Self::KnowledgeInjectionV1 => 0.75 + 0.5 * confidence.clamp(0.0, 1.0),
        }
    }
}

/// The RRF `k` every contributing list uses under
/// [`RankingProfile::KnowledgeInjectionV1`] for a requested injected cutoff
/// `top_k`: `clamp(20 + 2 * top_k, 30, 60)`.
///
/// Saturating arithmetic keeps an absurd configured `top_k` from wrapping; the
/// upper clamp makes the result 60 in that case anyway.
pub fn injection_rrf_k(top_k: usize) -> f64 {
    let raw = 20usize.saturating_add(top_k.saturating_mul(2));
    raw.clamp(30, 60) as f64
}

/// Per-signal 1-based ranks for one fusion run, in the caller's signal order.
///
/// A note absent from a signal's list has no entry for that signal. This is the
/// trace-facing companion to [`rrf_fuse_with_profile`]; it is derived from the
/// same sorted lists the fusion consumes, never recomputed separately.
pub type SignalRankMaps = Vec<HashMap<String, usize>>;

pub fn rrf_fuse(
    signals: &[(Vec<(String, f64)>, f64)],
    confidence_map: &HashMap<String, f64>,
) -> Vec<(String, f64)> {
    rrf_fuse_with_profile(signals, confidence_map, RankingProfile::Default)
}

/// Profile-aware fusion. See [`RankingProfile`].
pub fn rrf_fuse_with_profile(
    signals: &[(Vec<(String, f64)>, f64)],
    confidence_map: &HashMap<String, f64>,
    profile: RankingProfile,
) -> Vec<(String, f64)> {
    rrf_fuse_with_ranks(signals, confidence_map, profile).0
}

/// Fusion that additionally reports each signal's 1-based ranks.
pub fn rrf_fuse_with_ranks(
    signals: &[(Vec<(String, f64)>, f64)],
    confidence_map: &HashMap<String, f64>,
    profile: RankingProfile,
) -> (Vec<(String, f64)>, SignalRankMaps) {
    if signals.is_empty() {
        return (Vec::new(), Vec::new());
    }

    let mut all_note_ids: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for (ranked_list, _) in signals {
        for (id, _) in ranked_list {
            if seen.insert(id.clone()) {
                all_note_ids.push(id.clone());
            }
        }
    }

    if all_note_ids.is_empty() {
        return (Vec::new(), Vec::new());
    }

    let mut fused_scores: HashMap<String, f64> =
        all_note_ids.iter().map(|id| (id.clone(), 0.0)).collect();
    let mut signal_ranks: SignalRankMaps = Vec::with_capacity(signals.len());

    for (ranked_list, k) in signals {
        let mut sorted = ranked_list.clone();
        sorted.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        let mut ranks: HashMap<String, usize> = HashMap::new();
        for (idx, (id, _)) in sorted.iter().enumerate() {
            ranks.insert(id.clone(), idx + 1);
        }

        let missing_rank = sorted.len() + 1;
        for id in &all_note_ids {
            let rank = ranks.get(id).copied().unwrap_or(missing_rank);
            let contribution = 1.0 / (*k + rank as f64);
            *fused_scores.get_mut(id).expect("candidate should exist") += contribution;
        }
        signal_ranks.push(ranks);
    }

    let mut fused: Vec<(String, f64)> = fused_scores
        .into_iter()
        .map(|(id, score)| {
            let confidence = confidence_map.get(&id).copied().unwrap_or(1.0);
            (id, score * profile.confidence_multiplier(confidence))
        })
        .collect();

    // Equal final scores are ordered by note ID ascending, never `updated_at`.
    fused.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    (fused, signal_ranks)
}

#[cfg(test)]
mod tests {
    use super::rrf_fuse;
    use std::collections::HashMap;

    #[test]
    fn one_signal_preserves_order() {
        let signals = vec![(
            vec![
                ("note-a".to_string(), 3.0),
                ("note-b".to_string(), 2.0),
                ("note-c".to_string(), 1.0),
            ],
            60.0,
        )];

        let fused = rrf_fuse(&signals, &HashMap::new());
        assert_eq!(
            fused.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            vec!["note-a", "note-b", "note-c"]
        );
    }

    #[test]
    fn absent_note_gets_non_zero_contribution() {
        let signals = vec![
            (
                vec![("note-a".to_string(), 2.0), ("note-b".to_string(), 1.0)],
                60.0,
            ),
            (vec![("note-b".to_string(), 5.0)], 80.0),
        ];

        let fused = rrf_fuse(&signals, &HashMap::new());
        let score_a = fused.iter().find(|(id, _)| id == "note-a").unwrap().1;
        assert!(score_a > 0.0);
    }

    #[test]
    fn confidence_scales_final_score() {
        let signals = vec![(
            vec![("note-a".to_string(), 10.0), ("note-b".to_string(), 9.0)],
            60.0,
        )];

        let mut confidence_map = HashMap::new();
        confidence_map.insert("note-a".to_string(), 0.5);
        confidence_map.insert("note-b".to_string(), 1.0);

        let fused = rrf_fuse(&signals, &confidence_map);
        assert_eq!(fused[0].0, "note-b");
    }

    #[test]
    fn two_signal_three_note_expected_fused_order() {
        let signals = vec![
            (
                vec![
                    ("note-a".to_string(), 3.0),
                    ("note-b".to_string(), 2.0),
                    ("note-c".to_string(), 1.0),
                ],
                60.0,
            ),
            (
                vec![
                    ("note-c".to_string(), 3.0),
                    ("note-b".to_string(), 2.0),
                    ("note-a".to_string(), 1.0),
                ],
                60.0,
            ),
        ];

        let fused = rrf_fuse(&signals, &HashMap::new());
        assert_eq!(
            fused.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            vec!["note-a", "note-c", "note-b"]
        );
    }

    #[test]
    fn empty_signal_list_returns_empty() {
        let fused = rrf_fuse(&[], &HashMap::new());
        assert!(fused.is_empty());
    }

    // ── Proposal 5205: ranking-profile compatibility boundary ──────────────
    //
    // `legacy_rrf_fuse` below is an *independent* transcription of the fusion
    // that shipped before `RankingProfile` existed. AC5 requires ordinary
    // search and `run_rrf_discovery` to produce identical ordered note IDs and
    // scores under `Default`; comparing production against a re-derived
    // reference is what makes that a real check rather than a tautology.

    use super::{
        KNOWLEDGE_INJECTION_CANDIDATE_WINDOW, RankingProfile, injection_rrf_k,
        rrf_fuse_with_profile, rrf_fuse_with_ranks,
    };

    /// Pre-5205 fusion, transcribed from the shipped implementation.
    fn legacy_rrf_fuse(
        signals: &[(Vec<(String, f64)>, f64)],
        confidence_map: &HashMap<String, f64>,
    ) -> Vec<(String, f64)> {
        if signals.is_empty() {
            return Vec::new();
        }
        let mut all_note_ids: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (ranked_list, _) in signals {
            for (id, _) in ranked_list {
                if seen.insert(id.clone()) {
                    all_note_ids.push(id.clone());
                }
            }
        }
        if all_note_ids.is_empty() {
            return Vec::new();
        }
        let mut fused_scores: HashMap<String, f64> =
            all_note_ids.iter().map(|id| (id.clone(), 0.0)).collect();
        for (ranked_list, k) in signals {
            let mut sorted = ranked_list.clone();
            sorted.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            let mut ranks: HashMap<String, usize> = HashMap::new();
            for (idx, (id, _)) in sorted.iter().enumerate() {
                ranks.insert(id.clone(), idx + 1);
            }
            let missing_rank = sorted.len() + 1;
            for id in &all_note_ids {
                let rank = ranks.get(id).copied().unwrap_or(missing_rank);
                *fused_scores.get_mut(id).expect("candidate") += 1.0 / (*k + rank as f64);
            }
        }
        let mut fused: Vec<(String, f64)> = fused_scores
            .into_iter()
            .map(|(id, score)| {
                let confidence = confidence_map.get(&id).copied().unwrap_or(1.0);
                (id, score * confidence)
            })
            .collect();
        fused.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        fused
    }

    fn list(entries: &[(&str, f64)]) -> Vec<(String, f64)> {
        entries
            .iter()
            .map(|(id, score)| ((*id).to_owned(), *score))
            .collect()
    }

    /// The five-signal, k=60 shape `search_notes_inner` builds, plus the
    /// four-signal shape `run_rrf_discovery` builds, with a non-uniform
    /// confidence map so the multiplier actually participates.
    fn representative_default_fixtures()
    -> Vec<(Vec<(Vec<(String, f64)>, f64)>, HashMap<String, f64>)> {
        let mut confidence = HashMap::new();
        confidence.insert("n-alpha".to_owned(), 0.42);
        confidence.insert("n-bravo".to_owned(), 1.0);
        confidence.insert("n-charlie".to_owned(), 0.9);
        confidence.insert("n-delta".to_owned(), 0.05);

        // Ordinary `search_notes_inner`: lexical, semantic, temporal, graph,
        // task-affinity — every list at k = 60.
        let ordinary_search = vec![
            (
                list(&[("n-alpha", 2.5), ("n-bravo", 1.5), ("n-charlie", 0.5)]),
                60.0,
            ),
            (list(&[("n-charlie", 0.91), ("n-alpha", 0.4)]), 60.0),
            (
                list(&[("n-delta", 7.0), ("n-bravo", 3.0), ("n-alpha", 1.0)]),
                60.0,
            ),
            (list(&[("n-bravo", 0.8)]), 60.0),
            (list(&[]), 60.0),
        ];

        // `run_rrf_discovery`: fts, temporal, graph, task-affinity at k = 60.
        let discovery = vec![
            (list(&[("n-bravo", 4.0), ("n-delta", 2.0)]), 60.0),
            (
                list(&[
                    ("n-alpha", 0.3),
                    ("n-bravo", 0.2),
                    ("n-charlie", 0.1),
                    ("n-delta", 0.05),
                ]),
                60.0,
            ),
            (list(&[("n-charlie", 1.2), ("n-alpha", 0.9)]), 60.0),
            (list(&[("n-delta", 5.0)]), 60.0),
        ];

        vec![
            (ordinary_search, confidence.clone()),
            (discovery, confidence),
        ]
    }

    #[test]
    fn default_profile_reproduces_legacy_ordering_and_scores_bitwise() {
        for (index, (signals, confidence)) in
            representative_default_fixtures().into_iter().enumerate()
        {
            let legacy = legacy_rrf_fuse(&signals, &confidence);
            let current = rrf_fuse_with_profile(&signals, &confidence, RankingProfile::Default);
            assert_eq!(
                legacy.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
                current.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
                "fixture {index}: Default must preserve ordered note IDs"
            );
            for (expected, actual) in legacy.iter().zip(current.iter()) {
                assert_eq!(
                    expected.1.to_bits(),
                    actual.1.to_bits(),
                    "fixture {index}: Default must preserve the exact fused score of {}",
                    expected.0
                );
            }
        }
    }

    #[test]
    fn compat_wrapper_matches_explicit_default_profile() {
        for (signals, confidence) in representative_default_fixtures() {
            let wrapper = rrf_fuse(&signals, &confidence);
            let explicit = rrf_fuse_with_profile(&signals, &confidence, RankingProfile::Default);
            assert_eq!(wrapper.len(), explicit.len());
            for (a, b) in wrapper.iter().zip(explicit.iter()) {
                assert_eq!(a.0, b.0);
                assert_eq!(a.1.to_bits(), b.1.to_bits());
            }
        }
    }

    #[test]
    fn injection_profile_bounds_confidence_and_default_does_not() {
        // A 0.05-confidence note is annihilated under `Default` (×0.05) but
        // only mildly demoted under the injection profile (×0.775).
        assert_eq!(RankingProfile::Default.confidence_multiplier(0.05), 0.05);
        assert_eq!(
            RankingProfile::KnowledgeInjectionV1.confidence_multiplier(0.05),
            0.775
        );
        assert_eq!(
            RankingProfile::KnowledgeInjectionV1.confidence_multiplier(0.0),
            0.75
        );
        assert_eq!(
            RankingProfile::KnowledgeInjectionV1.confidence_multiplier(1.0),
            1.25
        );
        // Out-of-range confidences clamp rather than blow the bound.
        assert_eq!(
            RankingProfile::KnowledgeInjectionV1.confidence_multiplier(9.0),
            1.25
        );
        assert_eq!(
            RankingProfile::KnowledgeInjectionV1.confidence_multiplier(-3.0),
            0.75
        );
    }

    #[test]
    fn injection_profile_reorders_where_default_annihilates() {
        // `weak` is rank 1 in both lists, `strong` is rank 30 in both, with 28
        // fillers between them. Under `Default` a 0.05 confidence annihilates
        // the far better relevance; under the injection profile the bounded
        // prior cannot overcome a 29-rank gap. This asserts the *ordering
        // consequence*, not that a profile value was threaded through.
        let mut ranked: Vec<(String, f64)> = vec![("weak".to_owned(), 100.0)];
        for index in 0..28 {
            ranked.push((format!("filler-{index:02}"), 50.0 - index as f64));
        }
        ranked.push(("strong".to_owned(), 1.0));
        let k = injection_rrf_k(0);
        let signals = vec![(ranked.clone(), k), (ranked, k)];

        let mut confidence = HashMap::new();
        confidence.insert("weak".to_owned(), 0.05);
        confidence.insert("strong".to_owned(), 1.0);

        let position = |fused: &[(String, f64)], id: &str| {
            fused
                .iter()
                .position(|(candidate, _)| candidate == id)
                .unwrap_or_else(|| panic!("{id} must be fused"))
        };

        let default = rrf_fuse_with_profile(&signals, &confidence, RankingProfile::Default);
        assert!(
            position(&default, "strong") < position(&default, "weak"),
            "Default lets a 0.05 confidence annihilate the far more relevant note"
        );

        let injection =
            rrf_fuse_with_profile(&signals, &confidence, RankingProfile::KnowledgeInjectionV1);
        assert!(
            position(&injection, "weak") < position(&injection, "strong"),
            "KnowledgeInjectionV1 lets relevance dominate a bounded confidence prior"
        );
    }

    #[test]
    fn uniform_confidence_cannot_reorder_under_injection_profile() {
        // The observed production corpus is all-1.0 confidence. The transform
        // must then be a constant factor: relative order equals the order with
        // no confidence map at all.
        let signals = vec![
            (list(&[("n-c", 3.0), ("n-a", 2.0), ("n-b", 1.0)]), 42.0),
            (list(&[("n-b", 9.0), ("n-c", 1.0)]), 42.0),
        ];
        let uniform: HashMap<String, f64> = ["n-a", "n-b", "n-c"]
            .iter()
            .map(|id| ((*id).to_owned(), 1.0))
            .collect();
        let with_uniform =
            rrf_fuse_with_profile(&signals, &uniform, RankingProfile::KnowledgeInjectionV1);
        let without = rrf_fuse_with_profile(
            &signals,
            &HashMap::new(),
            RankingProfile::KnowledgeInjectionV1,
        );
        assert_eq!(
            with_uniform
                .iter()
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>(),
            without.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn equal_scores_break_ties_by_note_id_ascending() {
        // Two notes with identical per-signal ranks in mirrored lists fuse to
        // exactly the same score; the surviving order must be by ID.
        let signals = vec![
            (list(&[("zzz", 1.0), ("aaa", 1.0)]), 60.0),
            (list(&[("aaa", 1.0), ("zzz", 1.0)]), 60.0),
        ];
        let fused = rrf_fuse_with_profile(
            &signals,
            &HashMap::new(),
            RankingProfile::KnowledgeInjectionV1,
        );
        assert_eq!(
            fused[0].1.to_bits(),
            fused[1].1.to_bits(),
            "scores must tie"
        );
        assert_eq!(fused[0].0, "aaa");
        assert_eq!(fused[1].0, "zzz");
    }

    #[test]
    fn injection_k_is_clamped_list_size_aware() {
        assert_eq!(injection_rrf_k(0), 30.0); // 20 → clamped up
        assert_eq!(injection_rrf_k(4), 30.0); // 28 → clamped up
        assert_eq!(injection_rrf_k(5), 30.0); // exactly 30
        assert_eq!(injection_rrf_k(10), 40.0); // 20 + 20
        assert_eq!(injection_rrf_k(20), 60.0); // exactly 60
        assert_eq!(injection_rrf_k(50), 60.0); // 120 → clamped down
        assert_eq!(injection_rrf_k(usize::MAX), 60.0); // saturating, then clamped
    }

    #[test]
    fn injection_k_changes_the_fused_order_versus_k_60() {
        // Smaller `k` sharpens the contribution gradient between adjacent
        // ranks. Fusing the same lists at k = 60 and at `injection_rrf_k(0)`
        // must therefore produce different fused scores — proof that `k`
        // reaches scoring rather than being decoration.
        let signals_k = |k: f64| {
            vec![
                (list(&[("sharp", 9.0), ("steady", 8.0)]), k),
                (list(&[("steady", 8.0), ("other", 1.0)]), k),
            ]
        };
        let at_60 = rrf_fuse_with_profile(
            &signals_k(60.0),
            &HashMap::new(),
            RankingProfile::KnowledgeInjectionV1,
        );
        let at_30 = rrf_fuse_with_profile(
            &signals_k(injection_rrf_k(0)),
            &HashMap::new(),
            RankingProfile::KnowledgeInjectionV1,
        );
        // Same participants either way…
        assert_eq!(at_60.len(), at_30.len());
        // …but the top score must differ, so `k` demonstrably reaches scoring.
        assert_ne!(
            at_60[0].1.to_bits(),
            at_30[0].1.to_bits(),
            "rrf_k must change fused scores"
        );
    }

    #[test]
    fn signal_ranks_report_one_based_positions_and_omit_absent_notes() {
        let signals = vec![
            (list(&[("b", 5.0), ("a", 1.0)]), 60.0),
            (list(&[("c", 2.0)]), 60.0),
        ];
        let (_fused, ranks) = rrf_fuse_with_ranks(
            &signals,
            &HashMap::new(),
            RankingProfile::KnowledgeInjectionV1,
        );
        assert_eq!(ranks.len(), 2);
        assert_eq!(ranks[0].get("b"), Some(&1));
        assert_eq!(ranks[0].get("a"), Some(&2));
        assert_eq!(ranks[0].get("c"), None, "absent from signal 0");
        assert_eq!(ranks[1].get("c"), Some(&1));
        assert_eq!(ranks[1].get("b"), None, "absent from signal 1");
    }

    #[test]
    fn candidate_window_is_fifty() {
        assert_eq!(KNOWLEDGE_INJECTION_CANDIDATE_WINDOW, 50);
    }
}
