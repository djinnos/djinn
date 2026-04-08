use async_trait::async_trait;
use djinn_core::models::NoteDedupCandidate;

#[derive(Debug, Clone, Copy)]
pub(crate) struct PendingWriteDedup<'a> {
    pub(crate) project_path: &'a str,
    pub(crate) project_id: &'a str,
    pub(crate) title: &'a str,
    pub(crate) content: &'a str,
    pub(crate) note_type: &'a str,
    pub(crate) tags_json: &'a str,
}

#[derive(Debug, Clone)]
pub(crate) struct MemoryWriteDedupDecisionInput<'a> {
    pub(crate) project_path: &'a str,
    pub(crate) title: &'a str,
    pub(crate) content: &'a str,
    pub(crate) note_type: &'a str,
    pub(crate) candidates: &'a [NoteDedupCandidate],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MemoryWriteDedupDecision {
    CreateNew,
    ReuseExisting {
        candidate_id: String,
    },
    MergeIntoExisting {
        candidate_id: String,
        merged_title: String,
        merged_content: String,
    },
}

#[async_trait]
pub(crate) trait MemoryWriteDedupDecider: Send + Sync {
    async fn decide(
        &self,
        input: MemoryWriteDedupDecisionInput<'_>,
    ) -> Result<MemoryWriteDedupDecision, String>;
}
