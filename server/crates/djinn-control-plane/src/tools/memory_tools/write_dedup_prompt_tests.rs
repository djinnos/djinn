#[cfg(test)]
mod tests {
    use djinn_memory::NoteDedupCandidate;

    use crate::tools::memory_tools::write_dedup_prompt::{
        MEMORY_WRITE_DEDUP_CANDIDATE_CONTENT_CHAR_CAP, parse_memory_write_dedup_decision,
        render_memory_write_dedup_prompt,
    };
    use crate::tools::memory_tools::write_dedup_types::{
        MemoryWriteDedupDecision, MemoryWriteDedupDecisionInput,
    };

    fn candidate(content: String) -> NoteDedupCandidate {
        NoteDedupCandidate {
            id: "note_123".to_string(),
            permalink: "patterns/candidate".to_string(),
            title: "Candidate".to_string(),
            folder: "patterns".to_string(),
            note_type: "pattern".to_string(),
            content,
            abstract_: Some("abstract metadata".to_string()),
            overview: Some("overview metadata".to_string()),
            score: 0.9,
        }
    }

    fn render(candidate: &NoteDedupCandidate) -> String {
        render_memory_write_dedup_prompt(&MemoryWriteDedupDecisionInput {
            project_path: "owner/repo",
            title: "Incoming",
            content: "Incoming body",
            note_type: "pattern",
            candidates: std::slice::from_ref(candidate),
        })
    }

    #[test]
    fn parses_reuse_decision() {
        let decision = parse_memory_write_dedup_decision(
            r#"{"action":"reuse_existing","candidate_id":"note_123"}"#,
        )
        .unwrap();

        assert_eq!(
            decision,
            MemoryWriteDedupDecision::ReuseExisting {
                candidate_id: "note_123".to_string()
            }
        );
    }

    #[test]
    fn parses_merge_decision() {
        let decision = parse_memory_write_dedup_decision(
            r#"{"action":"merge_into_existing","candidate_id":"note_123","merged_title":"Merged","merged_content":"Combined"}"#,
        )
        .unwrap();

        assert_eq!(
            decision,
            MemoryWriteDedupDecision::MergeIntoExisting {
                candidate_id: "note_123".to_string(),
                merged_title: "Merged".to_string(),
                merged_content: "Combined".to_string(),
            }
        );
    }

    #[test]
    fn parses_supersede_decision() {
        let decision = parse_memory_write_dedup_decision(
            r#"{"action":"supersede_existing","candidate_id":"note_123","reason":"More authoritative coverage"}"#,
        )
        .unwrap();

        assert_eq!(
            decision,
            MemoryWriteDedupDecision::SupersedeExisting {
                candidate_id: "note_123".to_string(),
                reason: "More authoritative coverage".to_string(),
            }
        );
    }

    #[test]
    fn supersede_requires_candidate_id() {
        let error = parse_memory_write_dedup_decision(
            r#"{"action":"supersede_existing","reason":"Better"}"#,
        )
        .unwrap_err();

        assert!(error.contains("supersede_existing requires candidate_id"));
    }

    #[test]
    fn supersede_rejects_empty_candidate_id() {
        let error = parse_memory_write_dedup_decision(
            r#"{"action":"supersede_existing","candidate_id":"","reason":"Better"}"#,
        )
        .unwrap_err();

        assert!(error.contains("supersede_existing requires candidate_id"));
    }

    #[test]
    fn supersede_requires_reason() {
        let error = parse_memory_write_dedup_decision(
            r#"{"action":"supersede_existing","candidate_id":"note_123"}"#,
        )
        .unwrap_err();

        assert!(error.contains("supersede_existing requires reason"));
    }

    #[test]
    fn supersede_rejects_empty_reason() {
        let error = parse_memory_write_dedup_decision(
            r#"{"action":"supersede_existing","candidate_id":"note_123","reason":""}"#,
        )
        .unwrap_err();

        assert!(error.contains("supersede_existing requires reason"));
    }

    #[test]
    fn supersede_rejects_non_string_candidate_id() {
        let error = parse_memory_write_dedup_decision(
            r#"{"action":"supersede_existing","candidate_id":123,"reason":"Better"}"#,
        )
        .unwrap_err();

        assert!(error.contains("failed to parse dedup decision JSON"));
    }

    #[test]
    fn supersede_rejects_non_string_reason() {
        let error = parse_memory_write_dedup_decision(
            r#"{"action":"supersede_existing","candidate_id":"note_123","reason":123}"#,
        )
        .unwrap_err();

        assert!(error.contains("failed to parse dedup decision JSON"));
    }

    #[test]
    fn supersede_rejects_malformed_json() {
        let error = parse_memory_write_dedup_decision(
            r#"{"action":"supersede_existing","candidate_id":"note_123","reason"}"#,
        )
        .unwrap_err();

        assert!(error.contains("failed to parse dedup decision JSON"));
    }

    #[test]
    fn renders_full_candidate_body_below_cap_with_summary_as_metadata() {
        let prompt = render(&candidate("Authoritative candidate body.".to_string()));

        assert!(prompt.contains("  body:\nAuthoritative candidate body."));
        assert!(prompt.contains("  summary: overview metadata"));
        assert!(!prompt.contains("  body:\noverview metadata"));
    }

    #[test]
    fn truncates_candidate_body_deterministically_at_named_cap() {
        let content = "a".repeat(MEMORY_WRITE_DEDUP_CANDIDATE_CONTENT_CHAR_CAP + 10);
        let prompt = render(&candidate(content));
        let expected_body = "a".repeat(MEMORY_WRITE_DEDUP_CANDIDATE_CONTENT_CHAR_CAP);

        assert!(prompt.contains(&format!(
            "  body:\n{expected_body}\n… [truncated at {MEMORY_WRITE_DEDUP_CANDIDATE_CONTENT_CHAR_CAP} characters]"
        )));
    }

    #[test]
    fn truncates_candidate_body_on_unicode_character_boundary() {
        let content = format!(
            "{}Z",
            "é".repeat(MEMORY_WRITE_DEDUP_CANDIDATE_CONTENT_CHAR_CAP)
        );
        let prompt = render(&candidate(content));
        let expected_body = "é".repeat(MEMORY_WRITE_DEDUP_CANDIDATE_CONTENT_CHAR_CAP);

        assert!(prompt.contains(&format!(
            "  body:\n{expected_body}\n… [truncated at {MEMORY_WRITE_DEDUP_CANDIDATE_CONTENT_CHAR_CAP} characters]"
        )));
        assert!(!prompt.contains(&format!("{expected_body}Z")));
    }
}
