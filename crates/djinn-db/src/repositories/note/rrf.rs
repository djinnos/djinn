use std::collections::HashMap;

#[cfg_attr(not(test), expect(dead_code, reason = "RRF integration call-sites are added in follow-up task"))]
pub fn rrf_fuse(
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

    let mut fused_scores: HashMap<String, f64> = all_note_ids
        .iter()
        .map(|id| (id.clone(), 0.0))
        .collect();

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
        assert_eq!(fused.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(), vec!["note-a", "note-b", "note-c"]);
    }

    #[test]
    fn absent_note_gets_non_zero_contribution() {
        let signals = vec![
            (
                vec![("note-a".to_string(), 2.0), ("note-b".to_string(), 1.0)],
                60.0,
            ),
            (
                vec![("note-b".to_string(), 5.0)],
                80.0,
            ),
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
}
