#[cfg(test)]
mod tests {
    use crate::tools::memory_tools::write_dedup_prompt::parse_memory_write_dedup_decision;
    use crate::tools::memory_tools::write_dedup_types::MemoryWriteDedupDecision;

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
}
